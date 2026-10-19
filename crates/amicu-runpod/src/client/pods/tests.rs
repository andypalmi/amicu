use amicu_providers::TransportError;

use super::*;
use crate::client::tests::{KEY, Mock, client};

const POD: &str = r#"{"id":"pod1","name":"amicu-x","image":"img:1","args":"","disk":100,
    "ports":["8000/http"],"env":{"AMICU_OWNER":"own","HF_TOKEN":"leaky"},"registry":null,
    "status":"RUNNING","actions":["stop","restart","terminate"],"mounts":{},
    "gpu":{"id":"NVIDIA H100 80GB HBM3","count":2,"vcpuCount":32,"memory":250},
    "cloud":"SECURE","dataCenterId":"EU-RO-1","cudaVersion":"12.8","ssh":null,
    "template":null,"cost":5.38,"locked":false,"globalNetworking":{"enabled":false},
    "runtime":null,"createdAt":"2026-06-01T12:00:00Z","startedAt":"2026-06-01T12:02:00Z"}"#;

fn full_request() -> CreatePod {
    let mut r = CreatePod::new("amicu-x", "vllm/vllm-openai:v1", "NVIDIA H100 80GB HBM3", 2);
    r.entrypoint = vec!["vllm".into()];
    r.cmd = vec!["serve".into(), "m".into()];
    r.env = BTreeMap::from([
        ("AMICU_OWNER".to_owned(), "own".to_owned()),
        (
            "VLLM_API_KEY".to_owned(),
            "{{ RUNPOD_SECRET_k }}".to_owned(),
        ),
    ]);
    r.ports = vec!["8000/http".into()];
    r.disk_gb = Some(100);
    r.cloud = Cloud::Community;
    r.min_cuda_version = Some("13.0".into());
    r.data_center_ids = vec!["EU-RO-1".into(), "EU-SE-1".into()];
    r
}

#[tokio::test]
async fn create_pod_sends_exact_body() {
    let (c, mock) = client(Mock::default().on(HttpMethod::Post, "/pods", 201, POD));
    let pod = c.create_pod(&full_request()).await.unwrap();
    assert_eq!(pod.id, "pod1");
    assert_eq!(pod.status, PodStatus::Running);
    assert_eq!(pod.cost, Some(5.38));
    assert_eq!(pod.cloud, Some(Cloud::Secure));
    assert_eq!(pod.env.owner.as_deref(), Some("own"));
    assert!(!format!("{pod:?}").contains("leaky"));

    let seen = mock.requests();
    assert_eq!(seen[0].url, "https://api.runpod.io/v2/pods");
    assert_eq!(seen[0].bearer.as_deref(), Some(KEY));
    assert_eq!(
        seen[0].body.as_ref().unwrap(),
        &json!({
            "name": "amicu-x",
            "image": "vllm/vllm-openai:v1",
            "entrypoint": ["vllm"],
            "cmd": ["serve", "m"],
            "env": {"AMICU_OWNER": "own", "VLLM_API_KEY": "{{ RUNPOD_SECRET_k }}"},
            "ports": ["8000/http"],
            "disk": 100,
            "gpu": {"id": "NVIDIA H100 80GB HBM3", "count": 2, "minCudaVersion": "13.0"},
            "cloud": "COMMUNITY",
            "dataCenterIds": ["EU-RO-1", "EU-SE-1"],
            "startSsh": false,
            "startJupyter": false
        })
    );
}

#[test]
fn minimal_create_body_states_ports_and_disables_ssh() {
    assert_eq!(
        CreatePod::new("n", "i", "NVIDIA A40", 1).to_body(),
        json!({
            "name": "n",
            "image": "i",
            "ports": [],
            "gpu": {"id": "NVIDIA A40", "count": 1},
            "cloud": "SECURE",
            "startSsh": false,
            "startJupyter": false
        })
    );
}

#[tokio::test]
async fn create_transport_failure_is_ambiguous() {
    let (c, _) = client(Mock::default().fail(HttpMethod::Post, "/pods", TransportError::Timeout));
    assert!(matches!(
        c.create_pod(&full_request()).await.unwrap_err(),
        RunPodError::AmbiguousCreate(_)
    ));
}

#[tokio::test]
async fn create_error_detail_is_withheld_when_env_is_sent() {
    let body = r#"{"detail":"env VLLM_API_KEY={{ RUNPOD_SECRET_k }} bad"}"#;
    let (c, _) = client(Mock::default().on(HttpMethod::Post, "/pods", 400, body));
    let err = c.create_pod(&full_request()).await.unwrap_err();
    assert!(!format!("{err}{err:?}").contains("RUNPOD_SECRET"));
}

#[tokio::test]
async fn list_follows_pagination_and_skips_terminated() {
    let page1 = r#"{"pods":[{"id":"a","name":"amicu-x-2","status":"RUNNING"},
        {"id":"t","name":"amicu-x","status":"TERMINATED"}],
        "pagination":{"nextCursor":"c/1","hasNextPage":true}}"#;
    let page2 = r#"{"pods":[{"id":"b","name":"amicu-x","status":"EXITED"}],
        "pagination":{"nextCursor":null,"hasNextPage":false}}"#;
    let (c, mock) = client(
        Mock::default()
            .on(HttpMethod::Get, "/pods?cursor=c%2F1", 200, page2)
            .on(HttpMethod::Get, "/pods", 200, page1),
    );
    let found = c.find_pod_by_name("amicu-x").await.unwrap().unwrap();
    assert_eq!((found.id.as_str(), found.status), ("b", PodStatus::Exited));
    let urls: Vec<String> = mock.requests().into_iter().map(|r| r.url).collect();
    assert_eq!(
        urls,
        [
            "https://api.runpod.io/v2/pods",
            "https://api.runpod.io/v2/pods?cursor=c%2F1"
        ]
    );
}

#[tokio::test]
async fn duplicate_pod_names_are_an_error() {
    let body = r#"{"pods":[{"id":"a","name":"dup","status":"RUNNING"},
        {"id":"b","name":"dup","status":"STARTING"}],
        "pagination":{"nextCursor":null,"hasNextPage":false}}"#;
    let (c, _) = client(Mock::default().on(HttpMethod::Get, "/pods", 200, body));
    assert_eq!(
        c.find_pod_by_name("dup").await.unwrap_err(),
        RunPodError::DuplicateName {
            name: "dup".into(),
            count: 2
        }
    );
    assert!(c.find_pod_by_name("none").await.unwrap().is_none());
}

#[tokio::test]
async fn get_pod_404_and_unknown_status() {
    let (c, _) = client(
        Mock::default()
            .on(HttpMethod::Get, "/pods/gone", 404, "{}")
            .on(
                HttpMethod::Get,
                "/pods/new",
                200,
                r#"{"id":"new","name":"n","status":"HIBERNATING"}"#,
            ),
    );
    assert_eq!(
        c.get_pod("gone").await.unwrap_err(),
        RunPodError::NotFound {
            what: "pod `gone`".into()
        }
    );
    assert_eq!(c.get_pod("new").await.unwrap().status, PodStatus::Unknown);
}

#[tokio::test]
async fn update_pod_patches_only_set_fields() {
    let (c, mock) = client(Mock::default().on(HttpMethod::Patch, "/pods/pod1", 200, POD));
    let update = UpdatePod {
        image: Some("img:2".into()),
        cmd: Some(vec![]),
        env: Some(BTreeMap::from([("A".to_owned(), "1".to_owned())])),
        ..UpdatePod::default()
    };
    c.update_pod("pod1", &update).await.unwrap();
    let seen = mock.requests();
    assert_eq!(seen[0].method, HttpMethod::Patch);
    assert_eq!(seen[0].url, "https://api.runpod.io/v2/pods/pod1");
    assert_eq!(
        seen[0].body.as_ref().unwrap(),
        &json!({"image": "img:2", "cmd": [], "env": {"A": "1"}})
    );
    assert_eq!(UpdatePod::default().to_body(), json!({}));
}

#[tokio::test]
async fn update_pod_conflict_maps_to_conflict() {
    let (c, _) = client(Mock::default().on(
        HttpMethod::Patch,
        "/pods/pod1",
        409,
        r#"{"detail":"cluster pod"}"#,
    ));
    assert!(matches!(
        c.update_pod("pod1", &UpdatePod::default())
            .await
            .unwrap_err(),
        RunPodError::Conflict { .. }
    ));
}

#[tokio::test]
async fn terminate_204_and_404() {
    let (c, mock) = client(Mock::default().on(HttpMethod::Post, "/pods/pod1/action", 204, ""));
    assert_eq!(
        c.terminate_pod("pod1").await.unwrap(),
        DeleteOutcome::Deleted
    );
    let seen = mock.requests();
    assert_eq!(seen[0].url, "https://api.runpod.io/v2/pods/pod1/action");
    assert_eq!(
        seen[0].body.as_ref().unwrap(),
        &json!({"action": "terminate"})
    );

    let (c, _) = client(Mock::default().on(HttpMethod::Post, "/pods/pod1/action", 404, "{}"));
    assert_eq!(
        c.terminate_pod("pod1").await.unwrap(),
        DeleteOutcome::AlreadyGone
    );
    let (c, _) = client(Mock::default().on(HttpMethod::Post, "/pods/pod1/action", 409, "{}"));
    assert!(matches!(
        c.terminate_pod("pod1").await.unwrap_err(),
        RunPodError::Conflict { .. }
    ));
}

#[tokio::test]
async fn actions_return_the_pod_except_terminate() {
    let (c, mock) = client(Mock::default().on(HttpMethod::Post, "/pods/pod1/action", 200, POD));
    let pod = c
        .pod_action("pod1", PodAction::Start)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pod.id, "pod1");
    assert_eq!(
        mock.requests()[0].body.as_ref().unwrap(),
        &json!({"action": "start"})
    );

    let (c, _) = client(Mock::default().on(HttpMethod::Post, "/pods/pod1/action", 204, ""));
    assert!(
        c.pod_action("pod1", PodAction::Terminate)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delete_pod_is_idempotent() {
    let (c, _) = client(Mock::default().on(HttpMethod::Delete, "/pods/pod1", 404, "{}"));
    assert_eq!(
        c.delete_pod("pod1").await.unwrap(),
        DeleteOutcome::AlreadyGone
    );
    let (c, _) = client(Mock::default().on(HttpMethod::Delete, "/pods/pod1", 204, ""));
    assert_eq!(c.delete_pod("pod1").await.unwrap(), DeleteOutcome::Deleted);
}

#[tokio::test]
async fn unsafe_pod_ids_are_rejected_before_any_request() {
    let (c, mock) = client(Mock::default());
    for id in ["", "../pods", "a/b", "a?x=1"] {
        assert!(matches!(
            c.get_pod(id).await,
            Err(RunPodError::InvalidId(_))
        ));
        assert!(matches!(
            c.terminate_pod(id).await,
            Err(RunPodError::InvalidId(_))
        ));
        assert!(matches!(
            c.update_pod(id, &UpdatePod::default()).await,
            Err(RunPodError::InvalidId(_))
        ));
    }
    assert!(mock.requests().is_empty());
}

#[test]
fn debug_never_prints_env_values() {
    let update = UpdatePod {
        env: Some(BTreeMap::from([(
            "K".to_owned(),
            "update-secret".to_owned(),
        )])),
        ..UpdatePod::default()
    };
    let mut create = full_request();
    create.env.insert("T".into(), "create-secret".into());
    let dump = format!("{create:?} {update:?}");
    assert!(!dump.contains("update-secret") && !dump.contains("create-secret"));
    assert!(dump.contains("VLLM_API_KEY"));
}

#[test]
fn proxy_url_shape() {
    assert_eq!(
        pod_proxy_url("abc123", 8000),
        "https://abc123-8000.proxy.runpod.net"
    );
    let pod: Pod = serde_json::from_str(
        r#"{"id":"p","name":"n","status":"RUNNING","ports":["22/tcp","8080/http"]}"#,
    )
    .unwrap();
    assert_eq!(pod.http_port(), Some(8080));
    let vllm: Pod = serde_json::from_str(
        r#"{"id":"p","name":"n","status":"RUNNING","ports":["8000/http","22/tcp"]}"#,
    )
    .unwrap();
    assert_eq!(vllm.http_port(), Some(8000), "a vLLM pod serves on 8000");
}

#[test]
fn pod_env_compares_by_digest_and_unknown_cloud_decodes() {
    let pod: Pod = serde_json::from_str(
        r#"{"id":"p","name":"n","status":"RUNNING","cloud":"ORBITAL",
        "env":{"AMICU_OWNER":"own","A":"{{ RUNPOD_SECRET_a }}"}}"#,
    )
    .unwrap();
    assert_eq!(pod.cloud, Some(Cloud::Unknown));
    let mut want = BTreeMap::from([
        ("AMICU_OWNER".to_owned(), "own".to_owned()),
        ("A".to_owned(), "{{ RUNPOD_SECRET_a }}".to_owned()),
    ]);
    assert!(pod.env.matches(&want));
    want.insert("A".to_owned(), "{{ RUNPOD_SECRET_b }}".to_owned());
    assert!(!pod.env.matches(&want));
    assert!(!PodEnv::default().matches(&BTreeMap::new()));
    assert!(!format!("{pod:?}").contains("RUNPOD_SECRET"));
}
