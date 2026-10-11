//! `amicu modal auth-check` / `status` / `logs` / `metrics` / `stop` - operate a Modal
//! deployment from the command line.
//!
//! Project-independent (like `skill export`): none of these need a `*.hcl`
//! config, just `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` in the environment.
//! `ModalError`'s `Display` never contains a token, so none of the output
//! built here ever touches `Credentials` directly.

use std::io::{self, Write};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use amicu_modal::metrics::{self, MetricsSnapshot, Percentiles};
use amicu_modal::{AppStatus, Credentials, LogFd, LogLine, ModalClient, ModalError};

use crate::commands::build_runtime;
use crate::error::CliError;
use crate::project::Project;

fn io_err(error: io::Error) -> CliError {
    CliError::io("write failed", error)
}

/// Connects to Modal and calls `ClientHello`, the minimal RPC that
/// confirms the configured token is accepted. Exits non-zero on any
/// failure (missing/invalid credentials, connect failure, or a non-OK
/// gRPC status).
pub(crate) fn auth_check(out: &mut dyn Write) -> Result<u8, CliError> {
    let credentials = Credentials::from_env().map_err(ModalError::from)?;
    let rt = build_runtime()?;

    let hello = rt.block_on(async {
        let mut client = ModalClient::connect(credentials).await?;
        client.hello().await
    })?;

    // ClientHelloResponse carries no workspace/server identity field at the
    // vendored proto pin (only `warning`/`server_warnings`) — print those
    // if present, otherwise the plain confirmation the task asks for.
    writeln!(out, "authenticated").map_err(io_err)?;
    if let Some(warning) = &hello.warning {
        writeln!(out, "  warning: {warning}").map_err(io_err)?;
    }
    for server_warning in &hello.server_warnings {
        writeln!(out, "  server warning: {server_warning}").map_err(io_err)?;
    }
    Ok(0)
}

/// Shows a deployed app's state and its Functions' live stats
/// (`ModalClient::app_status`: `AppGetByDeploymentName` → `AppList` →
/// `AppGetLayout` → `FunctionGetCurrentStats` per Function).
pub(crate) fn status(app_name: &str, out: &mut dyn Write) -> Result<u8, CliError> {
    let credentials = Credentials::from_env().map_err(ModalError::from)?;
    let rt = build_runtime()?;

    let status = rt.block_on(async {
        let mut client = ModalClient::connect(credentials).await?;
        client.app_status(app_name).await
    })?;

    write_status(out, app_name, status.as_ref())
}

/// Renders an `app_status` result. Split out from [`status`] so it is
/// testable against fixtures, without a network call.
fn write_status(
    out: &mut dyn Write,
    app_name: &str,
    status: Option<&AppStatus>,
) -> Result<u8, CliError> {
    let Some(status) = status else {
        writeln!(out, "no deployment named {app_name}").map_err(io_err)?;
        return Ok(1);
    };

    writeln!(out, "app {app_name} ({}): {}", status.app_id, status.state).map_err(io_err)?;
    if status.functions.is_empty() {
        writeln!(out, "  no functions found").map_err(io_err)?;
    }
    for function in &status.functions {
        let backlog = function
            .backlog
            .map_or_else(|| "n/a".to_owned(), |backlog| backlog.to_string());
        writeln!(
            out,
            "  {} ({}): running={} backlog={backlog}",
            function.name, function.function_id, function.running_containers
        )
        .map_err(io_err)?;
        if let Some(url) = &function.url {
            writeln!(out, "    url: {url}").map_err(io_err)?;
        }
    }
    Ok(0)
}

/// Formats one [`LogLine`] the way `logs` prints it: `[stdout] text`, with
/// any trailing newline the server sent stripped so `writeln!` does not
/// double it.
fn format_log_line(line: &LogLine) -> String {
    let fd = match line.fd {
        LogFd::Stdout => "stdout",
        LogFd::Stderr => "stderr",
        LogFd::Info => "info",
        LogFd::Unspecified => "?",
    };
    format!("[{fd}] {}", line.text.trim_end_matches('\n'))
}

/// Prints an app's logs (`ModalClient::stream_logs`). `--follow` streams
/// until the app finishes or the command is interrupted (e.g. Ctrl-C), each
/// `AppGetLogs` call held open up to 55s before silently reconnecting.
/// Without `--follow`, it is a single `AppGetLogs` call held open up to a
/// short 5s (or until the app finishes first) and then returns — not an
/// instant snapshot, but no longer the ~55s wait a shared follow-mode
/// timeout used to impose. That 5s value mirrors the same request field
/// Python always sets to 55.0; it is not otherwise verified against the
/// server. `--since` resumes just past a previously seen entry id.
pub(crate) fn logs(
    app_name: &str,
    follow: bool,
    since: Option<String>,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let credentials = Credentials::from_env().map_err(ModalError::from)?;
    let rt = build_runtime()?;

    let mut write_error = None;
    let found = rt.block_on(async {
        let mut client = ModalClient::connect(credentials).await?;
        let Some(app_id) = client.find_app(app_name).await? else {
            return Ok::<bool, ModalError>(false);
        };
        client
            .stream_logs(&app_id, since, follow, |line| {
                if let Err(error) = writeln!(out, "{}", format_log_line(&line)) {
                    write_error.get_or_insert(error);
                    // Stop immediately — e.g. a broken pipe from `| head`
                    // under `--follow` — instead of letting `stream_logs`
                    // keep consuming and reconnecting until the app ends.
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            })
            .await?;
        Ok(true)
    })?;

    if let Some(error) = write_error {
        return Err(io_err(error));
    }
    if !found {
        writeln!(out, "no deployment named {app_name}").map_err(io_err)?;
        return Ok(1);
    }
    Ok(0)
}

/// Prints a one-shot runtime-metrics snapshot of a deployed app, or with
/// `watch` a new one every `interval_secs` until interrupted. The URL comes
/// from the app's first Function that has one (the same lookup as `status`);
/// the scrape itself is a plain HTTPS GET with the proxy-auth headers, and its
/// errors never contain them.
pub(crate) fn metrics(
    app_name: &str,
    watch: bool,
    interval_secs: u64,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let credentials = Credentials::from_env().map_err(ModalError::from)?;
    let rt = build_runtime()?;

    let status = rt.block_on(async {
        let mut client = ModalClient::connect(credentials).await?;
        client.app_status(app_name).await
    })?;
    let Some(status) = status else {
        writeln!(out, "no deployment named {app_name}").map_err(io_err)?;
        return Ok(1);
    };
    let Some(url) = status.functions.iter().find_map(|f| f.url.clone()) else {
        writeln!(out, "{app_name} has no URL to scrape (is it a web server?)").map_err(io_err)?;
        return Ok(1);
    };

    let mut previous: Option<(MetricsSnapshot, Instant)> = None;
    loop {
        let snapshot = rt
            .block_on(metrics::scrape(&url))
            .map_err(|e| CliError::Message(e.to_string()))?;
        let now = Instant::now();
        let snapshot = match &previous {
            Some((before, at)) => snapshot.rate_since(before, now.duration_since(*at)),
            None => snapshot,
        };
        write_metrics(out, app_name, &snapshot, watch)?;
        out.flush().map_err(io_err)?;
        if !watch {
            return Ok(0);
        }
        previous = Some((snapshot, now));
        std::thread::sleep(Duration::from_secs(interval_secs));
    }
}

fn opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| v.to_string())
}

fn percentiles(value: Option<Percentiles>) -> String {
    value.map_or_else(
        || "n/a".to_owned(),
        |p| {
            format!(
                "p50={:.0}ms p90={:.0}ms p99={:.0}ms",
                p.p50_ms, p.p90_ms, p.p99_ms
            )
        },
    )
}

/// Renders one snapshot. Split out from [`metrics`] so it is testable
/// without a network call. `watch` only changes the hint shown where a rate
/// needs a second scrape.
fn write_metrics(
    out: &mut dyn Write,
    app_name: &str,
    m: &MetricsSnapshot,
    watch: bool,
) -> Result<(), CliError> {
    let tps = |v: Option<f64>| {
        v.map_or_else(
            || {
                if watch {
                    "n/a".to_owned()
                } else {
                    "n/a (run with --watch)".to_owned()
                }
            },
            |v| format!("{v:.1}"),
        )
    };
    let kv = m
        .kv_cache_usage
        .map_or_else(|| "n/a".to_owned(), |v| format!("{:.1}%", v * 100.0));
    writeln!(out, "metrics {app_name} ({})", m.runtime.label()).map_err(io_err)?;
    writeln!(
        out,
        "  requests:    total={} running={} waiting={}",
        opt(m.requests_total),
        opt(m.requests_running),
        opt(m.requests_waiting)
    )
    .map_err(io_err)?;
    writeln!(
        out,
        "  tokens:      prompt={} generated={}",
        opt(m.prompt_tokens_total),
        opt(m.generation_tokens_total)
    )
    .map_err(io_err)?;
    writeln!(
        out,
        "  tokens/s:    generation={} prompt={}",
        tps(m.generation_tokens_per_second),
        tps(m.prompt_tokens_per_second)
    )
    .map_err(io_err)?;
    writeln!(out, "  kv cache:    {kv}").map_err(io_err)?;
    writeln!(out, "  preemptions: {}", opt(m.preemptions)).map_err(io_err)?;
    writeln!(out, "  e2e latency: {}", percentiles(m.e2e_latency)).map_err(io_err)?;
    writeln!(out, "  ttft:        {}", percentiles(m.time_to_first_token)).map_err(io_err)?;
    writeln!(
        out,
        "  per token:   {}",
        percentiles(m.time_per_output_token)
    )
    .map_err(io_err)?;
    Ok(())
}

/// Stops a deployed app (`ModalClient::find_app` + `ModalClient::stop_app`).
pub(crate) fn stop(app_name: &str, out: &mut dyn Write) -> Result<u8, CliError> {
    let credentials = Credentials::from_env().map_err(ModalError::from)?;
    let rt = build_runtime()?;

    let stopped = rt.block_on(async {
        let mut client = ModalClient::connect(credentials).await?;
        match client.find_app(app_name).await? {
            Some(app_id) => {
                client.stop_app(&app_id).await?;
                Ok::<Option<String>, ModalError>(Some(app_id))
            }
            None => Ok(None),
        }
    })?;

    let code = write_stop_result(out, app_name, stopped.as_deref())?;
    if stopped.is_some() {
        forget_stopped_app(app_name, out)?;
    }
    Ok(code)
}

/// Keeps a project's state consistent with a successful `stop`: when run in a
/// project whose state holds `app_name`, drops that deployment and its
/// endpoint so the next `plan`/`apply` recreates it. Outside a project (no
/// state file) this does nothing; a state problem warns but never fails the
/// stop, which already happened.
fn forget_stopped_app(app_name: &str, out: &mut dyn Write) -> Result<(), CliError> {
    match Project::discover().and_then(|project| crate::refresh::forget_app(&project, app_name)) {
        Ok(Some(id)) => {
            writeln!(out, "removed deployment {id} from local state").map_err(io_err)?;
        }
        Ok(None) => {}
        Err(error @ CliError::StateLocked(_)) => {
            eprintln!("warning: skipped updating local state: {error}");
        }
        Err(error) => eprintln!("warning: could not update local state: {error}"),
    }
    Ok(())
}

/// Renders a `stop` result. Split out from [`stop`] so it is testable
/// without a network call.
fn write_stop_result(
    out: &mut dyn Write,
    app_name: &str,
    app_id: Option<&str>,
) -> Result<u8, CliError> {
    if let Some(app_id) = app_id {
        writeln!(out, "stopped {app_name} ({app_id})").map_err(io_err)?;
        Ok(0)
    } else {
        writeln!(out, "no deployment named {app_name}").map_err(io_err)?;
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use amicu_modal::FunctionStatus;

    use super::*;

    fn new_buf() -> Vec<u8> {
        Vec::new()
    }

    fn as_str(out: &[u8]) -> &str {
        std::str::from_utf8(out).expect("utf-8 output")
    }

    #[test]
    fn write_status_covers_no_deployment_functions_and_empty_functions() {
        let mut out = new_buf();
        let code = write_status(&mut out, "amicu-missing", None).expect("writes ok");
        assert_eq!(code, 1);
        assert_eq!(as_str(&out), "no deployment named amicu-missing\n");

        let status = AppStatus {
            app_id: "ap-1".to_owned(),
            state: "deployed".to_owned(),
            functions: vec![
                FunctionStatus {
                    function_id: "fu-1".to_owned(),
                    name: "Server".to_owned(),
                    url: Some("https://x.modal.run".to_owned()),
                    running_containers: 1,
                    backlog: Some(0),
                },
                FunctionStatus {
                    function_id: "fu-2".to_owned(),
                    name: "Idle".to_owned(),
                    url: None,
                    running_containers: 0,
                    backlog: Some(3),
                },
                FunctionStatus {
                    function_id: "fu-3".to_owned(),
                    name: "Srv".to_owned(),
                    url: None,
                    running_containers: 2,
                    backlog: None,
                },
            ],
        };
        let mut out = new_buf();
        let code = write_status(&mut out, "amicu-app", Some(&status)).expect("writes ok");
        assert_eq!(code, 0);
        let rendered = as_str(&out);
        assert!(rendered.contains("app amicu-app (ap-1): deployed"));
        assert!(rendered.contains("Server (fu-1): running=1 backlog=0"));
        assert!(rendered.contains("Idle (fu-2): running=0 backlog=3"));
        assert!(rendered.contains("Srv (fu-3): running=2 backlog=n/a"));
        // Only the first function has a URL to print.
        assert_eq!(rendered.matches("url:").count(), 1, "{rendered}");

        let empty = AppStatus {
            app_id: "ap-2".to_owned(),
            state: "deployed".to_owned(),
            functions: Vec::new(),
        };
        let mut out = new_buf();
        write_status(&mut out, "amicu-empty", Some(&empty)).expect("writes ok");
        assert!(as_str(&out).contains("no functions found"));
    }

    #[test]
    fn format_log_line_labels_every_fd_and_strips_one_trailing_newline() {
        let make = |fd, text: &str| LogLine {
            entry_id: String::new(),
            fd,
            text: text.to_owned(),
            timestamp: 0.0,
        };
        assert_eq!(format_log_line(&make(LogFd::Stdout, "x")), "[stdout] x");
        assert_eq!(
            format_log_line(&make(LogFd::Stderr, "boom\n")),
            "[stderr] boom"
        );
        assert_eq!(format_log_line(&make(LogFd::Info, "x")), "[info] x");
        assert_eq!(format_log_line(&make(LogFd::Unspecified, "x")), "[?] x");
    }

    #[test]
    fn write_metrics_renders_vllm_and_marks_missing_series_na() {
        use amicu_modal::metrics::snapshot_from_text;
        let vllm = "vllm:request_success_total{finished_reason=\"stop\"} 7\n\
                    vllm:num_requests_running 1\n\
                    vllm:kv_cache_usage_perc 0.125\n\
                    vllm:generation_tokens_total 900\n\
                    vllm:e2e_request_latency_seconds_bucket{le=\"1.0\"} 10\n\
                    vllm:e2e_request_latency_seconds_bucket{le=\"+Inf\"} 10\n";
        let snap = snapshot_from_text(vllm).expect("vllm");
        let mut out = new_buf();
        write_metrics(&mut out, "amicu-app", &snap, false).expect("writes ok");
        let r = as_str(&out);
        assert!(r.contains("metrics amicu-app (vLLM)"), "{r}");
        assert!(r.contains("total=7 running=1 waiting=n/a"), "{r}");
        assert!(r.contains("generated=900"), "{r}");
        assert!(r.contains("generation=n/a (run with --watch)"), "{r}");
        assert!(r.contains("kv cache:    12.5%"), "{r}");
        assert!(r.contains("e2e latency: p50=500ms"), "{r}");
        assert!(r.contains("ttft:        n/a"), "{r}");

        let rated = snap.clone().rate_since(&snap, Duration::from_secs(1));
        let mut out = new_buf();
        write_metrics(&mut out, "amicu-app", &rated, true).expect("writes ok");
        assert!(as_str(&out).contains("generation=0.0"), "{}", as_str(&out));
    }

    #[test]
    fn write_metrics_renders_llamacpp_gauges() {
        use amicu_modal::metrics::snapshot_from_text;
        let snap = snapshot_from_text(
            "llamacpp:predicted_tokens_seconds 41.25\nllamacpp:requests_processing 2\n",
        )
        .expect("llama.cpp");
        let mut out = new_buf();
        write_metrics(&mut out, "amicu-llm", &snap, false).expect("writes ok");
        let r = as_str(&out);
        assert!(r.contains("(llama.cpp)"), "{r}");
        assert!(r.contains("total=n/a running=2"), "{r}");
        assert!(
            r.contains("generation=41.2") || r.contains("generation=41.3"),
            "{r}"
        );
        assert!(r.contains("e2e latency: n/a"), "{r}");
    }

    #[test]
    fn write_stop_result_reports_success_and_absence() {
        let mut out = new_buf();
        let code = write_stop_result(&mut out, "amicu-app", Some("ap-1")).expect("writes ok");
        assert_eq!(code, 0);
        assert_eq!(as_str(&out), "stopped amicu-app (ap-1)\n");

        let mut out = new_buf();
        let code = write_stop_result(&mut out, "amicu-missing", None).expect("writes ok");
        assert_eq!(code, 1);
        assert_eq!(as_str(&out), "no deployment named amicu-missing\n");
    }
}
