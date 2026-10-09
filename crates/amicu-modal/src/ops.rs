//! Operating a deployment already on Modal: log streaming, status, health,
//! and listing apps. `crate::deploy` creates/tears one down; this reads it
//! back. RPCs are cited against `modal-client` v1.3.1 (`proto/VENDORED.md`).

use std::ops::ControlFlow;
use std::time::Duration;

use crate::client::ModalClient;
use crate::error::ModalError;
use crate::pb::modal_client as pb;

/// Which stream a [`LogLine`] came from (`TaskLogs.file_descriptor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFd {
    Stdout,
    Stderr,
    /// Modal's own status lines, not the process's.
    Info,
    /// Unspecified, or a value newer than this crate knows.
    Unspecified,
}

impl From<i32> for LogFd {
    fn from(value: i32) -> Self {
        if value == pb::FileDescriptor::Stdout as i32 {
            Self::Stdout
        } else if value == pb::FileDescriptor::Stderr as i32 {
            Self::Stderr
        } else if value == pb::FileDescriptor::Info as i32 {
            Self::Info
        } else {
            Self::Unspecified
        }
    }
}

/// One printable line of container output, as delivered by `AppGetLogs`.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    /// The batch `entry_id` this line arrived in — pass back as
    /// `stream_logs`'s `since_entry_id` to resume just past it.
    pub entry_id: String,
    pub fd: LogFd,
    /// Exactly as Modal sent it (no trailing newline stripped or added).
    pub text: String,
    pub timestamp: f64,
}

/// Maps one `TaskLogsBatch` into zero or more [`LogLine`]s, mirroring
/// `_put_log`'s filtering (`_output.py:572-594`): a `task_state` change or
/// a `task_progress` entry with non-zero `len`/`pos` (an image-build byte
/// counter) carries no text. `last_entry_id` tracks the batch's own
/// `entry_id`, empty for `fd=server` messages (`:608-610`).
fn map_log_batch(batch: &pb::TaskLogsBatch, last_entry_id: &mut String) -> Vec<LogLine> {
    if !batch.entry_id.is_empty() {
        last_entry_id.clone_from(&batch.entry_id);
    }
    batch
        .items
        .iter()
        .filter(|log| {
            let is_progress = log
                .task_progress
                .as_ref()
                .is_some_and(|p| p.pos != 0 || p.len != 0);
            log.task_state == 0 && !is_progress && !log.data.is_empty()
        })
        .map(|log| LogLine {
            entry_id: batch.entry_id.clone(),
            fd: LogFd::from(log.file_descriptor),
            text: log.data.clone(),
            timestamp: log.timestamp,
        })
        .collect()
}

/// How long one `AppGetLogs` call is held open while following
/// (`_output.py:600-605`: `timeout=55`). Python's `get_app_logs_loop` always
/// tails, so it always sends this value.
const FOLLOW_LOG_POLL_TIMEOUT_SECS: f32 = 55.0;

/// How long one `AppGetLogs` call is held open for a one-shot (`follow =
/// false`) request. `AppGetLogsRequest.timeout` (`api.proto:368`, `float
/// timeout = 2`) is the same field Python always sets to 55.0 — there is no
/// dedicated "return immediately" mode on this RPC, so a one-shot call still
/// blocks for however long the server holds the stream open, just for a
/// shorter deadline than `--follow` uses. **5.0 is unverified against the
/// real server** beyond "it is the same field Python populates with a
/// different value"; if Modal enforces its own minimum poll window, one-shot
/// calls could still take longer than this.
const ONE_SHOT_LOG_POLL_TIMEOUT_SECS: f32 = 5.0;

/// Picks the `AppGetLogsRequest.timeout` for a [`ModalClient::stream_logs`]
/// call: the long [`FOLLOW_LOG_POLL_TIMEOUT_SECS`] while following, the
/// short [`ONE_SHOT_LOG_POLL_TIMEOUT_SECS`] otherwise. Split out from
/// `stream_logs` so the choice is testable without a socket.
fn log_poll_timeout_secs(follow: bool) -> f32 {
    if follow {
        FOLLOW_LOG_POLL_TIMEOUT_SECS
    } else {
        ONE_SHOT_LOG_POLL_TIMEOUT_SECS
    }
}

/// Status of one Function inside a deployed app. `running_containers` is
/// `FunctionStats.num_total_tasks` — the pinned proto has no
/// `num_total_runners` field; `url` derivation is in [`function_status`].
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionStatus {
    /// The Function's id (`fu-…`).
    pub function_id: String,
    pub name: String,
    pub url: Option<String>,
    pub running_containers: u32,
    /// Queued-but-not-running inputs (`FunctionStats.backlog`).
    pub backlog: u32,
}

/// Aggregated status of a deployed app, from `AppList` + `AppGetLayout` +
/// `FunctionGetCurrentStats`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppStatus {
    /// The Modal app id (`ap-…`).
    pub app_id: String,
    /// A human-readable state label — see [`app_state_label`].
    pub state: String,
    pub functions: Vec<FunctionStatus>,
}

/// A row from `AppList`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppSummary {
    /// Empty for an ephemeral/anonymous app.
    pub name: String,
    pub app_id: String,
    pub state: String,
    pub created_at: f64,
}

/// Maps `AppState` to the labels `modal app list`'s `APP_STATE_TO_MESSAGE`
/// table prints (`modal/cli/app.py:26-35`), minus its Rich styling.
fn app_state_label(state: i32) -> String {
    use pb::AppState;
    let label = if state == AppState::Deployed as i32 {
        "deployed"
    } else if state == AppState::Detached as i32 || state == AppState::DetachedDisconnected as i32 {
        "ephemeral (detached)"
    } else if state == AppState::Disabled as i32 {
        "disabled"
    } else if state == AppState::Ephemeral as i32 {
        "ephemeral"
    } else if state == AppState::Initializing as i32 {
        "initializing"
    } else if state == AppState::Stopped as i32 {
        "stopped"
    } else if state == AppState::Stopping as i32 {
        "stopping"
    } else {
        "unknown"
    };
    label.to_owned()
}

/// Builds a [`FunctionStatus`] from `AppGetLayout` handle metadata plus a
/// `FunctionGetCurrentStats` result. `FunctionHandleMetadata` has no
/// `flash_service_urls` like `Function` does, only `web_url` and
/// `experimental_flash_urls` (`api.proto:1885,1897`) — both checked here;
/// `tests/live_ops.rs` settles which a real server populates.
fn function_status(
    function_id: String,
    meta: &pb::FunctionHandleMetadata,
    stats: pb::FunctionStats,
) -> FunctionStatus {
    let url = (!meta.web_url.is_empty())
        .then(|| meta.web_url.clone())
        .or_else(|| meta.experimental_flash_urls.first().cloned());
    FunctionStatus {
        function_id,
        name: meta.function_name.clone(),
        url,
        running_containers: stats.num_total_tasks,
        backlog: stats.backlog,
    }
}

/// Every Function object in an `AppLayout`. Unlike `get_app_objects`
/// (`__init__.py:143-146`), the class-service entry (`"<Name>.*"`) is kept:
/// amicu's server has no distinct `Cls` object to filter it in favor of.
pub(crate) fn function_objects(
    layout: &pb::AppLayout,
) -> Vec<(String, &pb::FunctionHandleMetadata)> {
    layout
        .objects
        .iter()
        .filter(|object| !object.object_id.is_empty())
        .filter_map(|object| match &object.handle_metadata_oneof {
            Some(pb::object::HandleMetadataOneof::FunctionHandleMetadata(meta)) => {
                Some((object.object_id.clone(), meta))
            }
            _ => None,
        })
        .collect()
}

/// The result of an HTTP GET health probe against a deployed server's URL.
#[derive(Debug, Clone, PartialEq)]
pub enum HealthStatus {
    /// The server answered with this HTTP status code (2xx or not).
    Ok(u16),
    /// The request could not complete (DNS, connect, TLS, timeout, …).
    /// Carries `reqwest`'s error text — no auth header is sent here.
    Unreachable(String),
}

/// Pure mapping from a GET attempt's outcome to a [`HealthStatus`], split
/// out from [`ModalClient::health`] so it is testable without a socket.
fn classify_probe(outcome: Result<u16, String>) -> HealthStatus {
    match outcome {
        Ok(code) => HealthStatus::Ok(code),
        Err(reason) => HealthStatus::Unreachable(reason),
    }
}

/// Joins a base URL and health-check path with exactly one `/` between them.
fn build_health_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

impl ModalClient {
    /// Streams `AppGetLogs` for `app_id`, calling `sink` for each kept
    /// [`LogLine`]; `since_entry_id` resumes past a previously seen entry.
    /// Mirrors `get_app_logs_loop` (`modal/_output.py:543-675`): each call
    /// is held open for [`log_poll_timeout_secs`] (the full follow-mode
    /// window, or a short one-shot window — see its doc); `follow` decides
    /// whether to reconnect when a call ends without `app_done` (Python's
    /// helper always reconnects, since `modal app logs` always tails —
    /// `follow` adds a one-shot mode). Propagates the first RPC failure,
    /// unlike Python's indefinite tail, which silently retries transient
    /// errors.
    ///
    /// `sink` returning [`ControlFlow::Break`] stops streaming immediately
    /// — no more lines are delivered from the batch it broke in and no
    /// further `AppGetLogs` call is made, unlike Python's `OutputManager`,
    /// which has no equivalent early-exit.
    ///
    /// Unlike Python's default CLI output, which suppresses image-build log
    /// batches through its `OutputManager`, amicu has no such manager: build
    /// noise is delivered to `sink` like any other log line.
    pub async fn stream_logs(
        &mut self,
        app_id: &str,
        since_entry_id: Option<String>,
        follow: bool,
        mut sink: impl FnMut(LogLine) -> ControlFlow<()>,
    ) -> Result<(), ModalError> {
        let mut last_entry_id = since_entry_id.unwrap_or_default();
        let timeout = log_poll_timeout_secs(follow);

        loop {
            let mut stream = self
                .stub()
                .app_get_logs(pb::AppGetLogsRequest {
                    app_id: app_id.to_owned(),
                    timeout,
                    last_entry_id: last_entry_id.clone(),
                    ..pb::AppGetLogsRequest::default()
                })
                .await
                .map_err(|status| ModalError::from_status("AppGetLogs", status))?
                .into_inner();

            let mut app_done = false;
            while let Some(batch) = stream
                .message()
                .await
                .map_err(|status| ModalError::from_status("AppGetLogs", status))?
            {
                if batch.app_done {
                    app_done = true;
                    break;
                }
                for line in map_log_batch(&batch, &mut last_entry_id) {
                    if sink(line).is_break() {
                        return Ok(());
                    }
                }
            }
            if app_done || !follow {
                return Ok(());
            }
        }
    }

    /// `AppList`, filtered by id — the pinned proto has no "get one app's
    /// state" RPC.
    async fn lookup_app_state(&mut self, app_id: &str) -> Result<String, ModalError> {
        let response = self
            .stub()
            .app_list(pb::AppListRequest::default())
            .await
            .map_err(|status| ModalError::from_status("AppList", status))?
            .into_inner();
        let state = response
            .apps
            .into_iter()
            .find(|app| app.app_id == app_id)
            .map_or(0, |app| app.state);
        Ok(app_state_label(state))
    }

    /// Resolves `app_name` to a deployment, its state, and its Functions'
    /// live stats (`Ok(None)` when no such app is deployed): `find_app` →
    /// `AppList` (state) → `AppGetLayout` (`modal/experimental
    /// /__init__.py:135-136` — v1.3.1's own `get_app_objects` uses this, not
    /// the older `AppGetObjects` no code path in this pin calls) →
    /// `FunctionGetCurrentStats` per Function found.
    pub async fn app_status(&mut self, app_name: &str) -> Result<Option<AppStatus>, ModalError> {
        let Some(app_id) = self.find_app(app_name).await? else {
            return Ok(None);
        };

        let app_state = self.lookup_app_state(&app_id).await?;

        let layout = self
            .stub()
            .app_get_layout(pb::AppGetLayoutRequest {
                app_id: app_id.clone(),
            })
            .await
            .map_err(|status| ModalError::from_status("AppGetLayout", status))?
            .into_inner()
            .app_layout
            .unwrap_or_default();

        let mut functions = Vec::new();
        for (function_id, meta) in function_objects(&layout) {
            let current_stats = self
                .stub()
                .function_get_current_stats(pb::FunctionGetCurrentStatsRequest {
                    function_id: function_id.clone(),
                })
                .await
                .map_err(|status| ModalError::from_status("FunctionGetCurrentStats", status))?
                .into_inner();
            functions.push(function_status(function_id, meta, current_stats));
        }

        Ok(Some(AppStatus {
            app_id,
            state: app_state,
            functions,
        }))
    }

    /// Lists apps in the current environment, mirroring `modal app list`
    /// (`modal/cli/app.py:46-80`) minus its table rendering.
    pub async fn list_apps(&mut self) -> Result<Vec<AppSummary>, ModalError> {
        let response = self
            .stub()
            .app_list(pb::AppListRequest::default())
            .await
            .map_err(|status| ModalError::from_status("AppList", status))?
            .into_inner();
        Ok(response
            .apps
            .into_iter()
            .map(|app| AppSummary {
                name: app.name,
                app_id: app.app_id,
                state: app_state_label(app.state),
                created_at: app.created_at,
            })
            .collect())
    }

    /// A minimal HTTP GET health probe against a deployed server's tunnel
    /// URL. Not a Modal RPC, but kept on `ModalClient` with the other ops.
    pub async fn health(url: &str, path: &str, timeout: Duration) -> HealthStatus {
        let target = build_health_url(url, path);
        let outcome = async {
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|error| error.to_string())?;
            let response = client
                .get(&target)
                .send()
                .await
                .map_err(|error| error.to_string())?;
            Ok(response.status().as_u16())
        }
        .await;
        classify_probe(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_log(fd: pb::FileDescriptor, data: &str) -> pb::TaskLogs {
        pb::TaskLogs {
            data: data.to_owned(),
            file_descriptor: fd as i32,
            timestamp: 1_700_000_000.5,
            ..pb::TaskLogs::default()
        }
    }

    #[test]
    fn log_batch_keeps_data_lines_tracks_entry_id_and_drops_noise() {
        let mut progress_only = task_log(pb::FileDescriptor::Stdout, "");
        progress_only.task_progress = Some(pb::TaskProgress {
            pos: 512,
            len: 1024,
            ..pb::TaskProgress::default()
        });
        let mut state_change = task_log(pb::FileDescriptor::Stdout, "ignored\n");
        state_change.task_state = pb::TaskState::Active as i32;

        let batch = pb::TaskLogsBatch {
            entry_id: "en-1".to_owned(),
            items: vec![
                task_log(pb::FileDescriptor::Stdout, "hello\n"),
                task_log(pb::FileDescriptor::Stderr, "oops\n"),
                progress_only,
                state_change,
                task_log(pb::FileDescriptor::Stdout, ""),
            ],
            ..pb::TaskLogsBatch::default()
        };
        let mut last_entry_id = String::new();
        let lines = map_log_batch(&batch, &mut last_entry_id);

        assert_eq!(last_entry_id, "en-1");
        assert_eq!(
            lines,
            vec![
                LogLine {
                    entry_id: "en-1".to_owned(),
                    fd: LogFd::Stdout,
                    text: "hello\n".to_owned(),
                    timestamp: 1_700_000_000.5,
                },
                LogLine {
                    entry_id: "en-1".to_owned(),
                    fd: LogFd::Stderr,
                    text: "oops\n".to_owned(),
                    timestamp: 1_700_000_000.5,
                },
            ]
        );

        // entry_id is empty for fd=server messages (`_output.py:608-610`);
        // last_entry_id must not regress when that happens.
        let server_batch = pb::TaskLogsBatch {
            entry_id: String::new(),
            items: vec![task_log(pb::FileDescriptor::Info, "server message\n")],
            ..pb::TaskLogsBatch::default()
        };
        map_log_batch(&server_batch, &mut last_entry_id);
        assert_eq!(last_entry_id, "en-1", "unchanged by an entry_id-less batch");
    }

    // Both sides of every comparison here are exact literal constants (no
    // arithmetic that could round differently), so strict float equality is
    // safe despite `clippy::float_cmp`'s general advice against it.
    #[test]
    #[allow(clippy::float_cmp)]
    fn log_poll_timeout_is_short_one_shot_and_full_length_when_following() {
        assert_eq!(log_poll_timeout_secs(false), ONE_SHOT_LOG_POLL_TIMEOUT_SECS);
        assert_eq!(log_poll_timeout_secs(true), FOLLOW_LOG_POLL_TIMEOUT_SECS);
        assert_eq!(ONE_SHOT_LOG_POLL_TIMEOUT_SECS, 5.0);
        assert_eq!(FOLLOW_LOG_POLL_TIMEOUT_SECS, 55.0);
    }

    /// Mirrors the CLI's sink breaking after a write error: a fake consumer
    /// that keeps up to `limit` lines and then returns `Break`, applied to
    /// [`map_log_batch`]'s output the same way `stream_logs` applies a real
    /// sink to it. Asserts streaming stops exactly at that count rather
    /// than draining the rest of the batch.
    #[test]
    fn sink_break_stops_line_delivery_after_the_requested_count() {
        let batch = pb::TaskLogsBatch {
            entry_id: "en-1".to_owned(),
            items: vec![
                task_log(pb::FileDescriptor::Stdout, "one\n"),
                task_log(pb::FileDescriptor::Stdout, "two\n"),
                task_log(pb::FileDescriptor::Stdout, "three\n"),
            ],
            ..pb::TaskLogsBatch::default()
        };
        let mut last_entry_id = String::new();
        let lines = map_log_batch(&batch, &mut last_entry_id);
        assert_eq!(lines.len(), 3, "sanity: all three lines kept");

        let limit = 2;
        let mut delivered = Vec::new();
        let mut sink = |line: LogLine| {
            if delivered.len() >= limit {
                return ControlFlow::Break(());
            }
            delivered.push(line.text);
            if delivered.len() >= limit {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };

        let mut broke = false;
        for line in lines {
            if sink(line).is_break() {
                broke = true;
                break;
            }
        }
        assert!(broke, "the fake consumer must have stopped early");
        assert_eq!(delivered, vec!["one\n".to_owned(), "two\n".to_owned()]);
    }

    #[test]
    fn app_state_labels_match_the_python_cli_table() {
        use pb::AppState;
        for (state, label) in [
            (AppState::Deployed, "deployed"),
            (AppState::Detached, "ephemeral (detached)"),
            (AppState::DetachedDisconnected, "ephemeral (detached)"),
            (AppState::Stopped, "stopped"),
            (AppState::Stopping, "stopping"),
            (AppState::Initializing, "initializing"),
            (AppState::Disabled, "disabled"),
            (AppState::Ephemeral, "ephemeral"),
            (AppState::Unspecified, "unknown"),
        ] {
            assert_eq!(app_state_label(state as i32), label, "{state:?}");
        }
        assert_eq!(app_state_label(999), "unknown");
    }

    fn sample_layout() -> pb::AppLayout {
        pb::AppLayout {
            objects: vec![
                pb::Object {
                    object_id: "fu-server".to_owned(),
                    handle_metadata_oneof: Some(
                        pb::object::HandleMetadataOneof::FunctionHandleMetadata(
                            pb::FunctionHandleMetadata {
                                function_name: "Server".to_owned(),
                                web_url: "https://a.modal.run".to_owned(),
                                experimental_flash_urls: vec!["https://b.modal.run".to_owned()],
                                ..pb::FunctionHandleMetadata::default()
                            },
                        ),
                    ),
                },
                pb::Object {
                    object_id: "vo-weights".to_owned(),
                    handle_metadata_oneof: Some(pb::object::HandleMetadataOneof::VolumeMetadata(
                        pb::VolumeMetadata::default(),
                    )),
                },
            ],
            ..pb::AppLayout::default()
        }
    }

    #[test]
    fn function_objects_skips_non_function_objects_and_status_prefers_web_url() {
        let layout = sample_layout();
        let found = function_objects(&layout);
        assert_eq!(found.len(), 1, "the volume object must be skipped");
        let (id, mut meta) = (found[0].0.clone(), found[0].1.clone());
        let current_stats = pb::FunctionStats {
            backlog: 3,
            num_total_tasks: 2,
        };

        let status = function_status(id.clone(), &meta, current_stats);
        assert_eq!(status.url, Some("https://a.modal.run".to_owned()));
        assert_eq!(status.backlog, 3);
        assert_eq!(status.running_containers, 2);

        meta.web_url.clear();
        assert_eq!(
            function_status(id.clone(), &meta, current_stats).url,
            Some("https://b.modal.run".to_owned()),
            "falls back to experimental_flash_urls"
        );

        meta.experimental_flash_urls.clear();
        assert_eq!(function_status(id, &meta, current_stats).url, None);
    }

    #[test]
    fn app_status_aggregates_state_and_function_stats_from_fixtures() {
        let layout = sample_layout();
        let functions: Vec<FunctionStatus> = function_objects(&layout)
            .into_iter()
            .map(|(id, meta)| {
                function_status(
                    id,
                    meta,
                    pb::FunctionStats {
                        backlog: 0,
                        num_total_tasks: 1,
                    },
                )
            })
            .collect();
        let status = AppStatus {
            app_id: "ap-1".to_owned(),
            state: app_state_label(pb::AppState::Deployed as i32),
            functions,
        };

        assert_eq!(status.state, "deployed");
        assert_eq!(status.functions.len(), 1);
        assert_eq!(status.functions[0].function_id, "fu-server");
        assert_eq!(status.functions[0].running_containers, 1);
    }

    #[test]
    fn health_url_and_probe_mapping() {
        assert_eq!(
            build_health_url("https://x.modal.run", "/health"),
            "https://x.modal.run/health"
        );
        assert_eq!(
            build_health_url("https://x.modal.run/", "health"),
            "https://x.modal.run/health"
        );
        assert_eq!(
            build_health_url("https://x.modal.run", ""),
            "https://x.modal.run/"
        );

        assert_eq!(classify_probe(Ok(200)), HealthStatus::Ok(200));
        assert_eq!(classify_probe(Ok(503)), HealthStatus::Ok(503));
        assert_eq!(
            classify_probe(Err("connection refused".to_owned())),
            HealthStatus::Unreachable("connection refused".to_owned())
        );
    }
}
