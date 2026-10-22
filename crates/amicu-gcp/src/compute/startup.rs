//! The VM's startup script: runs the vLLM container with Docker and the NVIDIA
//! container toolkit, behind the same auth middleware the RunPod pods load.
//!
//! The script is the `startup-script` metadata value and holds no secret. With
//! [`SecretDelivery::SecretManager`] (the default) the VM's own service account
//! reads the API key, the Hugging Face token and the TLS private key from
//! Secret Manager at boot (metadata-server token, then the REST API with `curl`)
//! and writes them to a root-only file on tmpfs that Docker reads with
//! `--env-file`; nothing secret is in instance metadata or on disk.
//!
//! [`SecretDelivery::InstanceMetadata`] is the insecure fallback: the secrets
//! are instance metadata items (`amicu-api-key`, `amicu-hf-token`,
//! `amicu-tls-key`), readable by anyone who can `get` the instance in the
//! project and visible in the console. Use it only when Secret Manager cannot
//! be used.
//!
//! With a [`TlsIdentity`] vLLM serves HTTPS (`--ssl-certfile`, `--ssl-keyfile`)
//! from `/run/amicu/tls`. The certificate is public and always instance metadata.

use amicu_core::secret::SecretValue;
use serde_json::{Value, json};

use super::tls::TlsIdentity;
use crate::error::GcpError;
use crate::secrets::{SECRETMANAGER_BASE_URL, SecretKind, secret_id};

/// The auth middleware module, the same file the RunPod pods write. It guards
/// every route but `GET /health`.
const AUTH_MODULE: &str = include_str!("amicu_auth.py");
const AUTH_HEREDOC_END: &str = "AMICU_AUTH_EOF";
const MIDDLEWARE: &str = "amicu_auth.RequireKey";

/// Metadata key holding the API key (a secret; [`SecretDelivery::InstanceMetadata`] only).
pub const API_KEY_METADATA: &str = "amicu-api-key";
/// Metadata key holding the Hugging Face token (a secret; metadata delivery only).
pub const HF_TOKEN_METADATA: &str = "amicu-hf-token";
/// Metadata key holding the TLS private key (a secret; metadata delivery only).
pub const TLS_KEY_METADATA: &str = "amicu-tls-key";
/// Metadata key holding the TLS certificate (public, in both deliveries).
pub const TLS_CERT_METADATA: &str = "amicu-tls-cert";
const CONTAINER_NAME: &str = "amicu-vllm";

/// How the VM gets its secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SecretDelivery {
    /// The VM's service account reads them from Secret Manager at boot. The
    /// spec must name that service account, and the caller must have created
    /// the secrets and granted access ([`crate::compute::provision_secrets`]).
    #[default]
    SecretManager,
    /// Instance metadata items. INSECURE: anyone who can read the instance can
    /// read the secrets. An explicit opt-in fallback.
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
    /// Where the secrets come from; see [`SecretDelivery`].
    pub delivery: SecretDelivery,
    /// Serve HTTPS with this certificate and key; `None` serves plain HTTP.
    pub tls: Option<TlsIdentity>,
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
        if let Some(t) = &self.tls {
            t.validate()?;
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

/// The instance metadata items: the startup script, the public certificate, the
/// secrets under [`SecretDelivery::InstanceMetadata`] only, and
/// `install-nvidia-driver` when asked for.
pub(crate) fn startup_metadata(
    cfg: &StartupConfig,
    project: &str,
    instance: &str,
    install_driver: bool,
) -> Vec<Value> {
    let mut items = vec![json!({
        "key": "startup-script",
        "value": startup_script(cfg, project, instance),
    })];
    if let Some(t) = &cfg.tls {
        items.push(json!({ "key": TLS_CERT_METADATA, "value": t.cert_pem }));
    }
    if cfg.delivery == SecretDelivery::InstanceMetadata {
        // Insecure fallback (see the module docs): `validate` has checked them.
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
        if let Some(t) = &cfg.tls {
            items.push(json!({
                "key": TLS_KEY_METADATA,
                "value": t.key_pem.expose_secret_str().unwrap_or_default(),
            }));
        }
    }
    if install_driver {
        items.push(json!({ "key": "install-nvidia-driver", "value": "True" }));
    }
    items
}

/// The script, free of secrets. `project` and `instance` name the secrets the
/// VM reads under [`SecretDelivery::SecretManager`].
#[must_use]
pub fn startup_script(cfg: &StartupConfig, project: &str, instance: &str) -> String {
    let port = cfg.serving_port;
    let mut run: Vec<String> = [
        "docker",
        "run",
        "-d",
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
    ]
    .map(String::from)
    .to_vec();
    if cfg.tls.is_some() {
        run.extend(["-v", "/run/amicu/tls:/amicu-tls:ro"].map(String::from));
    }
    run.push("-p".into());
    run.push(format!("{port}:{port}"));
    run.push(cfg.container_image.clone());
    run.extend(cfg.vllm_args.iter().cloned());
    if cfg.tls.is_some() {
        run.extend(
            [
                "--ssl-certfile",
                "/amicu-tls/tls.crt",
                "--ssl-keyfile",
                "/amicu-tls/tls.key",
            ]
            .map(String::from),
        );
    }
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
    let secrets = secrets_block(cfg, project, instance);
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
         exec {run}\n"
    )
}

/// The shell that writes `/run/amicu/vllm.env` and the TLS files (tmpfs, root
/// only) from the chosen secret source. Nothing else in the script depends on
/// where the secrets come from.
fn secrets_block(cfg: &StartupConfig, project: &str, instance: &str) -> String {
    let (prelude, get) = match cfg.delivery {
        SecretDelivery::InstanceMetadata => (String::new(), Getter::Metadata),
        SecretDelivery::SecretManager => (sm_functions(project), Getter::SecretManager),
    };
    let fetch = |meta_key: &str, kind: SecretKind, var: &str| {
        let id = secret_id(instance, kind).unwrap_or_default();
        match get {
            Getter::Metadata => format!(
                "{var}=$(md {meta_key})
"
            ),
            Getter::SecretManager => format!(
                "{var}=$(sm {id})
"
            ),
        }
    };
    let mut out = format!(
        "{prelude}umask 077\n\
         install -d -m 0700 /run/amicu\n"
    );
    out.push_str(&fetch(API_KEY_METADATA, SecretKind::ApiKey, "api_key"));
    out.push_str("[ -n \"$api_key\" ] || { echo 'amicu: empty api key' >&2; exit 1; }\n");
    if cfg.hf_token.is_some() {
        out.push_str(&fetch(HF_TOKEN_METADATA, SecretKind::HfToken, "hf_token"));
    }
    out.push_str("{\n  printf 'VLLM_API_KEY=%s\\n' \"$api_key\"\n");
    if cfg.hf_token.is_some() {
        out.push_str("  printf 'HF_TOKEN=%s\\n' \"$hf_token\"\n");
    }
    out.push_str("} > /run/amicu/vllm.env\n");
    if cfg.tls.is_some() {
        out.push_str("install -d -m 0700 /run/amicu/tls\n");
        out.push_str(&fetch(TLS_KEY_METADATA, SecretKind::TlsKey, "tls_key"));
        out.push_str("printf '%s\\n' \"$tls_key\" > /run/amicu/tls/tls.key\n");
        out.push_str("md ");
        out.push_str(TLS_CERT_METADATA);
        out.push_str(" > /run/amicu/tls/tls.crt\n");
    }
    out.push_str("unset api_key hf_token tls_key\numask 022\n");
    out
}

#[derive(Clone, Copy)]
enum Getter {
    Metadata,
    SecretManager,
}

/// `sm ID` prints the latest version of secret `ID`: a token from the metadata
/// server (the VM's service account), then the Secret Manager REST API. The
/// token goes to `curl` on stdin (`-K -`), never in an argument list. Retries
/// for a few minutes, since IAM changes and the metadata server can lag at boot.
/// Needs `curl` and `python3`, which the Deep Learning VM images ship.
fn sm_functions(project: &str) -> String {
    format!(
        "tok() {{ curl -fsS -H 'Metadata-Flavor: Google' \
         'http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token' \
         | python3 -c 'import json,sys;print(json.load(sys.stdin)[\"access_token\"])'; }}\n\
         sm() {{ local i out; for i in $(seq 1 30); do \
         out=$(printf 'header = \"Authorization: Bearer %s\"\\n' \"$(tok)\" \
         | curl -fsS -K - '{SECRETMANAGER_BASE_URL}/projects/{project}/secrets/'\"$1\"'/versions/latest:access' \
         | python3 -c 'import base64,json,sys;sys.stdout.buffer.write(base64.b64decode(json.load(sys.stdin)[\"payload\"][\"data\"]))') \
         && {{ printf '%s' \"$out\"; return 0; }}; sleep 10; done; \
         echo \"amicu: could not read secret $1\" >&2; return 1; }}\n"
    )
}

/// Single-quotes `s` for a POSIX shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "my-proj";
    const N: &str = "vm-1";

    fn cfg() -> StartupConfig {
        StartupConfig {
            container_image: "vllm/vllm-openai:v0.11.0".into(),
            vllm_args: vec!["--model".into(), "a b'c".into()],
            serving_port: 8000,
            api_key: SecretValue::new("sk-secret"),
            hf_token: Some(SecretValue::new("hf_secret")),
            delivery: SecretDelivery::SecretManager,
            tls: None,
        }
    }

    fn tls_cfg(delivery: SecretDelivery) -> StartupConfig {
        StartupConfig {
            delivery,
            tls: Some(TlsIdentity::generate(N).unwrap()),
            ..cfg()
        }
    }

    fn script(c: &StartupConfig) -> String {
        startup_script(c, P, N)
    }

    fn metadata_text(c: &StartupConfig) -> String {
        serde_json::to_string(&startup_metadata(c, P, N, false)).unwrap()
    }

    #[test]
    fn secret_manager_is_the_default_delivery() {
        assert_eq!(SecretDelivery::default(), SecretDelivery::SecretManager);
    }

    #[test]
    fn script_runs_the_container_behind_the_middleware_without_secrets() {
        let s = script(&cfg());
        assert!(s.starts_with("#!/bin/bash\n"));
        assert!(!s.contains("sk-secret") && !s.contains("hf_secret"));
        assert!(s.contains("'--gpus' 'all'"));
        assert!(s.contains("'-p' '8000:8000'"));
        assert!(s.contains("'--model' 'a b'\\''c'"));
        assert!(
            s.contains("'--host' '0.0.0.0' '--port' '8000' '--middleware' 'amicu_auth.RequireKey'")
        );
        assert!(s.contains("class RequireKey"));
        assert!(!s.contains("--ssl-"));
    }

    #[test]
    fn secret_manager_delivery_reads_secrets_at_boot_and_leaves_none_in_metadata() {
        let c = tls_cfg(SecretDelivery::SecretManager);
        let s = script(&c);
        assert!(s.contains("sm my-proj") || s.contains("projects/my-proj/secrets/"));
        assert!(s.contains("secretmanager.googleapis.com/v1/projects/my-proj/secrets/"));
        assert!(s.contains("service-accounts/default/token"));
        assert!(s.contains("sm vm-1-api-key") && s.contains("sm vm-1-hf-token"));
        assert!(s.contains("sm vm-1-tls-key"));
        // The token goes to curl on stdin, not in an argument.
        assert!(s.contains("curl -fsS -K -"));
        assert!(!s.contains("md amicu-api-key") && !s.contains("md amicu-hf-token"));
        let key = c.tls.as_ref().unwrap().key_pem.expose_secret_str().unwrap();
        let body = key.lines().nth(1).unwrap();
        let meta = metadata_text(&c);
        for secret in ["sk-secret", "hf_secret", body, "PRIVATE KEY"] {
            assert!(!s.contains(secret), "script has {secret}");
            assert!(!meta.contains(secret), "metadata has {secret}");
        }
        assert!(meta.contains(TLS_CERT_METADATA) && meta.contains("BEGIN CERTIFICATE"));
        assert!(!meta.contains(API_KEY_METADATA) && !meta.contains(TLS_KEY_METADATA));
    }

    #[test]
    fn metadata_delivery_is_the_only_one_with_secrets_in_metadata() {
        let c = tls_cfg(SecretDelivery::InstanceMetadata);
        let s = script(&c);
        assert!(!s.contains("sk-secret") && !s.contains("secretmanager"));
        assert!(s.contains("md amicu-api-key") && s.contains("md amicu-hf-token"));
        assert!(s.contains("md amicu-tls-key"));
        let meta = metadata_text(&c);
        assert!(meta.contains("sk-secret") && meta.contains("hf_secret"));
        assert!(meta.contains(TLS_KEY_METADATA));
    }

    #[test]
    fn tls_adds_the_ssl_flags_and_a_read_only_mount() {
        let s = script(&tls_cfg(SecretDelivery::SecretManager));
        assert!(s.contains("'-v' '/run/amicu/tls:/amicu-tls:ro'"));
        assert!(s.contains(
            "'--ssl-certfile' '/amicu-tls/tls.crt' '--ssl-keyfile' '/amicu-tls/tls.key'"
        ));
        assert!(s.contains("md amicu-tls-cert > /run/amicu/tls/tls.crt"));
    }

    #[test]
    fn hf_token_line_is_absent_without_a_token() {
        for delivery in [
            SecretDelivery::SecretManager,
            SecretDelivery::InstanceMetadata,
        ] {
            let mut c = cfg();
            c.delivery = delivery;
            c.hf_token = None;
            assert!(!script(&c).contains("HF_TOKEN"));
            assert!(!script(&c).contains("hf-token"));
            assert!(
                startup_metadata(&c, P, N, false)
                    .iter()
                    .all(|i| i["key"] != HF_TOKEN_METADATA)
            );
        }
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

    #[test]
    fn every_script_variant_is_valid_bash() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for delivery in [
            SecretDelivery::SecretManager,
            SecretDelivery::InstanceMetadata,
        ] {
            for tls in [false, true] {
                let mut c = if tls { tls_cfg(delivery) } else { cfg() };
                c.delivery = delivery;
                let Ok(mut child) = Command::new("bash").arg("-n").stdin(Stdio::piped()).spawn()
                else {
                    return;
                };
                let mut stdin = child.stdin.take().unwrap();
                stdin.write_all(script(&c).as_bytes()).unwrap();
                drop(stdin);
                assert!(child.wait().unwrap().success(), "{delivery:?} tls={tls}");
            }
        }
    }
}
