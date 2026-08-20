//! reqwest-backed JWKS provider for the offline verifier.
//!
//! The verifier owns caching/refetch policy; this only performs one HTTP GET of
//! the JWKS document when asked. rustls backend, no system OpenSSL.

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
    /// Provider for a fixed JWKS URI.
    #[must_use]
    pub fn new(jwks_uri: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
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
