//! reqwest-backed JWKS provider for the offline verifier.
//!
//! The verifier owns caching/refetch policy; this only performs one HTTP GET of
//! the JWKS document when asked. rustls backend, no system OpenSSL.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::{AuthError, Result};
use crate::jwks::Jwks;
use crate::verifier::JwksProvider;

/// Fetches a JWKS over HTTPS.
pub struct HttpJwksProvider {
    client: reqwest::Client,
    jwks_uri: String,
}

impl HttpJwksProvider {
    /// Provider for a fixed JWKS URI. Uses a default request timeout so a hung
    /// issuer cannot stall verify calls; see [`Self::with_client`] to override.
    ///
    /// TODO(amicu-planning#95): also cap the response body size.
    #[must_use]
    pub fn new(jwks_uri: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
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
        self.client
            .get(&self.jwks_uri)
            .send()
            .await
            .map_err(|_| AuthError::JwksFetch)?
            .error_for_status()
            .map_err(|_| AuthError::JwksFetch)?
            .json::<Jwks>()
            .await
            .map_err(|_| AuthError::JwksFetch)
    }
}
