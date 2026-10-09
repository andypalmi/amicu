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

use reqwest::header::HeaderValue;

/// Env var holding the Proxy Auth Token id (`wk-...`), sent as `Modal-Key`.
pub const TOKEN_ID_ENV_VAR: &str = "MODAL_PROXY_TOKEN_ID";
/// Env var holding the Proxy Auth Token secret (`ws-...`), sent as `Modal-Secret`.
pub const TOKEN_SECRET_ENV_VAR: &str = "MODAL_PROXY_TOKEN_SECRET";

/// A validated Modal Proxy Auth Token. No `Display`; `Debug` redacts both
/// halves, and the header values are marked sensitive.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyToken {
    id: HeaderValue,
    secret: HeaderValue,
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
    /// The `(Modal-Key, Modal-Secret)` header pairs, values marked sensitive.
    #[must_use]
    pub fn headers(&self) -> [(&'static str, HeaderValue); 2] {
        [
            ("Modal-Key", self.id.clone()),
            ("Modal-Secret", self.secret.clone()),
        ]
    }
}

/// What the environment holds for the two token variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenConfig {
    /// Neither variable is set (or both are blank).
    None,
    /// Exactly one is set; `missing` names the other.
    Partial {
        set: &'static str,
        missing: &'static str,
    },
    /// A value contains characters not allowed in an HTTP header.
    Invalid { var: &'static str },
    /// Both set and valid.
    Complete(ProxyToken),
}

/// Payload-free summary of a [`TokenConfig`], for hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    None,
    Partial {
        set: &'static str,
        missing: &'static str,
    },
    Invalid {
        var: &'static str,
    },
    Complete,
}

fn header_value(var: &'static str, raw: &str) -> Result<HeaderValue, TokenConfig> {
    let mut value = HeaderValue::from_str(raw).map_err(|_| TokenConfig::Invalid { var })?;
    value.set_sensitive(true);
    Ok(value)
}

impl TokenConfig {
    /// Pure constructor; surrounding whitespace is trimmed, blank counts as unset.
    #[must_use]
    pub fn from_values(id: Option<String>, secret: Option<String>) -> Self {
        let clean = |v: Option<String>| v.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        match (clean(id), clean(secret)) {
            (None, None) => Self::None,
            (Some(_), None) => Self::Partial {
                set: TOKEN_ID_ENV_VAR,
                missing: TOKEN_SECRET_ENV_VAR,
            },
            (None, Some(_)) => Self::Partial {
                set: TOKEN_SECRET_ENV_VAR,
                missing: TOKEN_ID_ENV_VAR,
            },
            (Some(id), Some(secret)) => {
                let id = match header_value(TOKEN_ID_ENV_VAR, &id) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                match header_value(TOKEN_SECRET_ENV_VAR, &secret) {
                    Ok(secret) => Self::Complete(ProxyToken { id, secret }),
                    Err(e) => e,
                }
            }
        }
    }

    /// Reads [`TOKEN_ID_ENV_VAR`] / [`TOKEN_SECRET_ENV_VAR`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var(TOKEN_ID_ENV_VAR).ok(),
            std::env::var(TOKEN_SECRET_ENV_VAR).ok(),
        )
    }

    /// Payload-free state.
    #[must_use]
    pub fn state(&self) -> TokenState {
        match self {
            Self::None => TokenState::None,
            Self::Partial { set, missing } => TokenState::Partial { set, missing },
            Self::Invalid { var } => TokenState::Invalid { var },
            Self::Complete(_) => TokenState::Complete,
        }
    }
}

/// Attaches the env-configured proxy auth headers (only when the token is
/// complete and valid) to a request. Value bytes are marked sensitive.
pub fn apply(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match TokenConfig::from_env() {
        TokenConfig::Complete(token) => token
            .headers()
            .into_iter()
            .fold(request, |req, (name, value)| req.header(name, value)),
        _ => request,
    }
}

/// State of the environment, for [`hint_for_status`].
#[must_use]
pub fn env_state() -> TokenState {
    TokenConfig::from_env().state()
}

/// Actionable message for a `401`/`403` from a Modal URL, `None` otherwise.
/// Never contains a token value.
#[must_use]
pub fn hint_for_status(status: u16, state: TokenState) -> Option<String> {
    let tail = match (status, state) {
        (401, TokenState::None) => {
            "Modal Servers require proxy auth. Create a Proxy Auth Token in the Modal \
             dashboard (Settings -> Proxy Auth Tokens) and export MODAL_PROXY_TOKEN_ID / \
             MODAL_PROXY_TOKEN_SECRET."
                .to_owned()
        }
        (401, TokenState::Partial { set, missing }) => {
            format!("{set} is set but {missing} is not; export both.")
        }
        (401, TokenState::Invalid { var }) => format!(
            "{var} contains characters not allowed in an HTTP header (check for whitespace or \
             newlines), so no token was sent."
        ),
        (401, TokenState::Complete) => "The configured token was rejected: check that it is \
             current and belongs to the workspace that owns this app."
            .to_owned(),
        (403, _) => "The proxy token was rejected (403): it may be scoped to a different \
             workspace/environment."
            .to_owned(),
        _ => return None,
    };
    Some(format!(
        "Modal proxy rejected the request ({status}): {tail}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(id: Option<&str>, secret: Option<&str>) -> TokenConfig {
        TokenConfig::from_values(id.map(Into::into), secret.map(Into::into))
    }

    #[test]
    fn none_partial_complete() {
        assert_eq!(cfg(None, None), TokenConfig::None);
        assert_eq!(cfg(Some(" "), None), TokenConfig::None);
        assert_eq!(
            cfg(Some("wk-1"), None).state(),
            TokenState::Partial {
                set: TOKEN_ID_ENV_VAR,
                missing: TOKEN_SECRET_ENV_VAR
            }
        );
        assert_eq!(
            cfg(None, Some("ws-1")).state(),
            TokenState::Partial {
                set: TOKEN_SECRET_ENV_VAR,
                missing: TOKEN_ID_ENV_VAR
            }
        );
        assert_eq!(
            cfg(Some(" wk-1\n"), Some("ws-2")).state(),
            TokenState::Complete
        );
    }

    #[test]
    fn headers_are_sensitive_and_correct() {
        let TokenConfig::Complete(t) = cfg(Some("wk-1"), Some("ws-2")) else {
            panic!("expected complete");
        };
        let [(k, kv), (s, sv)] = t.headers();
        assert_eq!((k, kv.to_str().unwrap()), ("Modal-Key", "wk-1"));
        assert_eq!((s, sv.to_str().unwrap()), ("Modal-Secret", "ws-2"));
        assert!(kv.is_sensitive() && sv.is_sensitive());
    }

    #[test]
    fn invalid_header_chars_are_reported_without_the_value() {
        let c = cfg(Some("wk-1"), Some("ws-\nbad"));
        assert_eq!(
            c,
            TokenConfig::Invalid {
                var: TOKEN_SECRET_ENV_VAR
            }
        );
        let hint = hint_for_status(401, c.state()).unwrap();
        assert!(hint.contains("MODAL_PROXY_TOKEN_SECRET contains characters"));
        assert!(!hint.contains("bad"));
        assert!(!format!("{c:?}").contains("bad"));
    }

    #[test]
    fn debug_redacts_secret_and_id() {
        let c = cfg(Some("wk-visible"), Some("ws-topsecret"));
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("ws-topsecret"));
        assert!(!dbg.contains("wk-visible"));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn hints_by_status_and_state() {
        assert!(hint_for_status(200, TokenState::None).is_none());
        assert!(hint_for_status(503, TokenState::Complete).is_none());
        let none = hint_for_status(401, TokenState::None).unwrap();
        assert!(none.contains("MODAL_PROXY_TOKEN_ID") && none.contains("Proxy Auth Tokens"));
        let partial = hint_for_status(
            401,
            TokenState::Partial {
                set: TOKEN_ID_ENV_VAR,
                missing: TOKEN_SECRET_ENV_VAR,
            },
        )
        .unwrap();
        assert!(
            partial.contains("MODAL_PROXY_TOKEN_ID is set but MODAL_PROXY_TOKEN_SECRET is not")
        );
        assert!(
            hint_for_status(401, TokenState::Complete)
                .unwrap()
                .contains("rejected")
        );
        let forbidden = hint_for_status(403, TokenState::Complete).unwrap();
        assert!(forbidden.contains("(403)") && forbidden.contains("different workspace"));
    }
}
