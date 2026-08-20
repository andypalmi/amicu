//! SSH transport seam between the self-hosted adapter and a real SSH exchange.
//!
//! The self-hosted adapter builds an [`SshTarget`] and a command string and
//! interprets a [`CommandOutput`]; it never touches a concrete SSH client.
//! [`RusshTransport`] is the real implementation used in production; tests inject
//! a mock implementing [`SshTransport`] so the provisioning path is exercised
//! with no network, credentials, or spend.
//!
//! The split mirrors the [HTTP seam](crate::transport): an [`SshTransportError`]
//! means the exchange never completed (connection failure, auth failure, timeout)
//! — the analogue of a `reqwest` error, never a status code — while a command
//! that *ran* and returned any exit status (zero or not) is an
//! `Ok(CommandOutput)` for the adapter to interpret, exactly as an HTTP `4xx`/
//! `5xx` is an `Ok(HttpResponse)`. Auth failure is the one permanent variant (it
//! maps to [`CredentialsRejected`](crate::provider::ProviderError::CredentialsRejected));
//! connection and timeout are transient.

use amicu_core::secret::SecretValue;
use async_trait::async_trait;

/// Where to run a command: the host/address, the login user, and the port.
#[derive(Debug, Clone)]
pub struct SshTarget {
    /// Host name or address to connect to.
    pub host: String,
    /// Login user.
    pub user: String,
    /// TCP port; 22 by default.
    pub port: u16,
}

impl SshTarget {
    /// A target for `user`@`host` on the default SSH port (22).
    #[must_use]
    pub fn new(host: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            user: user.into(),
            port: 22,
        }
    }

    /// Override the TCP port.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

/// A completed remote command: its exit status and the raw `stdout`/`stderr`
/// bytes.
///
/// Returned for every command that ran, whatever its exit status — the adapter
/// inspects [`exit_status`](Self::exit_status) (via [`is_success`](Self::is_success))
/// to decide success or which [`ProviderError`](crate::provider::ProviderError)
/// applies, the way it inspects an HTTP status code.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// The command's exit status (0 on success).
    pub exit_status: i32,
    /// Raw standard output.
    pub stdout: Vec<u8>,
    /// Raw standard error.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// A command output with the given exit status and streams.
    #[must_use]
    pub fn new(exit_status: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            exit_status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Whether the command exited with status 0.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_status == 0
    }

    /// `stderr` decoded lossily as UTF-8, trimmed — a secret-free summary safe to
    /// fold into an error `detail`.
    #[must_use]
    pub fn stderr_summary(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// A transport-level SSH failure: the exchange never completed, so there is no
/// command output to interpret.
///
/// [`Auth`](Self::Auth) is permanent (the credential was rejected);
/// [`Connection`](Self::Connection) and [`Timeout`](Self::Timeout) are transient
/// (the adapter maps them to a retryable backend error).
#[derive(Debug, thiserror::Error)]
pub enum SshTransportError {
    /// The TCP/transport connection failed (DNS, refused, protocol error).
    #[error("ssh connection error: {0}")]
    Connection(String),
    /// Authentication was rejected (bad or unauthorized key).
    #[error("ssh authentication failed: {0}")]
    Auth(String),
    /// The exchange did not complete within the timeout.
    #[error("ssh exchange timed out")]
    Timeout,
}

/// The seam the adapter calls to run a command over SSH.
///
/// Implemented by [`RusshTransport`] in production and by test mocks. Kept
/// object-safe so the adapter can hold `Arc<dyn SshTransport>`.
///
/// The `key` is the transiently resolved SSH private key: the transport uses it
/// only to authenticate this one connection and never stores, logs, or returns
/// it.
#[async_trait]
pub trait SshTransport: Send + Sync {
    /// Run `command` on `target`, authenticating with `key`, and return its
    /// output.
    ///
    /// # Errors
    /// [`SshTransportError`] when the exchange does not complete. A command that
    /// ran is an `Ok(CommandOutput)` regardless of its exit status.
    async fn run(
        &self,
        target: &SshTarget,
        command: &str,
        key: &SecretValue,
    ) -> Result<CommandOutput, SshTransportError>;
}

/// The production [`SshTransport`], backed by [`russh`] — a pure-Rust async SSH
/// client.
///
/// `russh` (with its `ring` crypto backend) keeps the build hermetic: like the
/// workspace's rustls choice it pulls no system OpenSSL or `libssh2` C
/// dependency, and it holds the MSRV. Constructing this does no network I/O.
pub struct RusshTransport {
    timeout: std::time::Duration,
}

impl RusshTransport {
    /// A transport with the default exchange timeout (30s).
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(std::time::Duration::from_secs(30))
    }

    /// A transport with an explicit exchange timeout.
    #[must_use]
    pub fn with_timeout(timeout: std::time::Duration) -> Self {
        Self { timeout }
    }
}

impl Default for RusshTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Accepts the server host key. Host-key pinning against a known-hosts store is
/// out of scope for this phase.
// TODO(TASK-070): verify the server host key instead of accepting it.
struct AcceptingHandler;

impl russh::client::Handler for AcceptingHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[async_trait]
impl SshTransport for RusshTransport {
    async fn run(
        &self,
        target: &SshTarget,
        command: &str,
        key: &SecretValue,
    ) -> Result<CommandOutput, SshTransportError> {
        use std::sync::Arc;

        // Decode the transient key. It is used only to authenticate this
        // connection and is dropped with the caller's SecretValue.
        let pem = key
            .expose_secret_str()
            .map_err(|_| SshTransportError::Auth("ssh key is not valid UTF-8".into()))?;
        let secret = russh::keys::decode_secret_key(pem, None)
            .map_err(|e| SshTransportError::Auth(format!("ssh key could not be decoded: {e}")))?;

        let config = Arc::new(russh::client::Config::default());
        let connect = russh::client::connect(
            config,
            (target.host.as_str(), target.port),
            AcceptingHandler,
        );
        let mut handle = tokio::time::timeout(self.timeout, connect)
            .await
            .map_err(|_| SshTransportError::Timeout)?
            .map_err(|e| SshTransportError::Connection(e.to_string()))?;

        let key_with_hash = russh::keys::PrivateKeyWithHashAlg::new(
            Arc::new(secret),
            Some(russh::keys::HashAlg::Sha256),
        );
        let auth = handle
            .authenticate_publickey(&target.user, key_with_hash)
            .await
            .map_err(|e| SshTransportError::Connection(e.to_string()))?;
        if !auth.success() {
            return Err(SshTransportError::Auth("ssh key was rejected".into()));
        }

        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| SshTransportError::Connection(e.to_string()))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| SshTransportError::Connection(e.to_string()))?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status: i32 = 0;
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => stdout.extend_from_slice(data),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    stderr.extend_from_slice(data);
                }
                russh::ChannelMsg::ExitStatus { exit_status: code } => {
                    exit_status = i32::try_from(code).unwrap_or(-1);
                }
                _ => {}
            }
        }
        Ok(CommandOutput {
            exit_status,
            stdout,
            stderr,
        })
    }
}

/// A canned outcome registered on a [`MockSshTransport`], keyed by a command
/// substring.
#[cfg(test)]
type CannedOutcome = (String, Result<CommandOutput, SshTransportError>);

/// A test [`SshTransport`] returning canned outcomes keyed by a command
/// substring and recording the commands it was asked to run (per target), so
/// adapter tests drive the provisioning path with no network, credentials, or
/// spend.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockSshTransport {
    outcomes: std::sync::Mutex<Vec<CannedOutcome>>,
    seen: std::sync::Mutex<Vec<SeenCommand>>,
}

/// A command a [`MockSshTransport`] observed: the target host/user/port and the
/// command string. The key is never recorded — the mock ignores it — so a test
/// can assert the key never leaks.
#[cfg(test)]
pub(crate) struct SeenCommand {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub command: String,
}

#[cfg(test)]
impl MockSshTransport {
    /// Register a canned successful command output for commands containing
    /// `needle`.
    pub(crate) fn on(self, needle: &str, output: CommandOutput) -> Self {
        self.outcomes
            .lock()
            .unwrap()
            .push((needle.into(), Ok(output)));
        self
    }

    /// Register a canned transport failure for commands containing `needle`.
    pub(crate) fn on_error(self, needle: &str, error: SshTransportError) -> Self {
        self.outcomes
            .lock()
            .unwrap()
            .push((needle.into(), Err(error)));
        self
    }

    /// The commands this mock has seen, in order.
    pub(crate) fn commands(&self) -> std::sync::MutexGuard<'_, Vec<SeenCommand>> {
        self.seen.lock().unwrap()
    }
}

#[cfg(test)]
fn clone_error(error: &SshTransportError) -> SshTransportError {
    match error {
        SshTransportError::Connection(m) => SshTransportError::Connection(m.clone()),
        SshTransportError::Auth(m) => SshTransportError::Auth(m.clone()),
        SshTransportError::Timeout => SshTransportError::Timeout,
    }
}

#[cfg(test)]
#[async_trait]
impl SshTransport for MockSshTransport {
    async fn run(
        &self,
        target: &SshTarget,
        command: &str,
        _key: &SecretValue,
    ) -> Result<CommandOutput, SshTransportError> {
        self.seen.lock().unwrap().push(SeenCommand {
            host: target.host.clone(),
            user: target.user.clone(),
            port: target.port,
            command: command.to_string(),
        });
        let outcomes = self.outcomes.lock().unwrap();
        let hit = outcomes.iter().find(|(needle, _)| command.contains(needle));
        match hit {
            Some((_, Ok(output))) => Ok(output.clone()),
            Some((_, Err(error))) => Err(clone_error(error)),
            // Unmatched commands succeed with empty output, mirroring the HTTP
            // mock's default.
            None => Ok(CommandOutput::new(0, Vec::new(), Vec::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretValue {
        SecretValue::new("dummy-key".to_string())
    }

    #[test]
    fn output_success_and_summary() {
        let ok = CommandOutput::new(0, b"ready".to_vec(), Vec::new());
        assert!(ok.is_success());
        let bad = CommandOutput::new(1, Vec::new(), b"  boom\n".to_vec());
        assert!(!bad.is_success());
        assert_eq!(bad.stderr_summary(), "boom");
    }

    #[tokio::test]
    async fn mock_matches_on_command_substring_and_records_target() {
        let mock =
            MockSshTransport::default().on("prepare", CommandOutput::new(0, b"ok".to_vec(), []));
        let target = SshTarget::new("box.example", "amicu").with_port(2222);
        let out = mock.run(&target, "amicu-host prepare rack-1", &key()).await;
        assert!(out.unwrap().is_success());
        let seen = mock.commands();
        assert_eq!(seen[0].host, "box.example");
        assert_eq!(seen[0].user, "amicu");
        assert_eq!(seen[0].port, 2222);
        assert!(seen[0].command.contains("prepare"));
    }

    #[tokio::test]
    async fn mock_unmatched_command_succeeds_empty() {
        let out = MockSshTransport::default()
            .run(&SshTarget::new("h", "u"), "whatever", &key())
            .await
            .unwrap();
        assert!(out.is_success());
        assert!(out.stdout.is_empty());
    }

    #[tokio::test]
    async fn mock_returns_canned_error() {
        let mock = MockSshTransport::default().on_error("prepare", SshTransportError::Timeout);
        let err = mock
            .run(&SshTarget::new("h", "u"), "prepare", &key())
            .await
            .expect_err("canned error surfaces");
        assert!(matches!(err, SshTransportError::Timeout));
    }
}
