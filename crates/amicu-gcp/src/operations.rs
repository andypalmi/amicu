//! Long-running-operation polling: Compute Engine zone operations
//! (`zoneOperations.wait`) and Cloud Run v2 operations (`GET` until `done`).
//!
//! Both loops stop at [`PollTiming::timeout`] with
//! [`GcpError::OperationTimeout`]: the operation may still complete, so a
//! caller confirms the resource's state before retrying. Transient errors
//! (a timed-out `wait` call, a `5xx`, a rate limit) are retried until then;
//! anything else ends the wait.

use std::time::Duration;

use amicu_providers::HttpMethod;
use serde_json::Value;
use tokio::time::Instant;

use crate::client::{COMPUTE_BASE_URL, GcpClient, RUN_BASE_URL};
use crate::error::GcpError;
use crate::validate::{validate_project_id, validate_resource_name, validate_zone};

/// How long to wait for an operation, and the pause between polls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollTiming {
    /// Pause between two polls.
    pub interval: Duration,
    /// Give up after this long.
    pub timeout: Duration,
}

impl Default for PollTiming {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            timeout: Duration::from_mins(10),
        }
    }
}

/// Waits for the Compute Engine zone operation `operation` (the `name` of the
/// operation a create/delete call returned) to be `DONE`, and returns the final
/// operation.
///
/// Each poll is a `zoneOperations.wait` call, which Google holds open for up to
/// two minutes, so the loop re-issues it until the operation is done.
///
/// # Errors
/// [`GcpError::OperationFailed`] when the operation finished with an error,
/// [`GcpError::OperationTimeout`] at the deadline, or the first non-transient
/// error from the API.
pub async fn wait_compute_zone_operation(
    client: &GcpClient,
    project: &str,
    zone: &str,
    operation: &str,
    timing: PollTiming,
) -> Result<Value, GcpError> {
    validate_project_id(project)?;
    validate_zone(zone)?;
    validate_resource_name(operation)?;
    let url =
        format!("{COMPUTE_BASE_URL}/projects/{project}/zones/{zone}/operations/{operation}/wait");
    poll(client, HttpMethod::Post, &url, operation, timing, |op| {
        if op["status"] != "DONE" {
            return None;
        }
        Some(
            match op["error"]["errors"].as_array().and_then(|e| e.first()) {
                Some(first) => Err((
                    first["code"].as_str().unwrap_or("UNKNOWN").to_owned(),
                    first["message"].as_str().unwrap_or("no message").to_owned(),
                )),
                None => Ok(()),
            },
        )
    })
    .await
}

/// Waits for the Cloud Run v2 operation `operation` (its full `name`,
/// `projects/<p>/locations/<l>/operations/<id>`) to be `done`, and returns the
/// final operation (the created resource is in its `response`).
///
/// # Errors
/// As [`wait_compute_zone_operation`].
pub async fn wait_run_operation(
    client: &GcpClient,
    operation: &str,
    timing: PollTiming,
) -> Result<Value, GcpError> {
    validate_run_operation_name(operation)?;
    let url = format!("{RUN_BASE_URL}/{operation}");
    poll(client, HttpMethod::Get, &url, operation, timing, |op| {
        if op["done"] != true {
            return None;
        }
        let err = &op["error"];
        Some(if err.is_null() {
            Ok(())
        } else {
            Err((
                err["code"].to_string(),
                err["message"].as_str().unwrap_or("no message").to_owned(),
            ))
        })
    })
    .await
}

/// `projects/<p>/locations/<l>/operations/<id>`, each segment a plain token so
/// the name cannot break out of the URL path.
fn validate_run_operation_name(name: &str) -> Result<(), GcpError> {
    let parts: Vec<&str> = name.split('/').collect();
    let token = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    };
    let ok = parts.len() == 6
        && parts[0] == "projects"
        && parts[2] == "locations"
        && parts[4] == "operations"
        && [parts[1], parts[3], parts[5]].into_iter().all(token);
    if ok {
        Ok(())
    } else {
        Err(GcpError::Invalid {
            kind: "operation name",
            value: name.to_owned(),
            reason: "expected `projects/<p>/locations/<l>/operations/<id>`",
        })
    }
}

/// Issues `method url` until `finished` reports a result: `None` while the
/// operation is still running, `Some(Err((code, message)))` when it failed.
async fn poll(
    client: &GcpClient,
    method: HttpMethod,
    url: &str,
    operation: &str,
    timing: PollTiming,
    finished: impl Fn(&Value) -> Option<Result<(), (String, String)>>,
) -> Result<Value, GcpError> {
    let deadline = Instant::now() + timing.timeout;
    loop {
        // Bound each request by what is left of the deadline so a slow
        // response cannot overshoot the total timeout.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(response) = tokio::time::timeout(remaining, client.request(method, url, None)).await
        else {
            return Err(GcpError::OperationTimeout {
                operation: operation.to_owned(),
            });
        };
        match response {
            Ok(op) => match finished(&op) {
                Some(Ok(())) => return Ok(op),
                Some(Err((code, message))) => {
                    return Err(GcpError::OperationFailed {
                        operation: operation.to_owned(),
                        code,
                        message,
                    });
                }
                None => {}
            },
            Err(e) if e.is_retryable() => {}
            Err(e) => return Err(e),
        }
        if Instant::now() + timing.interval >= deadline {
            return Err(GcpError::OperationTimeout {
                operation: operation.to_owned(),
            });
        }
        tokio::time::sleep(timing.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use amicu_providers::{HttpResponse, TransportError};

    use super::*;
    use crate::client::tests::{Mock, client};

    const WAIT: &str = "/projects/my-proj/zones/us-central1-a/operations/operation-1/wait";
    const RUN_OP: &str = "projects/my-proj/locations/us-central1/operations/abc-123";

    fn fast() -> PollTiming {
        PollTiming {
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(30),
        }
    }

    async fn compute(mock: Mock) -> (Result<Value, GcpError>, usize) {
        let (c, mock) = client(mock);
        let r = wait_compute_zone_operation(&c, "my-proj", "us-central1-a", "operation-1", fast())
            .await;
        (r, mock.requests().len())
    }

    #[tokio::test(start_paused = true)]
    async fn compute_wait_polls_until_done() {
        let mock = Mock::default()
            .on(
                HttpMethod::Post,
                WAIT,
                200,
                r#"{"name":"operation-1","status":"RUNNING"}"#,
            )
            .on(
                HttpMethod::Post,
                WAIT,
                200,
                r#"{"name":"operation-1","status":"DONE","targetLink":"t"}"#,
            );
        let (r, calls) = compute(mock).await;
        assert_eq!(r.unwrap()["targetLink"], "t");
        assert_eq!(calls, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn compute_wait_reports_the_operation_error() {
        let mock = Mock::default().on(
            HttpMethod::Post,
            WAIT,
            200,
            r#"{"status":"DONE","error":{"errors":[{"code":"ZONE_RESOURCE_POOL_EXHAUSTED","message":"no GPUs"}]}}"#,
        );
        let (r, _) = compute(mock).await;
        assert_eq!(
            r,
            Err(GcpError::OperationFailed {
                operation: "operation-1".into(),
                code: "ZONE_RESOURCE_POOL_EXHAUSTED".into(),
                message: "no GPUs".into()
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn compute_wait_times_out() {
        let mock = Mock::default().on(HttpMethod::Post, WAIT, 200, r#"{"status":"RUNNING"}"#);
        let (r, calls) = compute(mock).await;
        assert_eq!(
            r,
            Err(GcpError::OperationTimeout {
                operation: "operation-1".into()
            })
        );
        assert!(calls > 1);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_errors_are_retried_but_others_end_the_wait() {
        let mock = Mock::default()
            .then(HttpMethod::Post, WAIT, Err(TransportError::Timeout))
            .then(HttpMethod::Post, WAIT, Ok(HttpResponse::new(503, "")))
            .on(HttpMethod::Post, WAIT, 200, r#"{"status":"DONE"}"#);
        assert!(compute(mock).await.0.is_ok());

        let mock = Mock::default().on(HttpMethod::Post, WAIT, 404, "{}");
        let (r, calls) = compute(mock).await;
        assert!(r.unwrap_err().is_not_found());
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn compute_wait_rejects_unsafe_names_before_any_request() {
        let (c, mock) = client(Mock::default());
        for (p, z, o) in [
            ("my-proj", "us-central1-a", "../x"),
            ("my-proj", "bad zone", "op-1"),
            ("X", "us-central1-a", "op-1"),
        ] {
            assert!(
                wait_compute_zone_operation(&c, p, z, o, fast())
                    .await
                    .is_err(),
                "{p} {z} {o}"
            );
        }
        assert!(mock.requests().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn run_operation_polls_until_done() {
        let (c, mock) = client(
            Mock::default()
                .on(HttpMethod::Get, RUN_OP, 200, r#"{"name":"n"}"#)
                .on(
                    HttpMethod::Get,
                    RUN_OP,
                    200,
                    r#"{"name":"n","done":true,"response":{"uri":"u"}}"#,
                ),
        );
        let op = wait_run_operation(&c, RUN_OP, fast()).await.unwrap();
        assert_eq!(op["response"]["uri"], "u");
        assert_eq!(mock.requests().len(), 2);
        assert_eq!(
            mock.requests()[0].url,
            format!("https://run.googleapis.com/v2/{RUN_OP}")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_operation_failure_and_timeout() {
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            RUN_OP,
            200,
            r#"{"done":true,"error":{"code":8,"message":"quota"}}"#,
        ));
        assert_eq!(
            wait_run_operation(&c, RUN_OP, fast()).await,
            Err(GcpError::OperationFailed {
                operation: RUN_OP.into(),
                code: "8".into(),
                message: "quota".into()
            })
        );

        let (c, _) = client(Mock::default().on(HttpMethod::Get, RUN_OP, 200, r#"{"done":false}"#));
        assert_eq!(
            wait_run_operation(&c, RUN_OP, fast()).await,
            Err(GcpError::OperationTimeout {
                operation: RUN_OP.into()
            })
        );
    }

    #[tokio::test]
    async fn run_operation_names_are_checked() {
        let (c, mock) = client(Mock::default());
        for bad in [
            "operations/x",
            "projects/p/locations/l/operations/../x",
            "projects/p/locations/l/operations/./x",
            "projects/./locations/l/operations/x",
            "projects/p/locations/l/operations/a?b",
            "projects/p/locations/l/operations/a/b",
            "",
        ] {
            assert!(wait_run_operation(&c, bad, fast()).await.is_err(), "{bad}");
        }
        assert!(mock.requests().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn hard_quota_ends_the_wait_but_rate_limits_are_retried() {
        let mock = Mock::default().on(
            HttpMethod::Post,
            WAIT,
            403,
            r#"{"error":{"errors":[{"reason":"quotaExceeded"}],"message":"q"}}"#,
        );
        let (r, calls) = compute(mock).await;
        assert!(r.unwrap_err().is_quota());
        assert_eq!(calls, 1);

        let mock = Mock::default().on(HttpMethod::Post, WAIT, 429, "{}").on(
            HttpMethod::Post,
            WAIT,
            200,
            r#"{"status":"DONE"}"#,
        );
        let (r, calls) = compute(mock).await;
        assert!(r.is_ok());
        assert_eq!(calls, 2);
    }

    /// A transport whose requests never complete.
    struct Hang;

    #[async_trait::async_trait]
    impl amicu_providers::HttpTransport for Hang {
        async fn execute(
            &self,
            _request: amicu_providers::HttpRequest,
        ) -> Result<HttpResponse, TransportError> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_request_cannot_overshoot_the_deadline() {
        let c = GcpClient::with_transport(
            std::sync::Arc::new(crate::auth::fake::FakeTokens(Ok("t"))),
            std::sync::Arc::new(Hang),
        );
        let start = Instant::now();
        let r = wait_run_operation(&c, RUN_OP, fast()).await;
        assert_eq!(
            r,
            Err(GcpError::OperationTimeout {
                operation: RUN_OP.into()
            })
        );
        assert!(start.elapsed() <= Duration::from_secs(30) + Duration::from_millis(1));
    }
}
