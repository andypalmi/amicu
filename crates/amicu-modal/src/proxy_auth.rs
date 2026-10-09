//! Modal Proxy Auth Token support for HTTP requests to a deployed server.
//!
//! Modal Servers (`is_server = true`, `*.modal.direct` URLs) require proxy
//! auth by default: an unauthenticated request gets a `401` from Modal's
//! proxy before it reaches the container. A request authenticates with a
//! workspace Proxy Auth Token, sent as the `Modal-Key` / `Modal-Secret`
//! headers (not `Authorization: Bearer`, which vLLM's `--api-key` also uses).
//!
//! Create a token in the Modal dashboard (Settings -> Proxy Auth Tokens) and
//! export [`TOKEN_ID_ENV_VAR`] (`wk-...`) and [`TOKEN_SECRET_ENV_VAR`]
//! (`ws-...`). The secret is held in [`ProxyToken`], whose `Debug` redacts it,
//! and is never logged or placed in an error.

use std::fmt;

/// Env var holding the Proxy Auth Token id (`wk-...`), sent as `Modal-Key`.
pub const TOKEN_ID_ENV_VAR: &str = "MODAL_PROXY_TOKEN_ID";
/// Env var holding the Proxy Auth Token secret (`ws-...`), sent as `Modal-Secret`.
pub const TOKEN_SECRET_ENV_VAR: &str = "MODAL_PROXY_TOKEN_SECRET";

/// A Modal Proxy Auth Token. No `Display`; `Debug` redacts the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyToken {
    id: String,
    secret: String,
}

impl fmt::Debug for ProxyToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyToken")
            .field("id", &"<redacted>")
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl ProxyToken {
    /// Builds a token from raw values; `None` unless both are non-blank.
    #[must_use]
    pub fn from_values(id: Option<String>, secret: Option<String>) -> Option<Self> {
        let id = id?.trim().to_owned();
        let secret = secret?.trim().to_owned();
        (!id.is_empty() && !secret.is_empty()).then_some(Self { id, secret })
    }

    /// Reads the token from [`TOKEN_ID_ENV_VAR`] / [`TOKEN_SECRET_ENV_VAR`].
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_values(
            std::env::var(TOKEN_ID_ENV_VAR).ok(),
            std::env::var(TOKEN_SECRET_ENV_VAR).ok(),
        )
    }

    /// The `(Modal-Key, Modal-Secret)` header pairs.
    #[must_use]
    pub fn headers(&self) -> [(&'static str, &str); 2] {
        [("Modal-Key", &self.id), ("Modal-Secret", &self.secret)]
    }
}

/// Attaches the env-configured proxy auth headers (if any) to a request.
pub fn apply(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match ProxyToken::from_env() {
        Some(token) => token
            .headers()
            .into_iter()
            .fold(request, |req, (name, value)| req.header(name, value)),
        None => request,
    }
}

/// Actionable message for a `401` from a Modal URL, `None` for other codes.
#[must_use]
pub fn hint_for_status(status: u16, token_configured: bool) -> Option<String> {
    if status != 401 {
        return None;
    }
    let tail = if token_configured {
        "The configured token was rejected: check that it is current and belongs to the \
         workspace that owns this app."
    } else {
        "Create a Proxy Auth Token in the Modal dashboard (Settings -> Proxy Auth Tokens) and \
         export MODAL_PROXY_TOKEN_ID / MODAL_PROXY_TOKEN_SECRET."
    };
    Some(format!(
        "Modal proxy rejected the request (401): Modal Servers require proxy auth. {tail}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_headers_without_both_values() {
        assert!(ProxyToken::from_values(None, None).is_none());
        assert!(ProxyToken::from_values(Some("wk-1".into()), None).is_none());
        assert!(ProxyToken::from_values(None, Some("ws-1".into())).is_none());
        assert!(ProxyToken::from_values(Some("wk-1".into()), Some("  ".into())).is_none());
    }

    #[test]
    fn headers_when_both_set() {
        let t = ProxyToken::from_values(Some("wk-1".into()), Some("ws-2".into())).unwrap();
        assert_eq!(
            t.headers(),
            [("Modal-Key", "wk-1"), ("Modal-Secret", "ws-2")]
        );
    }

    #[test]
    fn debug_redacts_secret_and_id() {
        let t = ProxyToken::from_values(Some("wk-visible".into()), Some("ws-topsecret".into()))
            .unwrap();
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("ws-topsecret"));
        assert!(!dbg.contains("wk-visible"));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn hint_only_for_401_and_never_leaks() {
        assert!(hint_for_status(200, false).is_none());
        assert!(hint_for_status(503, false).is_none());
        let unset = hint_for_status(401, false).unwrap();
        assert!(unset.contains("MODAL_PROXY_TOKEN_ID"));
        assert!(unset.contains("MODAL_PROXY_TOKEN_SECRET"));
        assert!(unset.contains("Proxy Auth Tokens"));
        let set = hint_for_status(401, true).unwrap();
        assert!(set.contains("rejected"));
    }
}
