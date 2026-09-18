//! The public `ModalClient`: connect, authenticate, and call `ClientHello`.

use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};

use crate::credentials::Credentials;
use crate::error::ModalError;
use crate::interceptor::AuthInterceptor;
use crate::pb::modal_client::modal_client_client::ModalClientClient;

/// Default Modal control-plane endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.modal.com:443";

/// Environment variable that overrides [`DEFAULT_ENDPOINT`] — set this to
/// point at a local mock server in tests.
pub const ENDPOINT_ENV_VAR: &str = "AMICU_MODAL_ENDPOINT";

/// The result of a successful `ClientHello` call.
#[derive(Debug, Clone, Default)]
pub struct Hello {
    /// A deprecation/compatibility warning from the server, if any.
    pub warning: Option<String>,
    /// Additional structured warnings the server attached to this session.
    pub server_warnings: Vec<String>,
}

impl Hello {
    /// `true` if the server returned no warnings at all.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.warning.is_none() && self.server_warnings.is_empty()
    }
}

/// An authenticated tonic client for Modal's control-plane gRPC API.
///
/// This PR only wires up `ClientHello` ([`ModalClient::hello`]); the wider
/// RPC surface is generated (see `crate::pb`) but not yet exposed — later
/// PRs against `andypalmi/amicu-planning#158` build on this connection.
pub struct ModalClient {
    inner: ModalClientClient<InterceptedService<Channel, AuthInterceptor>>,
    endpoint: String,
}

impl ModalClient {
    /// Resolves the target endpoint: [`ENDPOINT_ENV_VAR`] if set, otherwise
    /// [`DEFAULT_ENDPOINT`].
    #[must_use]
    pub fn resolve_endpoint() -> String {
        std::env::var(ENDPOINT_ENV_VAR).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned())
    }

    /// Builds the auth interceptor and opens a TLS channel to Modal. Does
    /// not itself perform an RPC — a bad token is not detected until the
    /// first call (e.g. [`ModalClient::hello`]).
    pub async fn connect(credentials: Credentials) -> Result<Self, ModalError> {
        let endpoint = Self::resolve_endpoint();
        let interceptor = AuthInterceptor::new(&credentials)?;

        let channel_endpoint =
            Endpoint::new(endpoint.clone()).map_err(|source| ModalError::InvalidEndpoint {
                endpoint: endpoint.clone(),
                source,
            })?;
        let channel = channel_endpoint
            .connect()
            .await
            .map_err(|source| ModalError::Connect {
                endpoint: endpoint.clone(),
                source,
            })?;

        let inner = ModalClientClient::with_interceptor(channel, interceptor);
        Ok(Self { inner, endpoint })
    }

    /// The endpoint this client is connected to.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Calls the `ClientHello` RPC, which is the minimal way to confirm the
    /// configured token is accepted by Modal's control plane.
    pub async fn hello(&mut self) -> Result<Hello, ModalError> {
        let response = self
            .inner
            .client_hello(())
            .await
            .map_err(|status| ModalError::from_status("ClientHello", status))?
            .into_inner();

        Ok(Hello {
            warning: (!response.warning.is_empty()).then_some(response.warning),
            server_warnings: response
                .server_warnings
                .into_iter()
                .map(|w| w.message)
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_endpoint_defaults_without_the_env_var() {
        assert!(
            std::env::var(ENDPOINT_ENV_VAR).is_err(),
            "test environment must not already export {ENDPOINT_ENV_VAR}"
        );
        assert_eq!(ModalClient::resolve_endpoint(), DEFAULT_ENDPOINT);
    }

    #[test]
    fn unauthenticated_status_maps_to_a_clear_error() {
        let status = tonic::Status::unauthenticated("bad token");
        let err = ModalError::from_status("ClientHello", status);
        assert!(matches!(err, ModalError::Unauthenticated));
        let message = err.to_string();
        assert!(message.contains("MODAL_TOKEN_ID") && message.contains("MODAL_TOKEN_SECRET"));
        // Never echoes the server's status text verbatim for this variant.
        assert!(!message.contains("bad token"));
    }

    #[test]
    fn other_statuses_map_to_the_generic_rpc_error() {
        let status = tonic::Status::unavailable("connection reset");
        let err = ModalError::from_status("ClientHello", status);
        assert!(matches!(
            err,
            ModalError::Rpc {
                rpc: "ClientHello",
                ..
            }
        ));
        assert!(err.to_string().contains("ClientHello"));
    }
}
