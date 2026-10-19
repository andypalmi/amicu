//! Serverless worker logs: `GET /serverless/{id}/workers`, then each worker's
//! `GET /serverless/{id}/workers/{workerId}/logs` Server-Sent Events stream
//! (`data: {"ts", "source", "line"}`), read for a bounded time and merged
//! with a worker prefix. Logs exist only while a worker does: a scaled-to-zero
//! endpoint has none.

use std::time::Duration;

use amicu_providers::StreamLimits;
use serde::Deserialize;

use crate::client::{RunPodClient, segment};
use crate::error::RunPodError;

/// Lines RunPod backfills per worker unless told otherwise.
pub const DEFAULT_TAIL: u32 = 100;

/// RunPod's ceiling for `tail`.
pub const MAX_TAIL: u32 = 5000;

/// How long one worker's stream is read: the backfill arrives at once, so
/// reading stops after a short quiet spell.
pub const WORKER_LOG_LIMITS: StreamLimits = StreamLimits {
    total: Duration::from_secs(15),
    idle: Duration::from_millis(1500),
    max_bytes: 4 * 1024 * 1024,
};

/// What to say when no worker is up.
pub const NO_WORKERS: &str = "no RunPod workers are running (the endpoint is scaled to zero); \
     logs exist only while a worker is up - send a request to start one";

/// One log event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LogLine {
    #[serde(default)]
    pub ts: Option<String>,
    /// `container` or `system`.
    #[serde(default)]
    pub source: Option<String>,
    pub line: String,
}

/// One worker's logs, or why they could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerLogs {
    pub worker_id: String,
    pub status: String,
    pub lines: Result<Vec<LogLine>, String>,
}

/// The `data:` payloads of complete events in an SSE body. An event cut off
/// by the bounded read (no JSON body) is dropped; a non-JSON payload is kept
/// as a bare line.
#[must_use]
pub fn parse_sse(body: &[u8]) -> Vec<LogLine> {
    let text = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    let mut flush = |data: &mut Vec<&str>| {
        if data.is_empty() {
            return;
        }
        let payload = data.join("\n");
        data.clear();
        match serde_json::from_str::<LogLine>(&payload) {
            Ok(line) => out.push(line),
            Err(_) if !payload.trim_start().starts_with('{') => out.push(LogLine {
                ts: None,
                source: None,
                line: payload,
            }),
            Err(_) => {}
        }
    };
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            flush(&mut data);
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    flush(&mut data);
    out
}

/// Every worker's lines as `[<worker>] <line>`, ordered by timestamp (stable,
/// so a worker's own order is kept); a worker whose logs failed contributes
/// one `[<worker>] logs unavailable: ...` line.
#[must_use]
pub fn merge(workers: &[WorkerLogs]) -> Vec<String> {
    let mut lines: Vec<(Option<&str>, String)> = Vec::new();
    for w in workers {
        match &w.lines {
            Ok(ls) => lines.extend(ls.iter().map(|l| {
                let source = match l.source.as_deref() {
                    Some("system") => " (system)",
                    _ => "",
                };
                (
                    l.ts.as_deref(),
                    format!("[{}]{source} {}", w.worker_id, l.line.trim_end()),
                )
            })),
            Err(e) => lines.push((None, format!("[{}] logs unavailable: {e}", w.worker_id))),
        }
    }
    lines.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    lines.into_iter().map(|(_, l)| l).collect()
}

impl RunPodClient {
    /// One worker's recent log lines (`tail` backfilled, clamped to
    /// [`MAX_TAIL`]), read within `limits`.
    ///
    /// # Errors
    /// The usual [`RunPodError`] mapping; `404` when the worker is gone.
    pub async fn worker_logs(
        &self,
        endpoint_id: &str,
        worker_id: &str,
        tail: u32,
        limits: StreamLimits,
    ) -> Result<Vec<LogLine>, RunPodError> {
        let path = format!(
            "/serverless/{}/workers/{}/logs?tail={}",
            segment(endpoint_id)?,
            segment(worker_id)?,
            tail.min(MAX_TAIL)
        );
        let resp = self
            .get_stream(&path, &format!("worker `{worker_id}`"), limits)
            .await?;
        Ok(parse_sse(&resp.body))
    }

    /// The logs of every worker endpoint `endpoint_id` has, one entry per
    /// worker (empty when it is scaled to zero). A worker whose stream fails
    /// keeps its error rather than failing the rest.
    ///
    /// # Errors
    /// When the worker list itself cannot be read.
    pub async fn endpoint_logs(
        &self,
        endpoint_id: &str,
        tail: u32,
        limits: StreamLimits,
    ) -> Result<Vec<WorkerLogs>, RunPodError> {
        let workers = self.list_workers(endpoint_id).await?.workers;
        let mut out = Vec::with_capacity(workers.len());
        for w in workers {
            let lines = self
                .worker_logs(endpoint_id, &w.id, tail, limits)
                .await
                .map_err(|e| e.to_string());
            out.push(WorkerLogs {
                worker_id: w.id,
                status: w.status,
                lines,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use amicu_providers::{HttpRequest, HttpResponse, HttpTransport, StreamLimits, TransportError};
    use async_trait::async_trait;

    use super::*;
    use crate::client::ApiKey;

    #[test]
    fn sse_events_are_parsed_and_a_cut_off_event_is_dropped() {
        let body = b"id: 2026-06-01T12:05:04Z/1\ndata: {\"ts\":\"2026-06-01T12:05:04Z\",\"source\":\"container\",\"line\":\"Worker ready.\"}\n\n: keep-alive\n\nid: x\r\ndata: plain text\r\n\r\ndata: {\"ts\":\"2026-06-01T12:05:05Z\",\"li";
        let lines = parse_sse(body);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].line, "Worker ready.");
        assert_eq!(lines[0].source.as_deref(), Some("container"));
        assert_eq!(lines[1].line, "plain text");
        assert_eq!(lines[1].ts, None);
    }

    #[test]
    fn merge_orders_by_time_and_prefixes_the_worker() {
        let at = |ts: &str, line: &str, source: &str| LogLine {
            ts: Some(ts.to_owned()),
            source: Some(source.to_owned()),
            line: line.to_owned(),
        };
        let merged = merge(&[
            WorkerLogs {
                worker_id: "w1".into(),
                status: "RUNNING".into(),
                lines: Ok(vec![at("2026-01-01T00:00:01Z", "a", "container")]),
            },
            WorkerLogs {
                worker_id: "w2".into(),
                status: "IDLE".into(),
                lines: Ok(vec![at("2026-01-01T00:00:00Z", "b\n", "system")]),
            },
            WorkerLogs {
                worker_id: "w3".into(),
                status: "INITIALIZING".into(),
                lines: Err("RunPod resource not found: worker `w3`".into()),
            },
        ]);
        assert_eq!(
            merged,
            [
                "[w2] (system) b",
                "[w1] a",
                "[w3] logs unavailable: RunPod resource not found: worker `w3`"
            ]
        );
    }

    #[derive(Default)]
    struct Mock {
        seen: Mutex<Vec<(String, Option<String>, bool)>>,
    }

    #[async_trait]
    impl HttpTransport for Mock {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.seen
                .lock()
                .unwrap()
                .push((request.url.clone(), request.bearer.clone(), false));
            if request.url.ends_with("/workers") {
                let body = r#"{"summary":{"running":1,"idle":1,"total":2},"workers":[
                    {"id":"w1","status":"RUNNING"},{"id":"w2","status":"IDLE"}]}"#;
                return Ok(HttpResponse::new(200, body.as_bytes().to_vec()));
            }
            Ok(HttpResponse::new(500, Vec::new()))
        }

        async fn execute_bounded(
            &self,
            request: HttpRequest,
            _limits: StreamLimits,
        ) -> Result<HttpResponse, TransportError> {
            let accept = request
                .headers
                .iter()
                .any(|(k, v)| k == "Accept" && v == "text/event-stream");
            self.seen
                .lock()
                .unwrap()
                .push((request.url.clone(), request.bearer.clone(), accept));
            if request.url.contains("/workers/w2/") {
                return Ok(HttpResponse::new(404, b"{}".to_vec()));
            }
            let body = "data: {\"ts\":\"t1\",\"source\":\"container\",\"line\":\"hello\"}\n\n";
            Ok(HttpResponse::new(200, body.as_bytes().to_vec()))
        }
    }

    #[tokio::test]
    async fn endpoint_logs_reads_each_worker_and_keeps_per_worker_errors() {
        let mock = Arc::new(Mock::default());
        let client =
            RunPodClient::with_transport(ApiKey::new("rpa_K"), mock.clone(), "https://x/v2");
        let logs = client
            .endpoint_logs("ep1", 9999, WORKER_LOG_LIMITS)
            .await
            .unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].lines.as_ref().unwrap()[0].line, "hello");
        assert!(logs[1].lines.as_ref().unwrap_err().contains("not found"));
        let seen = mock.seen.lock().unwrap();
        assert_eq!(seen[0].0, "https://x/v2/serverless/ep1/workers");
        assert_eq!(
            seen[1].0,
            "https://x/v2/serverless/ep1/workers/w1/logs?tail=5000"
        );
        assert!(seen[1].2, "asks for an event stream");
        assert_eq!(seen[1].1.as_deref(), Some("rpa_K"));
    }

    #[tokio::test]
    async fn a_worker_id_that_is_not_one_path_segment_is_refused() {
        let client = RunPodClient::with_transport(
            ApiKey::new("k"),
            Arc::new(Mock::default()),
            "https://x/v2",
        );
        let err = client
            .worker_logs("ep1", "../x", 10, WORKER_LOG_LIMITS)
            .await
            .unwrap_err();
        assert!(matches!(err, RunPodError::InvalidId(_)));
    }
}
