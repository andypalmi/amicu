use std::sync::Arc;
use std::time::Duration;

use amicu_providers::{HttpMethod, HttpResponse, HttpTransport, TransportError};
use serde_json::json;

use super::*;
use crate::catalog::GcpCatalog;
use crate::client::tests::{Mock, TOKEN, client};
use crate::error::GcpError;
use crate::labels::owner_marker;
use crate::operations::PollTiming;

const OP: &str = "projects/my-proj/locations/europe-west4/operations/op1";

fn spec(region: &str, gpu: &str) -> ServiceSpec {
    let mut s = ServiceSpec::new(
        "my-proj",
        region,
        "chat",
        gpu,
        "Qwen/Qwen3-8B",
        SecretRef::new("amicu-key", "3"),
        "run-sa@my-proj.iam.gserviceaccount.com",
    );
    s.hf_token = Some(SecretRef::new("amicu-hf", "latest"));
    s.max_instances = 2;
    s
}

fn svc_url(s: &ServiceSpec) -> String {
    format!("/locations/{}/services/{}", s.region, s.service_name())
}

fn create_suffix(s: &ServiceSpec) -> String {
    format!(
        "/locations/{}/services?serviceId={}",
        s.region,
        s.service_name()
    )
}

fn service_json(s: &ServiceSpec, owner: &str, ready: bool) -> String {
    let state = if ready {
        "CONDITION_SUCCEEDED"
    } else {
        "CONDITION_RECONCILING"
    };
    let mut v = json!({
        "labels": {"amicu-owner": owner},
        "terminalCondition": {"type": "Ready", "state": state},
        "uri": format!("https://{}-123.a.run.app", s.service_name()),
    });
    if ready {
        v["latestReadyRevision"] = json!("rev-1");
    }
    v.to_string()
}

fn fast() -> PollTiming {
    PollTiming {
        interval: Duration::from_secs(1),
        timeout: Duration::from_secs(30),
    }
}

fn lifecycle(mock: Mock) -> (CloudRunLifecycle, Arc<Mock>) {
    let (c, mock) = client(mock);
    (
        CloudRunLifecycle::new(Arc::new(c)).with_timing(fast()),
        mock,
    )
}

fn owner(s: &ServiceSpec) -> String {
    owner_marker(None, &s.project, &s.deployment_id)
}

#[test]
fn l4_body_in_europe_west4() {
    let s = spec("europe-west4", "l4");
    let b = s.create_body(&GcpCatalog::new()).unwrap();
    let t = &b["template"];
    let c = &t["containers"][0];
    assert_eq!(b["ingress"], "INGRESS_TRAFFIC_ALL");
    assert_eq!(b["invokerIamDisabled"], true);
    assert_eq!(b["labels"]["amicu-owner"], owner(&s));
    assert_eq!(t["nodeSelector"]["accelerator"], "nvidia-l4");
    assert_eq!(t["gpuZonalRedundancyDisabled"], true);
    assert_eq!(t["scaling"]["minInstanceCount"], 0);
    assert_eq!(t["scaling"]["maxInstanceCount"], 2);
    assert_eq!(t["maxInstanceRequestConcurrency"], 8);
    assert_eq!(t["timeout"], "3600s");
    assert_eq!(t["serviceAccount"], s.service_account);
    assert_eq!(c["image"], DEFAULT_VLLM_IMAGE);
    assert_eq!(c["resources"]["limits"]["nvidia.com/gpu"], "1");
    assert_eq!(c["resources"]["limits"]["cpu"], "4");
    assert_eq!(c["resources"]["limits"]["memory"], "16Gi");
    assert_eq!(c["resources"]["cpuIdle"], false);
    assert_eq!(c["ports"][0]["containerPort"], 8000);
    assert_eq!(c["startupProbe"]["httpGet"]["path"], "/health");
    assert_eq!(c["startupProbe"]["periodSeconds"], 10);
    assert_eq!(c["startupProbe"]["failureThreshold"], 180);
    let args: Vec<&str> = c["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(
        args[..4],
        [
            "--model",
            "Qwen/Qwen3-8B",
            "--served-model-name",
            "Qwen/Qwen3-8B"
        ]
    );
    assert!(args.contains(&"--max-model-len") && args.contains(&"8000"));
    assert!(!args.iter().any(|a| a.contains("api-key")));
    assert_eq!(
        c["env"],
        json!([
            {"name": "HF_TOKEN", "valueSource": {"secretKeyRef": {"secret": "amicu-hf", "version": "latest"}}},
            {"name": "VLLM_API_KEY", "valueSource": {"secretKeyRef": {"secret": "amicu-key", "version": "3"}}},
        ])
    );
}

#[test]
fn rtx_pro_6000_body_uses_the_larger_minimums() {
    let mut s = spec("europe-west4", "nvidia-rtx-pro-6000");
    s.zonal_redundancy = true;
    s.access = ServiceAccess::IamProtected;
    let b = s.create_body(&GcpCatalog::new()).unwrap();
    let t = &b["template"];
    let limits = &t["containers"][0]["resources"]["limits"];
    assert_eq!(t["nodeSelector"]["accelerator"], "nvidia-rtx-pro-6000");
    assert_eq!(limits["cpu"], "20");
    assert_eq!(limits["memory"], "80Gi");
    assert_eq!(t["gpuZonalRedundancyDisabled"], false);
    assert_eq!(b["invokerIamDisabled"], false);
}

#[test]
fn service_name_fits_cloud_run() {
    let n = service_name(&"p".repeat(30), &"d".repeat(80));
    assert!(n.len() <= 49, "{n}");
    assert!(crate::validate::validate_resource_name(&n).is_ok());
    assert_eq!(n, service_name(&"p".repeat(30), &"d".repeat(80)));
    assert_ne!(
        n,
        service_name(&"p".repeat(30), &format!("{}x", "d".repeat(79)))
    );
}

#[test]
fn region_gpu_and_input_validation() {
    let cat = GcpCatalog::new();
    // RTX PRO 6000 is not on Cloud Run in europe-west1; L4 is.
    assert!(
        spec("europe-west1", "nvidia-rtx-pro-6000")
            .validate(&cat)
            .is_err()
    );
    assert!(spec("europe-west1", "l4").validate(&cat).is_ok());
    assert!(spec("us-west1", "l4").validate(&cat).is_err());
    // Invitation-only region.
    assert!(spec("asia-south1", "l4").validate(&cat).is_err());
    // Not a Cloud Run GPU.
    assert!(spec("europe-west4", "h100").validate(&cat).is_err());
    let mut s = spec("europe-west4", "l4");
    s.cpu = Some(2);
    assert!(s.validate(&cat).is_err());
    let mut s = spec("europe-west4", "l4");
    s.extra_args = vec!["--api-key=abc".into()];
    assert!(s.validate(&cat).is_err());
    let mut s = spec("europe-west4", "l4");
    s.api_key = SecretRef::new("bad name", "1");
    assert!(s.validate(&cat).is_err());
    let mut s = spec("europe-west4", "l4");
    s.request_timeout_secs = 4000;
    assert!(s.validate(&cat).is_err());
}

#[tokio::test]
async fn prepare_returns_the_record_without_calling_google() {
    let s = spec("europe-west4", "l4");
    let (lc, mock) = lifecycle(Mock::default());
    let r = lc.prepare(&s).unwrap();
    assert_eq!(r.project, "my-proj");
    assert_eq!(r.service, s.service_name());
    assert_eq!(r.owner, owner(&s));
    assert!(mock.requests().is_empty());
    assert!(lc.prepare(&spec("us-west1", "l4")).is_err());
}

#[tokio::test(start_paused = true)]
async fn create_waits_for_the_operation_and_returns_ready_status() {
    let s = spec("europe-west4", "l4");
    let mock = Mock::default()
        .on(
            HttpMethod::Post,
            &create_suffix(&s),
            200,
            &format!(r#"{{"name":"{OP}"}}"#),
        )
        .on(HttpMethod::Get, "/operations/op1", 200, r#"{"done":false}"#)
        .on(HttpMethod::Get, "/operations/op1", 200, r#"{"done":true}"#)
        .on(
            HttpMethod::Get,
            &svc_url(&s),
            200,
            &service_json(&s, &owner(&s), true),
        );
    let (lc, mock) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    let st = lc.create(&rec, &s).await.unwrap();
    assert!(st.is_ready());
    assert_eq!(st.latest_ready_revision.as_deref(), Some("rev-1"));
    assert!(st.uri.unwrap().ends_with("a.run.app"));
    let seen = mock.requests();
    assert_eq!(seen[0].method, HttpMethod::Post);
    assert_eq!(seen[0].bearer.as_deref(), Some(TOKEN));
    assert_eq!(
        seen[0].body.as_ref().unwrap()["labels"]["amicu-owner"],
        owner(&s)
    );
}

#[tokio::test(start_paused = true)]
async fn lost_create_response_adopts_the_owned_service() {
    let s = spec("europe-west4", "l4");
    let mock = Mock::default()
        .then(
            HttpMethod::Post,
            &create_suffix(&s),
            Err(TransportError::Timeout),
        )
        .on(
            HttpMethod::Get,
            &svc_url(&s),
            200,
            &service_json(&s, &owner(&s), false),
        );
    let (lc, _) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    let st = lc.create(&rec, &s).await.unwrap();
    assert!(!st.is_ready());
    assert_eq!(st.state, ReadyState::Pending);
}

#[tokio::test(start_paused = true)]
async fn lost_create_with_no_service_keeps_the_transport_error() {
    let s = spec("europe-west4", "l4");
    let mock = Mock::default()
        .then(
            HttpMethod::Post,
            &create_suffix(&s),
            Err(TransportError::Timeout),
        )
        .on(
            HttpMethod::Get,
            &svc_url(&s),
            404,
            r#"{"error":{"status":"NOT_FOUND"}}"#,
        );
    let (lc, _) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    assert!(matches!(
        lc.create(&rec, &s).await,
        Err(GcpError::Transport(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn conflict_on_a_foreign_service_is_an_error() {
    let s = spec("europe-west4", "l4");
    let mock = Mock::default()
        .on(
            HttpMethod::Post,
            &create_suffix(&s),
            409,
            r#"{"error":{"status":"ALREADY_EXISTS","message":"exists"}}"#,
        )
        .on(
            HttpMethod::Get,
            &svc_url(&s),
            200,
            &service_json(&s, "someone-else", true),
        );
    let (lc, mock) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    assert!(matches!(
        lc.create(&rec, &s).await,
        Err(GcpError::NotOwned { .. })
    ));
    assert!(
        mock.requests()
            .iter()
            .all(|r| r.method != HttpMethod::Delete)
    );
}

#[tokio::test(start_paused = true)]
async fn conflict_on_our_own_service_is_adopted() {
    let s = spec("europe-west4", "l4");
    let mock = Mock::default()
        .on(
            HttpMethod::Post,
            &create_suffix(&s),
            409,
            r#"{"error":{"status":"ALREADY_EXISTS"}}"#,
        )
        .on(
            HttpMethod::Get,
            &svc_url(&s),
            200,
            &service_json(&s, &owner(&s), true),
        );
    let (lc, _) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    assert!(lc.create(&rec, &s).await.unwrap().is_ready());
}

#[tokio::test(start_paused = true)]
async fn quota_errors_map_with_a_hint() {
    let s = spec("europe-west4", "l4");
    // Immediate 429.
    let mock = Mock::default().on(
        HttpMethod::Post,
        &create_suffix(&s),
        429,
        r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded"}}"#,
    );
    let (lc, _) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    match lc.create(&rec, &s).await {
        Err(GcpError::QuotaExceeded { message }) => assert!(message.contains("Quotas page")),
        other => panic!("{other:?}"),
    }
    // Operation that fails with gRPC code 8.
    let mock = Mock::default()
        .on(
            HttpMethod::Post,
            &create_suffix(&s),
            200,
            &format!(r#"{{"name":"{OP}"}}"#),
        )
        .on(
            HttpMethod::Get,
            "/operations/op1",
            200,
            r#"{"done":true,"error":{"code":8,"message":"no GPU quota"}}"#,
        );
    let (lc, _) = lifecycle(mock);
    assert!(lc.create(&rec, &s).await.unwrap_err().is_quota());
}

#[tokio::test]
async fn status_reads_conditions_and_revision() {
    let s = spec("europe-west4", "l4");
    let body = json!({
        "labels": {"amicu-owner": owner(&s)},
        "conditions": [
            {"type": "RoutesReady", "state": "CONDITION_SUCCEEDED"},
            {"type": "Ready", "state": "CONDITION_FAILED", "message": "container failed"},
        ],
        "latestReadyRevision": "rev-0",
    })
    .to_string();
    let (lc, _) = lifecycle(Mock::default().on(HttpMethod::Get, &svc_url(&s), 200, &body));
    let rec = lc.prepare(&s).unwrap();
    let st = lc.status(&rec).await.unwrap().unwrap();
    assert_eq!(st.state, ReadyState::Failed("container failed".into()));
    assert!(!st.is_ready());
    assert_eq!(st.uri, None);
    assert!(matches!(lc.url(&rec).await, Ok(None)));
}

#[tokio::test(start_paused = true)]
async fn delete_refuses_an_unowned_service() {
    let s = spec("europe-west4", "l4");
    let mock = Mock::default().on(
        HttpMethod::Get,
        &svc_url(&s),
        200,
        &service_json(&s, "other", true),
    );
    let (lc, mock) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    assert!(matches!(
        lc.delete(&rec).await,
        Err(GcpError::NotOwned { .. })
    ));
    assert!(mock.requests().iter().all(|r| r.method == HttpMethod::Get));
}

#[tokio::test(start_paused = true)]
async fn delete_waits_then_confirms_404() {
    let s = spec("europe-west4", "l4");
    let mock = Mock::default()
        .on(
            HttpMethod::Get,
            &svc_url(&s),
            200,
            &service_json(&s, &owner(&s), true),
        )
        .on(
            HttpMethod::Get,
            &svc_url(&s),
            404,
            r#"{"error":{"status":"NOT_FOUND"}}"#,
        )
        .on(
            HttpMethod::Delete,
            &svc_url(&s),
            200,
            &format!(r#"{{"name":"{OP}"}}"#),
        )
        .on(HttpMethod::Get, "/operations/op1", 200, r#"{"done":true}"#);
    let (lc, _) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    assert_eq!(lc.delete(&rec).await.unwrap(), DeleteOutcome::Deleted);
}

#[tokio::test(start_paused = true)]
async fn delete_fails_if_the_service_is_still_there() {
    let s = spec("europe-west4", "l4");
    let mock = Mock::default()
        .on(
            HttpMethod::Get,
            &svc_url(&s),
            200,
            &service_json(&s, &owner(&s), true),
        )
        .on(
            HttpMethod::Delete,
            &svc_url(&s),
            200,
            &format!(r#"{{"name":"{OP}"}}"#),
        )
        .on(HttpMethod::Get, "/operations/op1", 200, r#"{"done":true}"#);
    let (lc, _) = lifecycle(mock);
    let rec = lc.prepare(&s).unwrap();
    assert!(matches!(
        lc.delete(&rec).await,
        Err(GcpError::OperationFailed { code, .. }) if code == "STILL_EXISTS"
    ));
}

#[tokio::test]
async fn delete_of_a_missing_service_is_already_gone() {
    let s = spec("europe-west4", "l4");
    let (lc, _) = lifecycle(Mock::default().on(HttpMethod::Get, &svc_url(&s), 404, "{}"));
    let rec = lc.prepare(&s).unwrap();
    assert_eq!(lc.delete(&rec).await.unwrap(), DeleteOutcome::AlreadyGone);
}

#[tokio::test(start_paused = true)]
async fn health_polls_through_cold_start_with_the_bearer_key() {
    let mock = Arc::new(
        Mock::default()
            .on(HttpMethod::Get, "/health", 503, "")
            .then(HttpMethod::Get, "/health", Err(TransportError::Timeout))
            .then(HttpMethod::Get, "/health", Ok(HttpResponse::new(200, "")))
            .then(HttpMethod::Get, "/health", Ok(HttpResponse::new(200, ""))),
    );
    let t: Arc<dyn HttpTransport> = mock.clone();
    wait_ready(&t, "https://x.run.app/", "k", HealthTiming::DEFAULT)
        .await
        .unwrap();
    let seen = mock.requests();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].url, "https://x.run.app/health");
    assert_eq!(seen[0].bearer.as_deref(), Some("k"));
}

#[tokio::test(start_paused = true)]
async fn health_stops_on_403_and_times_out() {
    let t: Arc<dyn HttpTransport> =
        Arc::new(Mock::default().on(HttpMethod::Get, "/health", 403, ""));
    assert!(matches!(
        wait_ready(&t, "https://x.run.app", "k", HealthTiming::DEFAULT).await,
        Err(GcpError::PermissionDenied { .. })
    ));
    let t: Arc<dyn HttpTransport> =
        Arc::new(Mock::default().on(HttpMethod::Get, "/health", 503, ""));
    let timing = HealthTiming {
        timeout: Duration::from_secs(60),
        ..HealthTiming::DEFAULT
    };
    assert!(matches!(
        wait_ready(&t, "https://x.run.app", "k", timing).await,
        Err(GcpError::OperationTimeout { .. })
    ));
}
