//! Opt-in error reporting to Sentry.
//!
//! Active ONLY when `SENTRY_DSN` is set; otherwise nothing is initialised and
//! the request layer is never mounted, so behaviour is unchanged. Errors only:
//! no tracing, no profiling, `send_default_pii` off. Every event passes
//! through [`scrub_event`] before leaving the process, which drops user,
//! request, extra data and breadcrumbs and redacts emails, bearer tokens and
//! `token=`/`code=`/`key=`/`secret=`/`password=` values from message text.
//!
//! What is reported: panics (via the SDK's panic integration) and every
//! response with a 5xx status (deduplicated, probes excluded; method and matched route template only - never
//! the concrete URI, query string, headers or body).

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use sentry::protocol::{Event, Stacktrace};
use sentry::{ClientInitGuard, ClientOptions, Level};

use crate::Environment;

const REDACTED: &str = "[redacted]";
const EMAIL: &str = "[email]";

/// Parameter names whose `name=value` (or `name: value`) values are redacted.
const SECRET_KEYS: [&str; 5] = ["token", "code", "key", "secret", "password"];

/// Initialise Sentry if `SENTRY_DSN` is set (non-empty and valid). The returned
/// guard must be kept alive for the life of the process (it flushes on drop).
/// `SENTRY_ENVIRONMENT` overrides the environment name, which otherwise follows
/// `AMICU_ENV`.
#[must_use]
pub fn init(environment: Environment) -> Option<ClientInitGuard> {
    init_with(|name| std::env::var(name).ok(), environment)
}

fn init_with<F: Fn(&str) -> Option<String>>(
    get: F,
    environment: Environment,
) -> Option<ClientInitGuard> {
    let raw = get("SENTRY_DSN")?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let Ok(dsn) = raw.parse::<sentry::types::Dsn>() else {
        // Never echo the value: a DSN carries a credential.
        let _ = writeln!(
            std::io::stderr(),
            "amicu-server: SENTRY_DSN is set but invalid; error reporting disabled"
        );
        return None;
    };
    let env_name = get("SENTRY_ENVIRONMENT")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            match environment {
                Environment::Development => "development",
                Environment::Production => "production",
            }
            .to_owned()
        });
    // `ClientOptions` is non-exhaustive: start from the defaults and set fields.
    let mut options = ClientOptions::new().traces_sample_rate(0.0);
    options.dsn = Some(dsn);
    options.release = Some(Cow::Owned(format!(
        "amicu-server@{}",
        env!("CARGO_PKG_VERSION")
    )));
    options.environment = Some(Cow::Owned(env_name));
    options.send_default_pii = false;
    options.attach_stacktrace = false;
    options.max_breadcrumbs = 0;
    options.before_send = Some(Arc::new(|event| Some(scrub_event(event))));
    Some(sentry::init(options))
}

/// Probe routes whose 5xx responses are never reported (an unready database
/// makes `/readyz` fail by design, and probes fire constantly).
const PROBE_ROUTES: [&str; 2] = ["/healthz", "/readyz"];

/// At most one report per `(method, route, status)` within this window, so an
/// outage cannot flood the Sentry quota.
const DEDUPE_WINDOW: Duration = Duration::from_secs(60);

/// Prune expired entries once the map grows past this many keys.
const DEDUPE_PRUNE_AT: usize = 256;

/// In-memory per-key rate limiter.
#[derive(Default)]
struct Dedupe {
    last: HashMap<(String, String, u16), Instant>,
}

impl Dedupe {
    fn allow(&mut self, key: (String, String, u16), now: Instant) -> bool {
        if self.last.len() >= DEDUPE_PRUNE_AT {
            self.last
                .retain(|_, t| now.saturating_duration_since(*t) < DEDUPE_WINDOW);
        }
        if let Some(t) = self.last.get(&key)
            && now.saturating_duration_since(*t) < DEDUPE_WINDOW
        {
            return false;
        }
        self.last.insert(key, now);
        true
    }
}

fn dedupe() -> &'static Mutex<Dedupe> {
    static D: OnceLock<Mutex<Dedupe>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(Dedupe::default()))
}

/// Report 5xx responses (deduplicated, probes excluded). Mounted only when
/// Sentry is enabled. The event carries the method and the matched route
/// template, never the real URI.
pub async fn report_server_errors(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "<unmatched>".to_owned(), |m| m.as_str().to_owned());
    let response = next.run(request).await;
    let status = response.status();
    if status.is_server_error() && !PROBE_ROUTES.contains(&route.as_str()) {
        let allowed = dedupe()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .allow(
                (method.clone(), route.clone(), status.as_u16()),
                Instant::now(),
            );
        if allowed {
            sentry::capture_message(
                &format!("HTTP {} on {method} {route}", status.as_u16()),
                Level::Error,
            );
        }
    }
    response
}

/// Context entries kept on events (everything else, e.g. device/hostname, goes).
const KEPT_CONTEXTS: [&str; 2] = ["runtime", "os"];

fn scrub_stacktrace(st: &mut Option<Stacktrace>) {
    if let Some(st) = st {
        for frame in &mut st.frames {
            frame.abs_path = None;
            frame.vars.clear();
            frame.pre_context.clear();
            frame.post_context.clear();
            frame.context_line = None;
        }
    }
}

/// Strip everything identifying from an event and redact secrets in its text.
#[must_use]
pub fn scrub_event(mut event: Event<'static>) -> Event<'static> {
    event.user = None;
    event.request = None;
    event.extra.clear();
    event.breadcrumbs = Vec::new().into();
    event.threads = Vec::new().into();
    event.modules.clear();
    event.tags.clear();
    event.server_name = None;
    event.transaction = None;
    event.culprit = None;
    event
        .contexts
        .retain(|k, _| KEPT_CONTEXTS.contains(&k.as_str()));
    scrub_stacktrace(&mut event.stacktrace);
    if let Some(m) = event.message.as_mut() {
        *m = scrub_text(m);
    }
    if let Some(l) = event.logentry.as_mut() {
        l.message = scrub_text(&l.message);
        l.params.clear();
    }
    for ex in &mut event.exception.values {
        if let Some(v) = ex.value.as_mut() {
            *v = scrub_text(v);
        }
        scrub_stacktrace(&mut ex.stacktrace);
        scrub_stacktrace(&mut ex.raw_stacktrace);
    }
    event
}

/// Redact URL credentials, JWTs, well-known API keys, `token=`-style values,
/// bearer tokens and emails in `input`. URL userinfo goes first so the user
/// part of `scheme://user:pw@host` is not left behind by the email pass.
#[must_use]
pub fn scrub_text(input: &str) -> String {
    let s = redact_userinfo(input);
    let s = redact_jwts(&s);
    let s = redact_api_keys(&s);
    let s = redact_keyed(&s);
    let s = redact_bearer(&s);
    redact_emails(&s)
}

fn is_value_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '&' | '"' | '\'' | ',' | ';' | ')' | '>' | '<' | '\\')
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// `scheme://userinfo@host` -> `scheme://[redacted]@host`.
fn redact_userinfo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(i) = rest.find("://") {
        let auth_start = i + 3;
        out.push_str(&rest[..auth_start]);
        rest = &rest[auth_start..];
        let auth_len = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '"' | '\''))
            .unwrap_or(rest.len());
        if let Some(at) = rest[..auth_len].rfind('@') {
            out.push_str(REDACTED);
            rest = &rest[at..];
        }
    }
    out.push_str(rest);
    out
}

/// `eyJ<b64>.<b64>.<b64 or empty>`.
fn redact_jwts(input: &str) -> String {
    let b = input.as_bytes();
    let seg = |from: usize| {
        from + input[from..]
            .find(|c: char| !is_token_char(c))
            .unwrap_or(input.len() - from)
    };
    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    let mut from = 0;
    while let Some(rel) = input[from..].find("eyJ") {
        let s = from + rel;
        from = s + 3;
        if s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] >= 0x80) {
            continue;
        }
        let e1 = seg(s);
        if b.get(e1) != Some(&b'.') {
            continue;
        }
        let e2 = seg(e1 + 1);
        if e2 == e1 + 1 || b.get(e2) != Some(&b'.') {
            continue;
        }
        let e3 = seg(e2 + 1);
        out.push_str(&input[pos..s]);
        out.push_str(REDACTED);
        pos = e3;
        from = e3;
    }
    out.push_str(&input[pos..]);
    out
}

fn looks_like_api_key(run: &str) -> bool {
    let n = run.len();
    let long = |prefix: &str, min: usize| run.starts_with(prefix) && n >= min;
    long("sk-", 20)
        || long("sk_", 20)
        || long("pk_", 20)
        || long("rk_", 20)
        || long("whsec_", 20)
        || long("ghp_", 30)
        || long("gho_", 30)
        || long("ghu_", 30)
        || long("ghs_", 30)
        || long("ghr_", 30)
        || long("github_pat_", 30)
        || (run.starts_with("xox")
            && matches!(run.as_bytes().get(3), Some(b'b' | b'a' | b'p'))
            && run.as_bytes().get(4) == Some(&b'-')
            && n >= 15)
        || ((run.starts_with("AKIA") || run.starts_with("ASIA"))
            && n == 20
            && run
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

/// Replace whole `[A-Za-z0-9_-]` runs that look like a known API key.
fn redact_api_keys(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        let Some(start) = rest.find(is_token_char) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c: char| !is_token_char(c)).unwrap_or(rest.len());
        let run = &rest[..end];
        out.push_str(if looks_like_api_key(run) {
            REDACTED
        } else {
            run
        });
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Redact the value in `<name><quote?> [=:] <quote?><value>` for every
/// occurrence of `name` (ASCII case-insensitive, `name` lowercase). The key
/// must start at a word boundary; `_`/`-` count as a boundary (so `api_key=`
/// and `access_token=` match) unless `allow_joined` is false (`error_code=`).
fn redact_key(input: &str, name: &str, allow_joined: bool) -> String {
    let lower = input.to_ascii_lowercase();
    let b = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    let mut from = 0;
    while let Some(rel) = lower[from..].find(name) {
        let s = from + rel;
        let mut j = s + name.len();
        from = j;
        if s > 0 {
            let p = b[s - 1];
            if p.is_ascii_alphanumeric() || p >= 0x80 || (!allow_joined && matches!(p, b'_' | b'-'))
            {
                continue;
            }
        }
        while matches!(b.get(j), Some(b'"' | b'\'' | b'\\')) {
            j += 1;
        }
        while b.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        if !matches!(b.get(j), Some(b'=' | b':')) {
            continue;
        }
        j += 1;
        while b
            .get(j)
            .is_some_and(|c| c.is_ascii_whitespace() || *c == b'\\')
        {
            j += 1;
        }
        let quote = b.get(j).copied().filter(|c| matches!(c, b'"' | b'\''));
        if quote.is_some() {
            j += 1;
        }
        let rest = &input[j..];
        let len = match quote {
            Some(q) => rest.find([q as char, '\\']),
            None => rest.find(is_value_end),
        }
        .unwrap_or(rest.len());
        if len > 0 {
            out.push_str(&input[pos..j]);
            out.push_str(REDACTED);
            pos = j + len;
            from = pos;
        }
    }
    out.push_str(&input[pos..]);
    out
}

fn redact_keyed(input: &str) -> String {
    SECRET_KEYS.iter().fold(input.to_owned(), |acc, k| {
        // `code` only matches standalone, so `error_code=5` is left alone.
        redact_key(&acc, k, *k != "code")
    })
}

fn redact_bearer(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let b = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    let mut from = 0;
    while let Some(rel) = lower[from..].find("bearer") {
        let s = from + rel;
        let mut j = s + 6;
        from = j;
        if s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] >= 0x80) {
            continue;
        }
        let ws = j;
        while b.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        if j == ws {
            continue;
        }
        let rest = &input[j..];
        let len = rest.find(is_value_end).unwrap_or(rest.len());
        if len > 0 {
            out.push_str(&input[pos..j]);
            out.push_str(REDACTED);
            pos = j + len;
            from = pos;
        }
    }
    out.push_str(&input[pos..]);
    out
}
fn is_email_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn redact_emails(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find('@') {
        let (before, after) = rest.split_at(at);
        let after = &after[1..];
        let local_len = before
            .chars()
            .rev()
            .take_while(|c| is_email_char(*c))
            .map(char::len_utf8)
            .sum::<usize>();
        let domain_len = after
            .chars()
            .take_while(|c| is_email_char(*c))
            .map(char::len_utf8)
            .sum::<usize>();
        let domain = &after[..domain_len];
        if local_len > 0 && domain.trim_end_matches('.').contains('.') {
            out.push_str(&before[..before.len() - local_len]);
            out.push_str(EMAIL);
            rest = &after[domain_len..];
        } else {
            out.push_str(before);
            out.push('@');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentry::protocol::{Exception, Frame, LogEntry, Request as SentryRequest, User};

    #[test]
    fn redacts_emails() {
        assert_eq!(
            scrub_text("failed for jane.doe+x@example.co.uk now"),
            "failed for [email] now"
        );
        assert_eq!(scrub_text("a@b.com,c@d.org"), "[email],[email]");
        assert_eq!(scrub_text("user @ host and a@b"), "user @ host and a@b");
    }

    #[test]
    fn redacts_url_userinfo() {
        assert_eq!(
            scrub_text("connect postgres://app:p%40ss@db.internal:5432/amicu failed"),
            "connect postgres://[redacted]@db.internal:5432/amicu failed"
        );
        assert_eq!(
            scrub_text("smtps://bob@example.com:pw@mail.host:465"),
            "smtps://[redacted]@mail.host:465"
        );
        for s in ["redis", "mysql", "amqp", "https", "weird+scheme"] {
            assert_eq!(
                scrub_text(&format!("{s}://u:p@h/x")),
                format!("{s}://[redacted]@h/x")
            );
        }
        assert_eq!(
            scrub_text("https://example.com/a@b"),
            "https://example.com/a@b"
        );
    }

    #[test]
    fn redacts_jwts() {
        assert_eq!(
            scrub_text("got eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig_-x here"),
            "got [redacted] here"
        );
        assert_eq!(scrub_text("eyJhbGci.eyJzdWI."), "[redacted]");
        assert_eq!(scrub_text("eyJonlyone"), "eyJonlyone");
    }

    #[test]
    fn redacts_api_key_prefixes() {
        for k in [
            "sk-abcdefghijklmnopqrstuvwx",
            "sk_live_abcdefghijklmnop1234",
            "pk_live_abcdefghijklmnop1234",
            "whsec_abcdefghijklmnop1234",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "github_pat_11ABCDEFG0abcdefghijklmnopqrstuv",
            "xoxb-123456789012-abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            assert_eq!(
                scrub_text(&format!("used {k} ok")),
                "used [redacted] ok",
                "{k}"
            );
        }
        for s in ["sk-short", "skeleton_key_value", "AKIAshort", "ghp_tiny"] {
            assert_eq!(scrub_text(s), s);
        }
    }

    #[test]
    fn redacts_bearer_tokens() {
        assert_eq!(
            scrub_text("Authorization: Bearer abc.def.ghi failed"),
            "Authorization: Bearer [redacted] failed"
        );
        assert_eq!(scrub_text("bearer   x"), "bearer   [redacted]");
        assert_eq!(scrub_text("bearers of news"), "bearers of news");
    }

    #[test]
    fn redacts_keyed_values() {
        assert_eq!(
            scrub_text("GET /x?token=abc123&code=Z9&ok=1 key=k1 SECRET=s Password=p@ss"),
            "GET /x?token=[redacted]&code=[redacted]&ok=1 key=[redacted] SECRET=[redacted] Password=[redacted]"
        );
        assert_eq!(scrub_text("api_key=zzz"), "api_key=[redacted]");
        assert_eq!(scrub_text("access_token=zzz"), "access_token=[redacted]");
        assert_eq!(scrub_text("x-api-key: zzz"), "x-api-key: [redacted]");
        assert_eq!(scrub_text("token: x"), "token: [redacted]");
        assert_eq!(scrub_text("token= next"), "token= [redacted]");
        assert_eq!(
            scrub_text(r#"password="x y z" ok"#),
            r#"password="[redacted]" ok"#
        );
        assert_eq!(
            scrub_text(r#"{"password":"hunter2","a":1}"#),
            r#"{"password":"[redacted]","a":1}"#
        );
        assert_eq!(
            scrub_text(r#"{\"secret\":\"abc\"}"#),
            r#"{\"secret\":\"[redacted]\"}"#
        );
        assert_eq!(scrub_text("token=&code=&x"), "token=&code=&x");
    }

    #[test]
    fn keyed_match_respects_word_boundaries() {
        for s in [
            "error_code=5",
            "monkey=banana",
            "hockey: on",
            "decode=1",
            "keys=a",
        ] {
            assert_eq!(scrub_text(s), s);
        }
    }

    #[test]
    fn leaves_clean_text_alone() {
        let s = "connection refused (os error 111) at 10.0.0.1:5432";
        assert_eq!(scrub_text(s), s);
    }

    #[test]
    fn scrubs_whole_event() {
        let mut event = Event {
            message: Some("boom for a@b.com".into()),
            user: Some(User {
                email: Some("a@b.com".into()),
                ..Default::default()
            }),
            request: Some(SentryRequest {
                url: Some("https://x/y?token=1".parse().unwrap()),
                query_string: Some("token=1".into()),
                cookies: Some("sid=1".into()),
                data: Some("body".into()),
                ..Default::default()
            }),
            logentry: Some(LogEntry {
                message: "pw password=hunter2".into(),
                params: vec!["a@b.com".into()],
            }),
            server_name: Some("host".into()),
            ..Default::default()
        };
        event
            .extra
            .insert("k".into(), sentry::protocol::Value::from("v"));
        event.modules.insert("m".into(), "1".into());
        event.tags.insert("t".into(), "v".into());
        event.contexts.insert(
            "device".into(),
            sentry::protocol::Context::Other(std::collections::BTreeMap::default()),
        );
        event.contexts.insert(
            "os".into(),
            sentry::protocol::Context::Other(std::collections::BTreeMap::default()),
        );
        let mut frame = Frame {
            function: Some("f".into()),
            filename: Some("lib.rs".into()),
            abs_path: Some("/Users/me/src/lib.rs".into()),
            ..Default::default()
        };
        frame
            .vars
            .insert("v".into(), sentry::protocol::Value::from("s"));
        event.exception.values.push(Exception {
            ty: "Panic".into(),
            value: Some("bad Bearer tok and a@b.com".into()),
            stacktrace: Some(Stacktrace {
                frames: vec![frame],
                ..Default::default()
            }),
            ..Default::default()
        });
        let out = scrub_event(event);
        assert!(out.user.is_none());
        assert!(out.request.is_none());
        assert!(out.extra.is_empty());
        assert!(out.modules.is_empty());
        assert!(out.tags.is_empty());
        assert_eq!(out.threads.values.len(), 0);
        assert!(out.server_name.is_none());
        assert_eq!(out.contexts.keys().collect::<Vec<_>>(), ["os"]);
        assert_eq!(out.message.as_deref(), Some("boom for [email]"));
        let l = out.logentry.unwrap();
        assert_eq!(l.message, "pw password=[redacted]");
        assert_eq!(l.params.len(), 0);
        assert_eq!(
            out.exception.values[0].value.as_deref(),
            Some("bad Bearer [redacted] and [email]")
        );
        let f = &out.exception.values[0].stacktrace.as_ref().unwrap().frames[0];
        assert!(f.abs_path.is_none());
        assert!(f.vars.is_empty());
        assert_eq!(f.filename.as_deref(), Some("lib.rs"));
        assert_eq!(f.function.as_deref(), Some("f"));
    }

    #[test]
    fn dedupe_allows_one_per_key_per_window() {
        let mut d = Dedupe::default();
        let t0 = Instant::now();
        let k = || ("GET".to_owned(), "/x".to_owned(), 500);
        assert!(d.allow(k(), t0));
        assert!(!d.allow(k(), t0 + Duration::from_secs(30)));
        assert!(d.allow(("GET".to_owned(), "/x".to_owned(), 503), t0));
        assert!(d.allow(k(), t0 + DEDUPE_WINDOW));
    }

    #[test]
    fn disabled_without_dsn() {
        assert!(init_with(|_| None, Environment::Development).is_none());
        assert!(init_with(|_| Some("  ".into()), Environment::Development).is_none());
        assert!(init_with(|_| Some("not a dsn".into()), Environment::Development).is_none());
    }
}
