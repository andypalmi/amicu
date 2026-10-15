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
//! response with a 5xx status (method and matched route template only - never
//! the concrete URI, query string, headers or body).

use std::borrow::Cow;
use std::io::Write;
use std::sync::Arc;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use sentry::protocol::Event;
use sentry::{ClientInitGuard, ClientOptions, Level};

use crate::Environment;

const REDACTED: &str = "[redacted]";
const EMAIL: &str = "[email]";

/// Parameter names whose `name=value` values are redacted.
const SECRET_KEYS: [&str; 5] = ["token=", "code=", "key=", "secret=", "password="];

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

/// Report every 5xx response. Mounted only when Sentry is enabled. The event
/// carries the method and the matched route template, never the real URI.
pub async fn report_server_errors(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "<unmatched>".to_owned(), |m| m.as_str().to_owned());
    let response = next.run(request).await;
    if response.status().is_server_error() {
        sentry::capture_message(
            &format!("HTTP {} on {method} {route}", response.status().as_u16()),
            Level::Error,
        );
    }
    response
}

/// Strip everything identifying from an event and redact secrets in its text.
#[must_use]
pub fn scrub_event(mut event: Event<'static>) -> Event<'static> {
    event.user = None;
    event.request = None;
    event.extra.clear();
    event.breadcrumbs = Vec::new().into();
    event.server_name = None;
    event.transaction = None;
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
    }
    event
}

/// Redact emails, bearer tokens and `token=`-style values in `input`.
#[must_use]
pub fn scrub_text(input: &str) -> String {
    let s = redact_bearer(&redact_keyed(input));
    redact_emails(&s)
}

fn is_value_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '&' | '"' | '\'' | ',' | ';' | ')' | '>' | '<')
}

/// Replace the value after each `prefix` (matched ASCII case-insensitively).
fn redact_after(input: &str, prefix: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find(prefix) {
        let value_start = pos + rel + prefix.len();
        out.push_str(&input[pos..value_start]);
        let rest = &input[value_start..];
        let len = rest.find(is_value_end).unwrap_or(rest.len());
        if len > 0 {
            out.push_str(REDACTED);
        }
        pos = value_start + len;
    }
    out.push_str(&input[pos..]);
    out
}

fn redact_keyed(input: &str) -> String {
    SECRET_KEYS
        .iter()
        .fold(input.to_owned(), |acc, k| redact_after(&acc, k))
}

fn redact_bearer(input: &str) -> String {
    redact_after(input, "bearer ")
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
    use sentry::protocol::{Exception, LogEntry, Request as SentryRequest, User};

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
    fn redacts_bearer_tokens() {
        assert_eq!(
            scrub_text("Authorization: Bearer abc.def.ghi failed"),
            "Authorization: Bearer [redacted] failed"
        );
        assert_eq!(scrub_text("bearer   x"), "bearer   x");
    }

    #[test]
    fn redacts_keyed_values() {
        assert_eq!(
            scrub_text("GET /x?token=abc123&code=Z9&ok=1 key=k1 SECRET=s Password=p@ss"),
            "GET /x?token=[redacted]&code=[redacted]&ok=1 key=[redacted] SECRET=[redacted] Password=[redacted]"
        );
        assert_eq!(scrub_text("api_key=zzz"), "api_key=[redacted]");
        assert_eq!(scrub_text("token= next"), "token= next");
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
        event.exception.values.push(Exception {
            ty: "Panic".into(),
            value: Some("bad Bearer tok and a@b.com".into()),
            ..Default::default()
        });
        let out = scrub_event(event);
        assert!(out.user.is_none());
        assert!(out.request.is_none());
        assert!(out.extra.is_empty());
        assert!(out.server_name.is_none());
        assert_eq!(out.message.as_deref(), Some("boom for [email]"));
        let l = out.logentry.unwrap();
        assert_eq!(l.message, "pw password=[redacted]");
        assert_eq!(l.params.len(), 0);
        assert_eq!(
            out.exception.values[0].value.as_deref(),
            Some("bad Bearer [redacted] and [email]")
        );
    }

    #[test]
    fn disabled_without_dsn() {
        assert!(init_with(|_| None, Environment::Development).is_none());
        assert!(init_with(|_| Some("  ".into()), Environment::Development).is_none());
        assert!(init_with(|_| Some("not a dsn".into()), Environment::Development).is_none());
    }
}
