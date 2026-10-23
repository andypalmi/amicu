//! Hardware metrics for a deployed app's containers, over Modal's
//! `ContainerExec`.
//!
//! Modal has no GPU-usage RPC for deployed Functions, so the vendor-agnostic
//! probe from [`amicu_telemetry::hardware`] (one fixed `sh` script, no user
//! input in it) is run *inside* each running container and its stdout parsed
//! by the shared pure parsers.
//!
//! RPC sequence per container, from the vendored v1.3.1 `api.proto`:
//! 1. `TaskList` (all running tasks in the environment), filtered by `app_id`
//!    to find the app's containers (`TaskStats.task_id`);
//! 2. `ContainerExec{task_id, command: ["sh", "-c", PROBE_SCRIPT],
//!    stdout = PIPE, stderr = DEVNULL, timeout_secs = 10}` -> `exec_id`;
//! 3. `ContainerExecGetOutput{exec_id, file_descriptor = STDOUT,
//!    get_raw_bytes}` (server stream of `RuntimeOutputBatch`; the final batch
//!    carries `exit_code`);
//! 4. `ContainerExecWait` only when the stream ended without an exit code.
//!
//! A container whose exec fails does not fail the others or the caller:
//! each container's result is a `Result<HardwareSample, String>` whose error
//! text comes from [`ModalError`]'s `Display`, which never carries token
//! metadata.

use std::time::Duration;

pub use amicu_telemetry::hardware::{
    AcceleratorSample, HardwareSample, HostSample, MAX_PROBE_OUTPUT_BYTES, MemorySource,
    PROBE_TIMEOUT, Vendor,
};
use amicu_telemetry::hardware::{PROBE_SCRIPT, parse_probe_output};

use crate::client::ModalClient;
use crate::error::ModalError;
use crate::pb::modal_client as pb;

/// Grace on top of [`PROBE_TIMEOUT`] for the exec round trips themselves.
const EXEC_GRACE: Duration = Duration::from_secs(5);

/// One container's probe result.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerHardware {
    /// Modal task id (`ta-...`).
    pub task_id: String,
    /// The parsed sample, or why the probe could not run.
    pub outcome: Result<HardwareSample, String>,
}

/// Accumulates `ContainerExecGetOutput` batches.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ExecOutput {
    pub stdout: Vec<u8>,
    pub exit_code: Option<i32>,
    /// More than `cap` bytes were produced; the excess was dropped.
    pub truncated: bool,
}

impl ExecOutput {
    /// Folds one batch in. Returns `true` when the process has finished.
    pub(crate) fn absorb(&mut self, batch: &pb::RuntimeOutputBatch, cap: usize) -> bool {
        let stdout = i32::from(pb::FileDescriptor::Stdout);
        let unspecified = i32::from(pb::FileDescriptor::Unspecified);
        for item in &batch.items {
            if item.file_descriptor != stdout && item.file_descriptor != unspecified {
                continue;
            }
            // Raw bytes when present; older servers fill only `message`.
            let chunk: &[u8] = if item.message_bytes.is_empty() {
                item.message.as_bytes()
            } else {
                &item.message_bytes
            };
            let room = cap.saturating_sub(self.stdout.len());
            if chunk.len() > room {
                self.truncated = true;
            }
            self.stdout
                .extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        if batch.exit_code.is_some() {
            self.exit_code = batch.exit_code;
        }
        self.exit_code.is_some()
    }
}

impl ModalClient {
    /// Ids of the app's running containers: `TaskList` filtered by `app_id`.
    pub async fn app_task_ids(&mut self, app_id: &str) -> Result<Vec<String>, ModalError> {
        let response = self
            .stub()
            .task_list(pb::TaskListRequest::default())
            .await
            .map_err(|status| ModalError::from_status("TaskList", status))?
            .into_inner();
        Ok(response
            .tasks
            .into_iter()
            .filter(|task| task.app_id == app_id)
            .map(|task| task.task_id)
            .collect())
    }

    /// Runs `command` in `task_id`, returning at most `cap` bytes of stdout.
    async fn exec_capture(
        &mut self,
        task_id: &str,
        command: Vec<String>,
        timeout: Duration,
        cap: usize,
    ) -> Result<ExecOutput, ModalError> {
        let exec_id = self
            .stub()
            .container_exec(pb::ContainerExecRequest {
                task_id: task_id.to_owned(),
                command,
                stdout_output: pb::ExecOutputOption::Pipe.into(),
                stderr_output: pb::ExecOutputOption::Devnull.into(),
                timeout_secs: u32::try_from(timeout.as_secs()).unwrap_or(u32::MAX),
                ..pb::ContainerExecRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("ContainerExec", status))?
            .into_inner()
            .exec_id;

        let mut stream = self
            .stub()
            .container_exec_get_output(pb::ContainerExecGetOutputRequest {
                exec_id: exec_id.clone(),
                timeout: timeout.as_secs_f32(),
                last_batch_index: 0,
                file_descriptor: pb::FileDescriptor::Stdout.into(),
                get_raw_bytes: true,
            })
            .await
            .map_err(|status| ModalError::from_status("ContainerExecGetOutput", status))?
            .into_inner();

        let mut output = ExecOutput::default();
        while let Some(batch) = stream
            .message()
            .await
            .map_err(|status| ModalError::from_status("ContainerExecGetOutput", status))?
        {
            if output.absorb(&batch, cap) {
                break;
            }
        }

        if output.exit_code.is_none() {
            let waited = self
                .stub()
                .container_exec_wait(pb::ContainerExecWaitRequest {
                    exec_id,
                    timeout: 2.0,
                })
                .await
                .map_err(|status| ModalError::from_status("ContainerExecWait", status))?
                .into_inner();
            output.exit_code = waited.exit_code;
        }
        Ok(output)
    }

    /// Probes one container. Never returns a transport error: it becomes the
    /// `Err` text of the result.
    pub async fn probe_container(&mut self, task_id: &str) -> Result<HardwareSample, String> {
        let command = amicu_telemetry::hardware::probe_command();
        debug_assert_eq!(command[2], PROBE_SCRIPT);
        let run = self.exec_capture(task_id, command, PROBE_TIMEOUT, MAX_PROBE_OUTPUT_BYTES);
        let output = tokio::time::timeout(PROBE_TIMEOUT + EXEC_GRACE, run)
            .await
            .map_err(|_| {
                format!(
                    "hardware probe timed out after {}s",
                    PROBE_TIMEOUT.as_secs()
                )
            })?
            .map_err(|e| e.to_string())?;
        match output.exit_code {
            Some(0) | None if !output.stdout.is_empty() => {
                Ok(parse_probe_output(&String::from_utf8_lossy(&output.stdout)))
            }
            Some(code) if code != 0 => Err(format!("hardware probe exited with status {code}")),
            _ => Err("hardware probe produced no output".to_owned()),
        }
    }

    /// Runs the hardware probe in every running container of `app_id`.
    /// `Ok(vec![])` means the app has no running container (scaled to zero).
    /// Only the container listing can fail the call; each exec's failure is
    /// recorded in its own [`ContainerHardware`].
    pub async fn probe_app_hardware(
        &mut self,
        app_id: &str,
    ) -> Result<Vec<ContainerHardware>, ModalError> {
        let mut results = Vec::new();
        for task_id in self.app_task_ids(app_id).await? {
            let outcome = self.probe_container(&task_id).await;
            results.push(ContainerHardware { task_id, outcome });
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(fd: pb::FileDescriptor, bytes: &[u8]) -> pb::RuntimeOutputMessage {
        pb::RuntimeOutputMessage {
            file_descriptor: fd.into(),
            message: String::new(),
            message_bytes: bytes.to_vec(),
        }
    }

    fn batch(items: Vec<pb::RuntimeOutputMessage>, exit: Option<i32>) -> pb::RuntimeOutputBatch {
        pb::RuntimeOutputBatch {
            items,
            exit_code: exit,
            ..pb::RuntimeOutputBatch::default()
        }
    }

    #[test]
    fn absorbs_stdout_across_batches_until_the_exit_code() {
        let mut out = ExecOutput::default();
        assert!(!out.absorb(
            &batch(vec![item(pb::FileDescriptor::Stdout, b"ab")], None),
            100
        ));
        assert!(!out.absorb(
            &batch(vec![item(pb::FileDescriptor::Stderr, b"IGNORED")], None),
            100
        ));
        assert!(out.absorb(
            &batch(vec![item(pb::FileDescriptor::Stdout, b"cd")], Some(0)),
            100
        ));
        assert_eq!(out.stdout, b"abcd");
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.truncated);
    }

    #[test]
    fn falls_back_to_the_string_message_for_old_servers() {
        let mut out = ExecOutput::default();
        let legacy = pb::RuntimeOutputMessage {
            file_descriptor: pb::FileDescriptor::Stdout.into(),
            message: "legacy".to_owned(),
            message_bytes: Vec::new(),
        };
        out.absorb(&batch(vec![legacy], None), 100);
        assert_eq!(out.stdout, b"legacy");
    }

    #[test]
    fn output_is_capped_and_marked_truncated() {
        let mut out = ExecOutput::default();
        out.absorb(
            &batch(vec![item(pb::FileDescriptor::Stdout, b"0123456789")], None),
            4,
        );
        out.absorb(
            &batch(vec![item(pb::FileDescriptor::Stdout, b"more")], Some(0)),
            4,
        );
        assert_eq!(out.stdout, b"0123");
        assert!(out.truncated);
    }

    #[test]
    fn a_failed_exec_error_text_has_no_token_metadata() {
        const TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.session-jwt-value.sig";
        let mut status = tonic::Status::unimplemented("exec is not supported for this function");
        status
            .metadata_mut()
            .insert("x-modal-auth-token", TOKEN.parse().unwrap());
        let reason = ModalError::from_status("ContainerExec", status).to_string();
        assert!(reason.contains("ContainerExec"), "{reason}");
        assert!(!reason.contains(TOKEN) && !reason.contains("x-modal-auth-token"));
    }
}
