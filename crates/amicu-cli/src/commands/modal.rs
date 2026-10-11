//! `amicu modal auth-check` / `status` / `logs` / `stop` - operate a Modal
//! deployment from the command line.
//!
//! Project-independent (like `skill export`): none of these need a `*.hcl`
//! config, just `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` in the environment.
//! `ModalError`'s `Display` never contains a token, so none of the output
//! built here ever touches `Credentials` directly.

use std::io::{self, Write};
use std::ops::ControlFlow;

use amicu_modal::{AppStatus, Credentials, LogFd, LogLine, ModalClient, ModalError};

use crate::commands::build_runtime;
use crate::error::CliError;

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

    write_stop_result(out, app_name, stopped.as_deref())
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
