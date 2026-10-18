use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

const KEY: &str = "sk-test-endpoint-key-0123456789";
const FAST: CheckTiming = CheckTiming {
    reject_timeout: Duration::from_millis(300),
    accept_timeout: Duration::from_millis(300),
    interval: Duration::from_millis(10),
};

/// A loopback HTTP server answering every request with the status the
/// handler picks from the lower-cased raw request text.
async fn serve<H>(handler: H) -> String
where
    H: Fn(&str) -> &'static str + Send + Sync + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let status = handler(&request);
                let head =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = sock.write_all(head.as_bytes()).await;
            });
        }
    });
    url
}

fn has_key(request: &str) -> bool {
    request.contains(&format!("authorization: bearer {}", KEY.to_lowercase()))
}

fn check<'a>(
    endpoint: &'a str,
    key: &'a SecretValue,
    gateway: &'a [(String, SecretValue)],
) -> EndpointCheck<'a> {
    EndpointCheck {
        endpoint,
        path: PROBE_PATH,
        api_key: key,
        gateway_headers: gateway,
        timing: FAST,
    }
}

fn key() -> SecretValue {
    SecretValue::new(KEY.to_owned())
}

fn gateway() -> [(String, SecretValue); 1] {
    [("Modal-Key".to_owned(), SecretValue::new("wk-id".to_owned()))]
}

#[tokio::test]
async fn a_protected_endpoint_passes() {
    let url = serve(|r| {
        if has_key(r) {
            "200 OK"
        } else {
            "401 Unauthorized"
        }
    })
    .await;
    verify_endpoint(&check(&url, &key(), &[])).await.unwrap();
}

#[tokio::test]
async fn an_open_endpoint_is_an_exposure() {
    let url = serve(|_| "200 OK").await;
    let err = verify_endpoint(&check(&url, &key(), &[]))
        .await
        .unwrap_err();
    assert!(err.detail().contains("unauthenticated request"), "{err}");
    assert!(err.detail().contains("200"));
    assert!(!err.to_string().contains(KEY));
}

#[tokio::test]
async fn a_403_counts_as_a_refusal() {
    let url = serve(|r| {
        if has_key(r) {
            "200 OK"
        } else {
            "403 Forbidden"
        }
    })
    .await;
    verify_endpoint(&check(&url, &key(), &[])).await.unwrap();
}

#[tokio::test]
async fn a_rejected_key_fails_without_echoing_it() {
    let url = serve(|_| "401 Unauthorized").await;
    let err = verify_endpoint(&check(&url, &key(), &[]))
        .await
        .unwrap_err();
    assert!(err.detail().contains("rejected the configured API key"));
    assert!(!err.to_string().contains(KEY));
    assert!(!format!("{err:?}").contains(KEY));
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_closed() {
    let err = verify_endpoint(&check("http://127.0.0.1:1", &key(), &[]))
        .await
        .unwrap_err();
    assert!(err.detail().contains("could not verify"));
}

#[tokio::test]
async fn a_loading_runtime_is_waited_for() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let url = serve(move |r| {
        if !has_key(r) {
            return "401 Unauthorized";
        }
        // The first authenticated answers are 503 (model loading).
        if seen.fetch_add(1, Ordering::SeqCst) < 2 {
            "503 Service Unavailable"
        } else {
            "200 OK"
        }
    })
    .await;
    let k = key();
    let mut c = check(&url, &k, &[]);
    c.timing.accept_timeout = Duration::from_secs(5);
    verify_endpoint(&c).await.unwrap();
    assert!(calls.load(Ordering::SeqCst) >= 3);
}

#[tokio::test]
async fn gateway_headers_ride_with_the_authenticated_request() {
    // The gateway demands its header on everything; the runtime demands the
    // key. Anonymous and gateway-only requests are refused, key + gateway ok.
    let url = serve(|r| {
        if r.contains("modal-key: wk-id") && has_key(r) {
            "200 OK"
        } else {
            "401 Unauthorized"
        }
    })
    .await;
    verify_endpoint(&check(&url, &key(), &gateway()))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_runtime_that_ignores_the_key_behind_a_gateway_is_flagged() {
    // The gateway refuses anonymous requests, but the runtime behind it
    // accepts anything the gateway lets through.
    let url = serve(|r| {
        if r.contains("modal-key: wk-id") {
            "200 OK"
        } else {
            "401 Unauthorized"
        }
    })
    .await;
    let err = verify_endpoint(&check(&url, &key(), &gateway()))
        .await
        .unwrap_err();
    assert!(err.detail().contains("not enforcing the API key"), "{err}");
    assert!(!err.to_string().contains("wk-id"));
}

#[tokio::test]
async fn the_single_probe_flags_open_and_passes_refusals() {
    let t = Duration::from_millis(100);
    let open = serve(|_| "200 OK").await;
    let err = ensure_rejects_unauthenticated(&open, "/health", t, t)
        .await
        .unwrap_err();
    assert!(err.detail().contains("unauthenticated"));
    let closed = serve(|_| "401 Unauthorized").await;
    ensure_rejects_unauthenticated(&closed, "/health", t, t)
        .await
        .unwrap();
    let down = ensure_rejects_unauthenticated("http://127.0.0.1:1", "/health", t, t)
        .await
        .unwrap_err();
    assert!(down.detail().contains("could not verify"));
}

#[test]
fn key_values_that_change_semantics_are_refused_without_echoing() {
    let ok = SecretValue::new(KEY.to_owned());
    assert_eq!(validate_api_key_value(&ok).unwrap(), KEY);
    for bad in [
        "",
        "a,b",
        "has space",
        "tab\there",
        "new\nline",
        "caf\u{e9}",
    ] {
        let why = validate_api_key_value(&SecretValue::new(bad.to_owned())).unwrap_err();
        assert!(bad.is_empty() || !why.contains(bad), "{why}");
    }
    assert!(validate_api_key_value(&SecretValue::new(vec![0xff, 0xfe])).is_err());
}

#[test]
fn debug_of_the_check_redacts_secrets() {
    let k = key();
    let gw = [(
        "Modal-Secret".to_owned(),
        SecretValue::new("ws-hidden".to_owned()),
    )];
    let text = format!("{:?}", check("http://x", &k, &gw));
    assert!(!text.contains(KEY) && !text.contains("ws-hidden"), "{text}");
    assert!(text.contains("Modal-Secret"));
}

#[tokio::test]
async fn teardown_report_says_whether_the_stop_worked() {
    let ok = teardown_report("open", "ap-1", "hint", || async { Ok(()) }).await;
    assert_eq!(ok, "open; the deployment (ap-1) was stopped");
    let bad = teardown_report("open", "ap-1", "modal app stop ap-1", || async {
        Err("rpc down".to_owned())
    })
    .await;
    assert!(bad.contains("teardown FAILED (rpc down)"));
    assert!(bad.contains("modal app stop ap-1"));
}

/// Runs `rollback_or_teardown` with scripted results, returning the report,
/// whether the previous version survived, and which steps ran.
async fn scripted_update(
    previous: Option<&str>,
    rollback: Result<(), &'static str>,
    reverify: Result<(), &'static str>,
    stop: Result<(), &'static str>,
) -> (String, bool, Vec<&'static str>) {
    let calls = std::sync::Mutex::new(Vec::new());
    let (report, kept) = rollback_or_teardown(
        "open",
        "ap-1",
        "modal app stop ap-1",
        previous,
        || async {
            calls.lock().unwrap().push("rollback");
            rollback.map_err(str::to_owned)
        },
        || async {
            calls.lock().unwrap().push("reverify");
            reverify.map_err(str::to_owned)
        },
        || async {
            calls.lock().unwrap().push("stop");
            stop.map_err(str::to_owned)
        },
    )
    .await;
    (report, kept, calls.into_inner().unwrap())
}

#[tokio::test]
async fn a_failed_update_rolls_back_only_the_new_version() {
    let (report, kept, calls) = scripted_update(Some("version 3"), Ok(()), Ok(()), Ok(())).await;
    assert!(kept, "{report}");
    assert_eq!(calls, ["rollback", "reverify"], "the app is never stopped");
    assert!(report.contains("rolled back to version 3"));
    assert!(report.contains("still serving"));
}

#[tokio::test]
async fn a_rollback_that_fails_or_does_not_close_the_endpoint_stops_everything() {
    let (report, kept, calls) =
        scripted_update(Some("version 3"), Err("rpc down"), Ok(()), Ok(())).await;
    assert!(!kept);
    assert_eq!(calls, ["rollback", "stop"]);
    assert!(report.contains("rolling back to version 3 failed (rpc down)"));
    assert!(report.contains("was stopped"));

    let (report, kept, calls) =
        scripted_update(Some("version 3"), Ok(()), Err("still open"), Ok(())).await;
    assert!(!kept);
    assert_eq!(calls, ["rollback", "reverify", "stop"]);
    assert!(report.contains("still failed its check (still open)"));

    let (report, kept, _) =
        scripted_update(Some("version 3"), Err("x"), Ok(()), Err("stop down")).await;
    assert!(!kept);
    assert!(report.contains("teardown FAILED (stop down)"));
}

#[tokio::test]
async fn a_fresh_deployment_has_nothing_to_roll_back_to() {
    let (report, kept, calls) = scripted_update(None, Ok(()), Ok(()), Ok(())).await;
    assert!(!kept);
    assert_eq!(calls, ["stop"]);
    assert_eq!(report, "open; the deployment (ap-1) was stopped");
}

#[test]
fn exposure_round_trips_through_json() {
    for e in [
        EndpointExposure::Loopback,
        EndpointExposure::Public {
            gateway: GatewayAuth::ModalProxyToken,
        },
        EndpointExposure::Public {
            gateway: GatewayAuth::None,
        },
        EndpointExposure::Public {
            gateway: GatewayAuth::RunPodBearer,
        },
    ] {
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<EndpointExposure>(&json).unwrap(), e);
    }
}

#[test]
fn runpod_bearer_serializes_stably_and_needs_no_extra_headers() {
    // Recorded in state: the wire name must never drift.
    assert_eq!(
        serde_json::to_string(&GatewayAuth::RunPodBearer).unwrap(),
        "\"runpod_bearer\""
    );
    assert!(!GatewayAuth::RunPodBearer.needs_extra_headers());
    assert!(!GatewayAuth::None.needs_extra_headers());
    assert!(GatewayAuth::ModalProxyToken.needs_extra_headers());
}

/// A RunPod-style gateway: one `Authorization: Bearer` header is checked by
/// the gateway and forwarded to the runtime, so the shared check runs with no
/// gateway headers: anonymous is refused, the key works.
#[tokio::test]
async fn a_runpod_bearer_endpoint_passes_with_no_gateway_headers() {
    let url = serve(|r| {
        if has_key(r) {
            "200 OK"
        } else {
            "401 Unauthorized"
        }
    })
    .await;
    verify_endpoint(&check(&url, &key(), &[])).await.unwrap();

    // A gateway that lets anonymous requests through is still an exposure.
    let open = serve(|_| "200 OK").await;
    let err = verify_endpoint(&check(&open, &key(), &[]))
        .await
        .unwrap_err();
    assert!(err.detail().contains("unauthenticated request"), "{err}");
    assert!(!err.to_string().contains(KEY));
}
