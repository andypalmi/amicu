//! reqwest-backed JWKS provider for the offline verifier.
//!
//! The verifier owns caching/refetch policy; this only performs one HTTP GET of
//! the JWKS document when asked. rustls backend, no system OpenSSL.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::{AuthError, Result};
use crate::jwks::Jwks;
use crate::verifier::JwksProvider;

/// Whole-request timeout (connect + response) for the default client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on the JWKS response body. A real key set is a few hundred bytes per
/// key; 64 KiB leaves ample headroom while refusing to buffer an unbounded (or
/// hostile) body from a compromised/misbehaving issuer.
const MAX_JWKS_BYTES: usize = 64 * 1024;

/// Fetches a JWKS over HTTPS.
pub struct HttpJwksProvider {
    client: reqwest::Client,
    jwks_uri: String,
}

impl HttpJwksProvider {
    /// Provider for a fixed JWKS URI. Uses a default request timeout so a hung
    /// issuer cannot stall verify calls; see [`Self::with_client`] to override.
    /// The response body is capped at [`MAX_JWKS_BYTES`] regardless of client.
    #[must_use]
    pub fn new(jwks_uri: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            client,
            jwks_uri: jwks_uri.into(),
        }
    }

    /// Provider with a caller-supplied client (shared pools, timeouts, proxies).
    #[must_use]
    pub fn with_client(client: reqwest::Client, jwks_uri: impl Into<String>) -> Self {
        Self {
            client,
            jwks_uri: jwks_uri.into(),
        }
    }
}

#[async_trait]
impl JwksProvider for HttpJwksProvider {
    async fn fetch(&self) -> Result<Jwks> {
        let mut resp = self
            .client
            .get(&self.jwks_uri)
            .send()
            .await
            .map_err(|_| AuthError::JwksFetch)?
            .error_for_status()
            .map_err(|_| AuthError::JwksFetch)?;

        // Reject early on an advertised oversize body...
        if resp
            .content_length()
            .is_some_and(|len| len > MAX_JWKS_BYTES as u64)
        {
            return Err(AuthError::JwksFetch);
        }

        // ...and enforce the cap while streaming, since Content-Length may be
        // absent or untruthful. Never buffer more than MAX_JWKS_BYTES.
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| AuthError::JwksFetch)? {
            if body.len() + chunk.len() > MAX_JWKS_BYTES {
                return Err(AuthError::JwksFetch);
            }
            body.extend_from_slice(&chunk);
        }

        serde_json::from_slice::<Jwks>(&body).map_err(|_| AuthError::JwksFetch)
    }
}
