//! Structured security events.
//!
//! Every event is a `tracing` event on the stable target [`TARGET`]
//! (`amicu::security`) with a fixed set of fields, so an operator can route
//! them (e.g. to a SIEM / log-based alert) with one filter:
//!
//! | field | meaning |
//! |-------|---------|
//! | `event` | stable event name, e.g. `signin_failed` |
//! | `outcome` | `ok` / `failed` / `accepted` / `rate_limited` (when relevant) |
//! | `reason` | a fixed, non-secret category (never free text) |
//! | `email_hash`, `ip_hash` | keyed pseudonyms, see [`pseudonym`] |
//! | `user_id`, `workspace_id`, `target_user_id`, `role` | opaque ids / role names |
//!
//! # What is never logged
//!
//! Plaintext email addresses, IP addresses, passwords, and tokens (access,
//! refresh, reset, verify). [`SecurityEvent`] has no way to carry them: the
//! builder only accepts an email/IP to HASH, and every other free-form field
//! is `&'static str` or an opaque id. Do not `Debug`-format secrets into
//! these events.

use std::sync::OnceLock;

use sha2::{Digest, Sha256};

/// The `tracing` target every security event is emitted on.
pub const TARGET: &str = "amicu::security";

/// Default pseudonym key when none is configured: keeps hashes stable across
/// replicas/restarts, but is public (anyone with a log can brute-force a
/// low-entropy input). Set a real secret via [`set_hash_key`] in production.
const DEFAULT_KEY: &[u8] = b"amicu-security-events-v1";

static HASH_KEY: OnceLock<Vec<u8>> = OnceLock::new();

/// Install the secret key used to pseudonymize emails and IPs (first call
/// wins; returns `false` if a key was already set). Must be identical across
/// replicas for hashes (and the per-email buckets keyed on them) to agree.
pub fn set_hash_key(key: &[u8]) -> bool {
    HASH_KEY.set(key.to_vec()).is_ok()
}

/// Keyed, domain-separated pseudonym of `value` (first 16 bytes of
/// `SHA-256(key || 0 || kind || 0 || value)`, hex). Not reversible without
/// the key plus a guess; stable for correlation.
#[must_use]
pub fn pseudonym(kind: &str, value: &str) -> String {
    let key = HASH_KEY.get().map_or(DEFAULT_KEY, Vec::as_slice);
    let mut h = Sha256::new();
    h.update(key);
    h.update([0]);
    h.update(kind.as_bytes());
    h.update([0]);
    h.update(value.as_bytes());
    let digest = h.finalize();
    digest[..16]
        .iter()
        .fold(String::with_capacity(32), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Pseudonym of an email (trimmed + lowercased first, like account lookup).
#[must_use]
pub fn email_hash(email: &str) -> String {
    pseudonym("email", &crate::service::normalize_email(email))
}

/// Pseudonym of a client IP (its textual form).
#[must_use]
pub fn ip_hash(ip: &str) -> String {
    pseudonym("ip", ip)
}

/// Event severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Normal, expected activity.
    Info,
    /// Failed or suspicious activity.
    Warn,
}

/// Builder for one security event. Emit with [`SecurityEvent::emit`].
#[derive(Debug)]
#[must_use]
pub struct SecurityEvent {
    name: &'static str,
    severity: Severity,
    outcome: Option<&'static str>,
    reason: Option<&'static str>,
    email_hash: Option<String>,
    ip_hash: Option<String>,
    user_id: Option<String>,
    workspace_id: Option<String>,
    target_user_id: Option<String>,
    role: Option<&'static str>,
}

impl SecurityEvent {
    /// Start an event with a stable `name`.
    pub fn new(name: &'static str, severity: Severity) -> Self {
        Self {
            name,
            severity,
            outcome: None,
            reason: None,
            email_hash: None,
            ip_hash: None,
            user_id: None,
            workspace_id: None,
            target_user_id: None,
            role: None,
        }
    }

    /// Outcome category.
    pub fn outcome(mut self, outcome: &'static str) -> Self {
        self.outcome = Some(outcome);
        self
    }

    /// Fixed reason category.
    pub fn reason(mut self, reason: &'static str) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Attach the HASH of `email`; the plaintext is not retained.
    pub fn email(mut self, email: &str) -> Self {
        self.email_hash = Some(email_hash(email));
        self
    }

    /// Attach an already-computed IP pseudonym (see [`ip_hash`]).
    pub fn ip_hash(mut self, hash: Option<&str>) -> Self {
        self.ip_hash = hash.map(str::to_owned);
        self
    }

    /// Acting / subject user id (opaque).
    pub fn user(mut self, id: &str) -> Self {
        self.user_id = Some(id.to_owned());
        self
    }

    /// Workspace id (opaque).
    pub fn workspace(mut self, id: &str) -> Self {
        self.workspace_id = Some(id.to_owned());
        self
    }

    /// Target user id of an administrative action (opaque).
    pub fn target_user(mut self, id: &str) -> Self {
        self.target_user_id = Some(id.to_owned());
        self
    }

    /// Role name involved.
    pub fn role(mut self, role: &'static str) -> Self {
        self.role = Some(role);
        self
    }

    /// Emit on [`TARGET`].
    pub fn emit(self) {
        macro_rules! go {
            ($lvl:ident) => {
                tracing::$lvl!(
                    target: TARGET,
                    event = self.name,
                    outcome = self.outcome,
                    reason = self.reason,
                    email_hash = self.email_hash.as_deref(),
                    ip_hash = self.ip_hash.as_deref(),
                    user_id = self.user_id.as_deref(),
                    workspace_id = self.workspace_id.as_deref(),
                    target_user_id = self.target_user_id.as_deref(),
                    role = self.role,
                    "security event"
                )
            };
        }
        match self.severity {
            Severity::Info => go!(info),
            Severity::Warn => go!(warn),
        }
    }
}

/// Test helper: run `f` while capturing every event on [`TARGET`] as
/// `(level, "field=value ..." )` strings. Hidden from docs; used by this
/// crate's and `amicu-api`'s tests.
#[doc(hidden)]
pub mod testing {
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    /// One captured event: its target and `name -> value` field pairs.
    #[derive(Debug, Clone)]
    pub struct Captured {
        /// The event's target.
        pub target: String,
        /// Field name/value pairs (values rendered via `Debug`/`Display`).
        pub fields: Vec<(String, String)>,
    }

    impl Captured {
        /// A field's value, if present.
        #[must_use]
        pub fn get(&self, name: &str) -> Option<&str> {
            self.fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }

        /// The `event` field.
        #[must_use]
        pub fn name(&self) -> &str {
            self.get("event").unwrap_or("")
        }
    }

    struct V(Vec<(String, String)>);
    impl Visit for V {
        fn record_str(&mut self, f: &Field, v: &str) {
            self.0.push((f.name().to_owned(), v.to_owned()));
        }
        fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
            self.0.push((f.name().to_owned(), format!("{v:?}")));
        }
    }

    type Sink = Arc<Mutex<Vec<Captured>>>;

    thread_local! {
        static SINK: std::cell::RefCell<Option<Sink>> = const { std::cell::RefCell::new(None) };
    }

    /// Process-global subscriber that routes each event to the CURRENT
    /// THREAD's sink (if any). A global (rather than per-thread) dispatcher
    /// keeps tracing's callsite-interest cache stable when tests run in
    /// parallel.
    struct Router;

    impl Subscriber for Router {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, e: &Event<'_>) {
            SINK.with(|s| {
                if let Some(sink) = s.borrow().as_ref() {
                    let mut v = V(Vec::new());
                    e.record(&mut v);
                    sink.lock().expect("collector").push(Captured {
                        target: e.metadata().target().to_owned(),
                        fields: v.0,
                    });
                }
            });
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    /// Collects events into a shared vector.
    #[derive(Clone, Default)]
    pub struct Collector(pub Sink);

    /// Stops capturing on this thread when dropped.
    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            SINK.with(|s| *s.borrow_mut() = None);
        }
    }

    /// Start capturing events emitted on the CURRENT THREAD until the guard
    /// drops (use with a current-thread runtime, i.e. plain `#[tokio::test]`).
    #[must_use]
    pub fn capture() -> (Collector, Guard) {
        let _ = tracing::subscriber::set_global_default(Router);
        let c = Collector::default();
        SINK.with(|s| *s.borrow_mut() = Some(c.0.clone()));
        (c, Guard)
    }

    impl Collector {
        /// Snapshot of the security-target events captured so far.
        #[must_use]
        pub fn security_events(&self) -> Vec<Captured> {
            self.0
                .lock()
                .expect("collector")
                .iter()
                .filter(|c| c.target == super::TARGET)
                .cloned()
                .collect()
        }
    }
}
