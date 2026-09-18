//! Modal API token credentials.

/// A Modal API token pair (`MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET`).
///
/// The `Debug` impl redacts `token_secret` — never derive `Debug` on this
/// type or print `token_secret` directly.
#[derive(Clone)]
pub struct Credentials {
    /// The `MODAL_TOKEN_ID` value.
    pub token_id: String,
    /// The `MODAL_TOKEN_SECRET` value. Never logged or printed.
    pub token_secret: String,
}

impl Credentials {
    /// Reads credentials from `MODAL_TOKEN_ID` and `MODAL_TOKEN_SECRET`.
    ///
    /// # Errors
    ///
    /// Returns the name of the first missing variable (never its value —
    /// there is none to leak for a missing var, but callers should still
    /// avoid echoing this into logs verbatim in case the message wording
    /// changes later).
    pub fn from_env() -> Result<Self, MissingCredential> {
        let token_id =
            std::env::var("MODAL_TOKEN_ID").map_err(|_| MissingCredential("MODAL_TOKEN_ID"))?;
        let token_secret = std::env::var("MODAL_TOKEN_SECRET")
            .map_err(|_| MissingCredential("MODAL_TOKEN_SECRET"))?;
        Ok(Self {
            token_id,
            token_secret,
        })
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("token_id", &self.token_id)
            .field("token_secret", &"<redacted>")
            .finish()
    }
}

/// A required environment variable was not set.
#[derive(Debug, thiserror::Error)]
#[error("{0} is not set")]
pub struct MissingCredential(pub &'static str);
