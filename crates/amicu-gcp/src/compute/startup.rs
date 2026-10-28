//! The VM's startup script: runs the vLLM container with Docker and the NVIDIA
//! container toolkit, behind the same auth middleware the RunPod pods load.
//! The Deep Learning VM images ship the NVIDIA driver but neither Docker nor
//! the toolkit, so the script installs whichever is missing from the official
//! apt repositories: Docker Engine from Docker's and `nvidia-container-toolkit`
//! from NVIDIA's, each signing key checked against a pinned fingerprint
//! ([`DOCKER_KEY_FINGERPRINT`], [`NVIDIA_KEY_FINGERPRINT`]) before apt trusts it.
//!
//! The script reports its outcome as guest attributes (namespace
//! [`GUEST_NAMESPACE`]): `status` is `booting` (with the kernel's `boot_id`)
//! from the start of each boot, then `ready` once vLLM answers `/health` on the
//! VM, or `failed:<step>` with the tail of the script's log in `detail`. The
//! readiness wait reads them to stop at once on a failed boot.
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

use std::time::Duration;

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
/// Metadata key that turns on guest attributes, where the script reports.
pub const GUEST_ATTRIBUTES_METADATA: &str = "enable-guest-attributes";
/// The guest attribute namespace the script writes `status` and `detail` in.
pub const GUEST_NAMESPACE: &str = "amicu";
/// Fingerprint of Docker's apt repository key (`Docker Release (CE deb)`).
pub const DOCKER_KEY_FINGERPRINT: &str = "9DC858229FC7DD38854AE2D88D81803C0EBFCD88";
const DOCKER_KEY_URL: &str = "https://download.docker.com/linux/ubuntu/gpg";
/// Fingerprint of NVIDIA's container toolkit repository key
/// (`NVIDIA CORPORATION (Open Source Projects)`).
pub const NVIDIA_KEY_FINGERPRINT: &str = "C95B321B61E88C1809C4F759DDCAE044F796ECB0";
const NVIDIA_KEY_URL: &str = "https://nvidia.github.io/libnvidia-container/gpgkey";
/// Written once Docker and the NVIDIA runtime are installed and configured.
const TOOLS_MARKER: &str = "/var/lib/amicu/tools-ready";
/// Container restarts that count as a crash loop.
const MAX_RESTARTS: u32 = 3;
const CONTAINER_NAME: &str = "amicu-vllm";
const IDLE_HEREDOC_END: &str = "AMICU_IDLE_EOF";
const IDLE_UNIT: &str = "amicu-idle-watchdog";

/// Polls vLLM's `/metrics` with the API key (read from the root-only env file)
/// and powers the VM off once nothing has run, waited or finished for `WINDOW`
/// seconds. The clock starts at the first good answer, so model loading never
/// counts; after that, a failed or garbled answer counts as idle (a dead vLLM
/// serves nothing).
const IDLE_WATCHDOG: &str = r#"#!/bin/bash
url='URL'
window=WINDOW
key=$(sed -n 's/^VLLM_API_KEY=//p' /run/amicu/vllm.env)
last_done=""
last_active=""
while sleep 30; do
  now=$(date +%s)
  ok=0
  m=$(printf 'header = "Authorization: Bearer %s"\n' "$key" | curl -ksS -m 10 -K - "$url") && printf '%s\n' "$m" | grep -q '^vllm:' && ok=1
  if [ "$ok" = 1 ]; then
    busy=$(printf '%s\n' "$m" | awk '/^vllm:num_requests_(running|waiting)[{ ]/ {s+=$NF} END {print s+0}')
    finished=$(printf '%s\n' "$m" | awk '/^vllm:request_success_total[{ ]/ {s+=$NF} END {print s+0}')
    if [ -z "$last_active" ] || [ "$busy" != 0 ] || [ "$finished" != "$last_done" ]; then last_active=$now; fi
    last_done=$finished
  fi
  if [ -n "$last_active" ] && [ $((now - last_active)) -ge "$window" ]; then shutdown -h now; exit 0; fi
done
"#;

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
    /// Power the VM off after this long without a request; `None` keeps it
    /// until `max_run_duration`.
    pub idle_shutdown: Option<Duration>,
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
        if self.idle_shutdown.is_some_and(|d| d.as_secs() == 0) {
            return Err(bad("idle shutdown", "must be at least one second"));
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
    let mut items = vec![
        json!({
            "key": "startup-script",
            "value": startup_script(cfg, project, instance),
        }),
        json!({ "key": GUEST_ATTRIBUTES_METADATA, "value": "TRUE" }),
    ];
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
    let idle = idle_block(cfg);
    let tools = tools_block();
    let wait = wait_block(cfg);
    let image = sh_quote(&cfg.container_image);
    format!(
        "#!/bin/bash\n\
         set -euo pipefail\n\
         install -d -m 0700 /run/amicu\n\
         exec 2> >(tee -a /run/amicu/startup.log >&2)\n\
         ga() {{ curl -fsS -m 10 -X PUT -H 'Metadata-Flavor: Google' --data-binary @- \
         \"http://metadata.google.internal/computeMetadata/v1/instance/guest-attributes/{GUEST_NAMESPACE}/$1\" \
         >/dev/null 2>&1 || true; }}\n\
         printf booting | ga status\n\
         ga boot_id < /proc/sys/kernel/random/boot_id\n\
         step='read secrets'\n\
         on_exit() {{ rc=$?; if [ \"$rc\" != 0 ]; then sleep 1; \
         tail -c 4000 /run/amicu/startup.log | ga detail; printf 'failed:%s' \"$step\" | ga status; fi; }}\n\
         trap on_exit EXIT\n\
         md() {{ curl -fsS -H 'Metadata-Flavor: Google' \
         \"http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1\"; }}\n\
         {secrets}\
         install -d -m 0755 /opt/amicu /opt/amicu-hf\n\
         cat > /opt/amicu/amicu_auth.py <<'{AUTH_HEREDOC_END}'\n\
         {AUTH_MODULE}{AUTH_HEREDOC_END}\n\
         step='wait for the NVIDIA driver'\n\
         for _ in $(seq 1 60); do nvidia-smi -L >/dev/null 2>&1 && break; sleep 5; done\n\
         nvidia-smi -L >/dev/null || {{ echo 'amicu: no NVIDIA driver' >&2; exit 1; }}\n\
         {tools}\
         step='pull the vLLM image'\n\
         for i in 1 2 3; do docker pull -q {image} >/dev/null && break; [ \"$i\" = 3 ] && exit 1; sleep 10; done\n\
         step='start the vLLM container'\n\
         docker rm -f {CONTAINER_NAME} >/dev/null 2>&1 || true\n\
         {run} >/dev/null\n\
         {idle}\
         {wait}"
    )
}

/// Waits on the VM for vLLM's `/health`, then reports `ready`. It fails only on
/// a stopped container or a crash loop: a slow model load is left to apply's
/// own deadline, which knows how long it is willing to wait.
fn wait_block(cfg: &StartupConfig) -> String {
    let scheme = if cfg.tls.is_some() { "https" } else { "http" };
    let port = cfg.serving_port;
    format!(
        "step='wait for vLLM to load the model'\n\
         bad=0\n\
         until curl -ksf -m 5 -o /dev/null '{scheme}://127.0.0.1:{port}/health'; do\n\
         read -r cs restarts <<< \"$(docker inspect -f '{{{{.State.Status}}}} {{{{.RestartCount}}}}' {CONTAINER_NAME} 2>/dev/null || echo missing 0)\"\n\
         case \"$cs\" in exited|dead|missing) bad=$((bad + 1)) ;; *) bad=0 ;; esac\n\
         if [ \"$bad\" -ge 2 ] || [ \"$restarts\" -ge {MAX_RESTARTS} ]; then\n\
         echo \"amicu: vLLM is not serving (container {CONTAINER_NAME}: $cs, $restarts restarts)\" >&2\n\
         docker logs --tail 50 {CONTAINER_NAME} >&2 || true\n\
         exit 1\n\
         fi\n\
         sleep 5\n\
         done\n\
         printf ready | ga status\n\
         echo 'amicu: vLLM is ready' >&2\n"
    )
}

/// Installs Docker Engine and the NVIDIA container toolkit from their
/// official apt repositories when either is missing, then points Docker at the
/// NVIDIA runtime. Skipped only when the marker from an earlier boot is there,
/// Docker runs and lists the `nvidia` runtime; otherwise an install that a
/// reboot or preemption cut short is finished first (`dpkg --configure -a`).
///
/// `trust` imports the downloaded key file into a scratch keyring and exports
/// only the key with the pinned fingerprint, so apt trusts exactly that key.
/// `apt-get update` may fail on an unrelated repository of the image: only a
/// candidate for each needed package is required. The script runs with
/// `pipefail`, so no pipeline here ends in a reader that exits early (`grep -q`,
/// `awk ... exit`): the writer's SIGPIPE would fail the pipeline.
fn tools_block() -> String {
    format!(
        "step='install Docker and the NVIDIA container toolkit'\n\
         nvidia_runtime() {{ docker info --format '{{{{json .Runtimes}}}}' 2>/dev/null | grep '\"nvidia\"' >/dev/null; }}\n\
         systemctl start docker >/dev/null 2>&1 || true\n\
         if [ ! -f {TOOLS_MARKER} ] || ! systemctl is-active --quiet docker || ! nvidia_runtime; then\n\
         echo 'amicu: installing Docker and the NVIDIA container toolkit' >&2\n\
         export DEBIAN_FRONTEND=noninteractive GNUPGHOME=/run/amicu/gnupg\n\
         install -d -m 0700 \"$GNUPGHOME\"\n\
         install -d -m 0755 /etc/apt/keyrings\n\
         log=/run/amicu/startup.log\n\
         aptq() {{ apt-get -qq -o DPkg::Lock::Timeout=600 -o Acquire::Retries=5 \"$@\" >>\"$log\" 2>&1; }}\n\
         for i in $(seq 1 60); do dpkg --configure -a >>\"$log\" 2>&1 && break; [ \"$i\" = 60 ] && exit 1; sleep 10; done\n\
         trust() {{ curl -fsSL --retry 5 --retry-connrefused \"$2\" -o \"/run/amicu/$1.asc\"; \
         gpg --batch --quiet --import \"/run/amicu/$1.asc\" >>\"$log\" 2>&1; \
         gpg --batch --export \"$3\" > \"/run/amicu/$1.gpg\" 2>>\"$log\"; \
         keys=$(gpg --show-keys --with-colons \"/run/amicu/$1.gpg\" 2>/dev/null | grep -c '^pub:' || true); \
         fpr=$(gpg --show-keys --with-colons \"/run/amicu/$1.gpg\" 2>/dev/null | awk -F: '$1 == \"fpr\" && !n++ {{ print $10 }}'); \
         [ \"$keys\" = 1 ] && [ \"$fpr\" = \"$3\" ] || {{ echo \"amicu: the $1 repository key is not $3\" >&2; exit 1; }}; \
         install -m 0644 \"/run/amicu/$1.gpg\" \"/etc/apt/keyrings/$1.gpg\"; \
         printf 'deb [arch=%s signed-by=/etc/apt/keyrings/%s.gpg] %s\\n' \"$(dpkg --print-architecture)\" \"$1\" \"$4\" \
         > \"/etc/apt/sources.list.d/$1.list\"; }}\n\
         pkgs=''\n\
         if ! command -v docker >/dev/null 2>&1; then\n\
         . /etc/os-release\n\
         trust docker '{DOCKER_KEY_URL}' {DOCKER_KEY_FINGERPRINT} \
         \"https://download.docker.com/linux/ubuntu ${{UBUNTU_CODENAME:-$VERSION_CODENAME}} stable\"\n\
         pkgs='docker-ce docker-ce-cli containerd.io'\n\
         fi\n\
         if ! command -v nvidia-ctk >/dev/null 2>&1; then\n\
         trust nvidia-container-toolkit '{NVIDIA_KEY_URL}' {NVIDIA_KEY_FINGERPRINT} \
         'https://nvidia.github.io/libnvidia-container/stable/deb/$(ARCH) /'\n\
         pkgs=\"$pkgs nvidia-container-toolkit\"\n\
         fi\n\
         if [ -n \"$pkgs\" ]; then\n\
         for i in 1 2 3; do\n\
         apt-get -o DPkg::Lock::Timeout=600 -o Acquire::Retries=5 update > /run/amicu/apt-update.log 2>&1 \
         || echo 'amicu: apt-get update reported errors; checking the needed packages' >&2\n\
         missing=''\n\
         for p in $pkgs; do\n\
         c=$(apt-cache policy \"$p\" 2>/dev/null | sed -n 's/^ *Candidate: //p')\n\
         [ -n \"$c\" ] && [ \"$c\" != '(none)' ] || missing=\"$missing $p\"\n\
         done\n\
         [ -z \"$missing\" ] && break\n\
         if [ \"$i\" = 3 ]; then\n\
         grep -E '^(E|Err):|download\\.docker\\.com|nvidia\\.github\\.io' /run/amicu/apt-update.log | grep -v '^Hit:' >&2 || true\n\
         echo \"amicu: apt cannot find$missing\" >&2\n\
         exit 1\n\
         fi\n
         sleep 10\n\
         done\n\
         aptq install -y --no-install-recommends $pkgs\n\
         fi\n\
         nvidia-ctk runtime configure --runtime=docker >/dev/null\n\
         systemctl restart docker\n\
         nvidia_runtime || {{ echo 'amicu: Docker does not list the nvidia runtime' >&2; exit 1; }}\n\
         install -D -m 0644 /dev/null {TOOLS_MARKER}\n\
         fi\n"
    )
}

/// Installs and starts the idle watchdog as a transient systemd unit (a plain
/// background process would die with the startup script's cgroup).
fn idle_block(cfg: &StartupConfig) -> String {
    let Some(window) = cfg.idle_shutdown else {
        return String::new();
    };
    let scheme = if cfg.tls.is_some() { "https" } else { "http" };
    let script = IDLE_WATCHDOG
        .replace(
            "URL",
            &format!("{scheme}://127.0.0.1:{}/metrics", cfg.serving_port),
        )
        .replace("WINDOW", &window.as_secs().to_string());
    format!(
        "cat > /opt/amicu/idle-watchdog.sh <<'{IDLE_HEREDOC_END}'\n\
         {script}{IDLE_HEREDOC_END}\n\
         systemctl stop {IDLE_UNIT} >/dev/null 2>&1 || true\n\
         systemd-run --unit={IDLE_UNIT} --collect /bin/bash /opt/amicu/idle-watchdog.sh >/dev/null\n"
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
    out.push_str(&non_empty("api_key", "api key"));
    if cfg.hf_token.is_some() {
        out.push_str(&fetch(HF_TOKEN_METADATA, SecretKind::HfToken, "hf_token"));
        out.push_str(&non_empty("hf_token", "hugging face token"));
    }
    out.push_str("{\n  printf 'VLLM_API_KEY=%s\\n' \"$api_key\"\n");
    if cfg.hf_token.is_some() {
        out.push_str("  printf 'HF_TOKEN=%s\\n' \"$hf_token\"\n");
    }
    out.push_str("} > /run/amicu/vllm.env\n");
    if cfg.tls.is_some() {
        out.push_str("install -d -m 0700 /run/amicu/tls\n");
        out.push_str(&fetch(TLS_KEY_METADATA, SecretKind::TlsKey, "tls_key"));
        out.push_str(&non_empty("tls_key", "tls key"));
        out.push_str("printf '%s\\n' \"$tls_key\" > /run/amicu/tls/tls.key\n");
        out.push_str("md ");
        out.push_str(TLS_CERT_METADATA);
        out.push_str(" > /run/amicu/tls/tls.crt\n");
        out.push_str(
            "[ -s /run/amicu/tls/tls.crt ] || { echo 'amicu: empty tls certificate' >&2; exit 1; }\n",
        );
    }
    out.push_str("unset api_key hf_token tls_key\numask 022\n");
    out
}

/// Shell that stops the script when variable `var` came back empty. A failed
/// fetch already stops it (`set -e` applies to the plain assignment); this
/// catches a fetch that succeeds with nothing.
fn non_empty(var: &str, what: &str) -> String {
    format!("[ -n \"${var}\" ] || {{ echo 'amicu: empty {what}' >&2; exit 1; }}\n")
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
            idle_shutdown: None,
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
    fn the_watchdog_is_only_in_the_script_when_idle_shutdown_is_set() {
        assert!(!script(&cfg()).contains("idle-watchdog"));
        let c = StartupConfig {
            idle_shutdown: Some(Duration::from_mins(30)),
            ..cfg()
        };
        let s = script(&c);
        assert!(s.contains("window=1800\n"));
        assert!(s.contains("url='http://127.0.0.1:8000/metrics'"));
        assert!(s.contains("shutdown -h now"));
        assert!(s.contains("systemd-run --unit=amicu-idle-watchdog"));
        assert!(!s.contains("sk-secret"));
        let c = StartupConfig {
            idle_shutdown: Some(Duration::from_mins(1)),
            ..tls_cfg(SecretDelivery::SecretManager)
        };
        assert!(script(&c).contains("url='https://127.0.0.1:8000/metrics'"));
        let zero = StartupConfig {
            idle_shutdown: Some(Duration::ZERO),
            ..cfg()
        };
        assert!(zero.validate().is_err());
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
    fn empty_secret_values_fail_the_script() {
        for delivery in [
            SecretDelivery::SecretManager,
            SecretDelivery::InstanceMetadata,
        ] {
            let s = script(&tls_cfg(delivery));
            for what in [
                "api key",
                "hugging face token",
                "tls key",
                "tls certificate",
            ] {
                assert!(s.contains(&format!("echo 'amicu: empty {what}'")), "{what}");
            }
            // Values are read by plain assignment, so a failed fetch stops
            // the script too, and none is hidden inside a printf.
            assert!(s.contains("api_key=$("));
            assert!(!s.contains("printf 'VLLM_API_KEY=%s\\n' \"$("));
        }
    }

    #[test]
    fn the_script_waits_for_vllm_until_it_answers_or_the_container_fails() {
        let s = script(&cfg());
        assert!(s.contains("'--restart' 'on-failure'"));
        assert!(s.contains("until curl -ksf -m 5 -o /dev/null 'http://127.0.0.1:8000/health'; do"));
        assert!(s.contains("{{.State.Status}} {{.RestartCount}}"));
        assert!(s.contains("if [ \"$bad\" -ge 2 ] || [ \"$restarts\" -ge 3 ]; then"));
        assert!(!s.contains("seq 1 360"));
        assert_eq!(
            s.matches("docker logs --tail 50 amicu-vllm >&2 || true")
                .count(),
            1
        );
        assert!(
            script(&tls_cfg(SecretDelivery::SecretManager))
                .contains("'https://127.0.0.1:8000/health'")
        );
        let pull = s.find("docker pull -q 'vllm/vllm-openai:v0.11.0'").unwrap();
        assert!(pull < s.find("'docker' 'run'").unwrap());
    }

    #[test]
    fn docker_and_the_toolkit_are_installed_from_pinned_repositories_and_repaired() {
        let s = script(&cfg());
        assert!(s.contains(
            "if [ ! -f /var/lib/amicu/tools-ready ] || ! systemctl is-active --quiet docker || \
             ! nvidia_runtime; then"
        ));
        assert!(s.contains("docker info --format '{{json .Runtimes}}'"));
        assert!(s.contains("trust docker 'https://download.docker.com/linux/ubuntu/gpg' 9DC858229FC7DD38854AE2D88D81803C0EBFCD88 \"https://download.docker.com/linux/ubuntu ${UBUNTU_CODENAME:-$VERSION_CODENAME} stable\"\n"));
        assert!(s.contains("pkgs='docker-ce docker-ce-cli containerd.io'"));
        assert!(s.contains("trust nvidia-container-toolkit 'https://nvidia.github.io/libnvidia-container/gpgkey' C95B321B61E88C1809C4F759DDCAE044F796ECB0 'https://nvidia.github.io/libnvidia-container/stable/deb/$(ARCH) /'\n"));
        assert!(s.contains("pkgs=\"$pkgs nvidia-container-toolkit\""));
        assert!(!s.contains("docker.io"));

        let repair = s.find("dpkg --configure -a").unwrap();
        let install = s
            .find("aptq install -y --no-install-recommends $pkgs")
            .unwrap();
        assert!(repair < install);
        assert!(s.contains("DPkg::Lock::Timeout=600"));

        assert!(s.contains("curl -fsSL --retry 5 --retry-connrefused \"$2\""));
        assert!(s.contains("gpg --batch --quiet --import \"/run/amicu/$1.asc\""));
        let export = s
            .find("gpg --batch --export \"$3\" > \"/run/amicu/$1.gpg\"")
            .unwrap();
        let check = s
            .find("[ \"$keys\" = 1 ] && [ \"$fpr\" = \"$3\" ] ||")
            .unwrap();
        let trusted = s
            .find("install -m 0644 \"/run/amicu/$1.gpg\" \"/etc/apt/keyrings/$1.gpg\"")
            .unwrap();
        assert!(export < check && check < trusted);
        assert!(!s.contains("keyrings/$1.asc"));
        assert!(s.contains("signed-by=/etc/apt/keyrings/%s.gpg"));

        assert!(s.contains("update > /run/amicu/apt-update.log 2>&1"));
        assert!(s.contains(
            "|| echo 'amicu: apt-get update reported errors; checking the needed packages' >&2"
        ));
        assert!(
            s.contains("c=$(apt-cache policy \"$p\" 2>/dev/null | sed -n 's/^ *Candidate: //p')")
        );
        assert!(s.contains("[ -n \"$c\" ] && [ \"$c\" != '(none)' ]"));
        let reason = s
            .find("grep -E '^(E|Err):|download\\.docker\\.com|nvidia\\.github\\.io' /run/amicu/apt-update.log")
            .unwrap();
        assert!(reason < s.find("echo \"amicu: apt cannot find$missing\"").unwrap());

        let configure = s
            .find("nvidia-ctk runtime configure --runtime=docker")
            .unwrap();
        let restart = s.find("systemctl restart docker").unwrap();
        let marker = s
            .find("install -D -m 0644 /dev/null /var/lib/amicu/tools-ready")
            .unwrap();
        assert!(install < configure && configure < restart && restart < marker);
        assert!(s.find("nvidia-smi -L >/dev/null ||").unwrap() < install);
        assert!(marker < s.find("docker pull").unwrap());
    }

    /// Under `pipefail` a reader that stops early (`grep -q`, `awk` with
    /// `exit`) fails the pipeline with the writer's SIGPIPE: that made every
    /// apt candidate look missing.
    #[test]
    fn no_pipeline_in_the_script_ends_in_an_early_exiting_reader() {
        for c in [cfg(), tls_cfg(SecretDelivery::SecretManager)] {
            let s = script(&c);
            assert!(s.contains("set -euo pipefail"));
            assert!(!s.contains("| grep -q"), "{s}");
            assert!(!s.contains("exit }'"), "{s}");
        }
    }

    #[test]
    fn the_script_reports_its_outcome_as_guest_attributes() {
        let s = script(&cfg());
        assert!(s.contains("computeMetadata/v1/instance/guest-attributes/amicu/$1"));
        assert!(s.contains("trap on_exit EXIT"));
        assert!(s.contains("printf 'failed:%s' \"$step\" | ga status"));
        assert!(s.contains("tail -c 4000 /run/amicu/startup.log | ga detail"));
        assert!(s.contains("exec 2> >(tee -a /run/amicu/startup.log >&2)"));
        for step in [
            "read secrets",
            "wait for the NVIDIA driver",
            "install Docker and the NVIDIA container toolkit",
            "pull the vLLM image",
            "start the vLLM container",
            "wait for vLLM to load the model",
        ] {
            assert!(s.contains(&format!("step='{step}'\n")), "{step}");
        }
        assert!(s.trim_end().ends_with("echo 'amicu: vLLM is ready' >&2"));
        assert!(s.contains("printf ready | ga status\n"));
        let booting = s.find("printf booting | ga status\n").unwrap();
        assert!(s.contains("ga boot_id < /proc/sys/kernel/random/boot_id\n"));
        assert!(booting < s.find("step='read secrets'").unwrap());
        assert!(s.find("trap on_exit EXIT").unwrap() < s.find("sm vm-1-api-key").unwrap());
        let meta = startup_metadata(&cfg(), P, N, false);
        assert!(
            meta.iter()
                .any(|i| i["key"] == GUEST_ATTRIBUTES_METADATA && i["value"] == "TRUE")
        );
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

    // `bash` on a Windows runner is the WSL launcher stub, not a shell.
    #[cfg(unix)]
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
