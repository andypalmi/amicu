//! The VM's startup script: runs the vLLM container with Docker and the NVIDIA
//! container toolkit, behind the same auth middleware the RunPod pods load.
//!
//! The script is the `startup-script` metadata value and holds no secret. The
//! API key and the Hugging Face token reach the VM as instance metadata items
//! (`amicu-api-key`, `amicu-hf-token`), which the script copies into a
//! root-only file on tmpfs that Docker reads with `--env-file`.
//!
//! Metadata is readable by anyone who can `get` the instance in the project, so
//! this delivery is temporary: [`secrets_block`] is the one place that changes
//! when the keys move to Secret Manager (the VM's service account then reads
//! them at boot and the metadata items go away).

use amicu_core::secret::SecretValue;
use serde_json::{Value, json};

use crate::error::GcpError;

/// The auth middleware module, the same file the RunPod pods write. It guards
/// every route but `GET /health`.
const AUTH_MODULE: &str = include_str!("amicu_auth.py");
const AUTH_HEREDOC_END: &str = "AMICU_AUTH_EOF";
const MIDDLEWARE: &str = "amicu_auth.RequireKey";

/// Metadata key holding the API key (a secret).
pub const API_KEY_METADATA: &str = "amicu-api-key";
/// Metadata key holding the Hugging Face token (a secret).
pub const HF_TOKEN_METADATA: &str = "amicu-hf-token";
const CONTAINER_NAME: &str = "amicu-vllm";

/// How the VM gets its secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SecretDelivery {
    /// Instance metadata items marked as secrets in the code. Temporary.
    #[default]
    InstanceMetadata,
}

/// What the startup script runs.
#[derive(Debug, Clone)]
pub struct StartupConfig {
    /// The vLLM image, for example `vllm/vllm-openai:v0.11.0`.
    pub container_image: String,
    /// Arguments for the image (`--model ...`), without `--host`, `--port` or
    /// `--middleware`, which the script adds. Contains no secret.
    pub vllm_args: Vec<String>,
    /// The port vLLM binds and Docker publishes.
    pub serving_port: u16,
    /// The bearer key every request but `GET /health` needs.
    pub api_key: SecretValue,
    pub hf_token: Option<SecretValue>,
}

impl StartupConfig {
    /// # Errors
    /// [`GcpError::Invalid`] for an empty or multi-line secret, a bad image
    /// reference or port 0.
    pub fn validate(&self) -> Result<(), GcpError> {
        let bad = |kind, reason| GcpError::Invalid {
            kind,
            value: String::new(),
            reason,
        };
        secret_str(&self.api_key).ok_or_else(|| {
            bad(
                "api key",
                "must be non-empty single-line text without NUL bytes",
            )
        })?;
        if let Some(t) = &self.hf_token {
            secret_str(t).ok_or_else(|| {
                bad(
                    "hugging face token",
                    "must be non-empty single-line text without NUL bytes",
                )
            })?;
        }
        if self.serving_port == 0 {
            return Err(bad("serving port", "must not be 0"));
        }
        let image_ok = !self.container_image.is_empty()
            && self.container_image.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'/' | b':' | b'.' | b'_' | b'-' | b'@')
            });
        if !image_ok {
            return Err(bad("container image", "not a plain image reference"));
        }
        Ok(())
    }
}

fn secret_str(s: &SecretValue) -> Option<&str> {
    let v = s.expose_secret_str().ok()?;
    (!v.is_empty() && !v.contains(['\n', '\r', '\0'])).then_some(v)
}

/// The instance metadata items: the startup script, the secrets, and
/// `install-nvidia-driver` when asked for.
pub(crate) fn startup_metadata(cfg: &StartupConfig, install_driver: bool) -> Vec<Value> {
    let mut items = vec![json!({ "key": "startup-script", "value": startup_script(cfg) })];
    // Secrets (see the module docs): `validate` has already checked them.
    items.push(json!({
        "key": API_KEY_METADATA,
        "value": secret_str(&cfg.api_key).unwrap_or_default(),
    }));
    if let Some(t) = &cfg.hf_token {
        items.push(json!({
            "key": HF_TOKEN_METADATA,
            "value": secret_str(t).unwrap_or_default(),
        }));
    }
    if install_driver {
        items.push(json!({ "key": "install-nvidia-driver", "value": "True" }));
    }
    items
}

/// The script, free of secrets.
#[must_use]
pub fn startup_script(cfg: &StartupConfig) -> String {
    let port = cfg.serving_port;
    let mut run: Vec<String> = [
        "docker",
        "run",
        "-d",
        "--restart",
        "on-failure",
        "--name",
        CONTAINER_NAME,
        "--gpus",
        "all",
        "--ipc=host",
        "--env-file",
        "/run/amicu/vllm.env",
        "-e",
        "PYTHONPATH=/amicu",
        "-v",
        "/opt/amicu:/amicu:ro",
        "-v",
        "/opt/amicu-hf:/root/.cache/huggingface",
        "-p",
    ]
    .map(String::from)
    .to_vec();
    run.push(format!("{port}:{port}"));
    run.push(cfg.container_image.clone());
    run.extend(cfg.vllm_args.iter().cloned());
    run.extend(
        [
            "--host",
            "0.0.0.0",
            "--port",
            &port.to_string(),
            "--middleware",
            MIDDLEWARE,
        ]
        .map(String::from),
    );
    let run = run
        .iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let secrets = secrets_block(SecretDelivery::InstanceMetadata, cfg.hf_token.is_some());
    format!(
        "#!/bin/bash\n\
         set -euo pipefail\n\
         md() {{ curl -fsS -H 'Metadata-Flavor: Google' \
         \"http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1\"; }}\n\
         {secrets}\
         install -d -m 0755 /opt/amicu /opt/amicu-hf\n\
         cat > /opt/amicu/amicu_auth.py <<'{AUTH_HEREDOC_END}'\n\
         {AUTH_MODULE}{AUTH_HEREDOC_END}\n\
         command -v docker >/dev/null || {{ echo 'amicu: docker is not installed' >&2; exit 1; }}\n\
         for _ in $(seq 1 60); do nvidia-smi -L >/dev/null 2>&1 && break; sleep 5; done\n\
         nvidia-smi -L >/dev/null || {{ echo 'amicu: no NVIDIA driver' >&2; exit 1; }}\n\
         docker rm -f {CONTAINER_NAME} >/dev/null 2>&1 || true\n\
         {run} >/dev/null\n\
         sleep 5\n\
         state=\"$(docker inspect -f '{{{{.State.Status}}}}' {CONTAINER_NAME} 2>/dev/null || true)\"\n\
         if [ \"$state\" != running ]; then\n\
         echo \"amicu: container {CONTAINER_NAME} is not running (state: ${{state:-unknown}})\" >&2\n\
         docker logs --tail 50 {CONTAINER_NAME} >&2 || true\n\
         exit 1\n\
         fi\n\
         sleep 15\n\
         state=\"$(docker inspect -f '{{{{.State.Status}}}}' {CONTAINER_NAME} 2>/dev/null || true)\"\n\
         if [ \"$state\" != running ]; then\n\
         echo \"amicu: container {CONTAINER_NAME} stopped or is crash-looping (state: ${{state:-unknown}})\" >&2\n\
         docker logs --tail 50 {CONTAINER_NAME} >&2 || true\n\
         exit 1\n\
         fi\n\
         echo 'amicu: container {CONTAINER_NAME} started' >&2\n"
    )
}

/// The shell that writes `/run/amicu/vllm.env` (tmpfs, root only) from the
/// chosen secret source. Secret Manager support is added as another
/// [`SecretDelivery`] arm here, reading the secrets with the VM's service
/// account; nothing else in the script depends on where they come from.
fn secrets_block(delivery: SecretDelivery, has_hf_token: bool) -> String {
    match delivery {
        SecretDelivery::InstanceMetadata => {
            // Each value is read into a variable first, so a failed fetch stops
            // the script (`set -e`) and an empty one is rejected: a command
            // substitution inside `printf` would hide both.
            let (hf_read, hf) = if has_hf_token {
                (
                    format!(
                        "hf_token=\"$(md {HF_TOKEN_METADATA})\"\n\
                         [ -n \"$hf_token\" ] || {{ echo 'amicu: metadata {HF_TOKEN_METADATA} is empty' >&2; exit 1; }}\n"
                    ),
                    "  printf 'HF_TOKEN=%s\\n' \"$hf_token\"\n",
                )
            } else {
                (String::new(), "")
            };
            format!(
                "api_key=\"$(md {API_KEY_METADATA})\"\n\
                 [ -n \"$api_key\" ] || {{ echo 'amicu: metadata {API_KEY_METADATA} is empty' >&2; exit 1; }}\n\
                 {hf_read}\
                 umask 077\n\
                 install -d -m 0700 /run/amicu\n\
                 {{\n  printf 'VLLM_API_KEY=%s\\n' \"$api_key\"\n{hf}}} > /run/amicu/vllm.env\n\
                 unset api_key hf_token\n\
                 umask 022\n"
            )
        }
    }
}

/// Single-quotes `s` for a POSIX shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StartupConfig {
        StartupConfig {
            container_image: "vllm/vllm-openai:v0.11.0".into(),
            vllm_args: vec!["--model".into(), "a b'c".into()],
            serving_port: 8000,
            api_key: SecretValue::new("sk-secret"),
            hf_token: Some(SecretValue::new("hf_secret")),
        }
    }

    #[test]
    fn script_runs_the_container_behind_the_middleware_without_secrets() {
        let s = startup_script(&cfg());
        assert!(s.starts_with("#!/bin/bash\n"));
        assert!(!s.contains("sk-secret") && !s.contains("hf_secret"));
        assert!(s.contains("'--gpus' 'all'"));
        assert!(s.contains("'-p' '8000:8000'"));
        assert!(s.contains("'--model' 'a b'\\''c'"));
        assert!(
            s.contains("'--host' '0.0.0.0' '--port' '8000' '--middleware' 'amicu_auth.RequireKey'")
        );
        assert!(s.contains("class RequireKey"));
        assert!(s.contains("md amicu-api-key") && s.contains("md amicu-hf-token"));
    }

    #[test]
    fn empty_metadata_values_fail_the_script() {
        let s = startup_script(&cfg());
        assert!(s.contains("api_key=\"$(md amicu-api-key)\""));
        assert!(s.contains("[ -n \"$api_key\" ] ||"));
        assert!(s.contains("[ -n \"$hf_token\" ] ||"));
        // No command substitution is left to hide a failure inside printf.
        assert!(!s.contains("printf 'VLLM_API_KEY=%s\\n' \"$(md"));
        let mut c = cfg();
        c.hf_token = None;
        assert!(!startup_script(&c).contains("$hf_token"));
    }

    #[test]
    fn the_container_restarts_on_failure_and_is_verified_running() {
        let s = startup_script(&cfg());
        assert!(s.contains("'--restart' 'on-failure'"));
        assert!(!s.contains("exec 'docker'"));
        assert!(s.contains("docker inspect -f"));
        assert!(s.contains("docker logs --tail 50 amicu-vllm"));
        // `restarting` is a crash loop, not a pass: only `running` is accepted,
        // checked twice, 15 s apart, with the container logs on failure.
        assert!(!s.contains("restarting"));
        assert_eq!(s.matches("[ \"$state\" != running ]").count(), 2);
        assert_eq!(
            s.matches("docker logs --tail 50 amicu-vllm >&2 || true")
                .count(),
            2
        );
        assert!(s.contains("sleep 15\n"));
    }

    #[test]
    fn hf_token_line_is_absent_without_a_token() {
        let mut c = cfg();
        c.hf_token = None;
        assert!(!startup_script(&c).contains("HF_TOKEN"));
        assert!(
            startup_metadata(&c, false)
                .iter()
                .all(|i| i["key"] != HF_TOKEN_METADATA)
        );
    }

    #[test]
    fn secrets_must_be_single_line() {
        let mut c = cfg();
        c.api_key = SecretValue::new("a\nb");
        assert!(c.validate().is_err());
        c.api_key = SecretValue::new("");
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.container_image = "x; rm -rf /".into();
        assert!(c.validate().is_err());
    }
}
