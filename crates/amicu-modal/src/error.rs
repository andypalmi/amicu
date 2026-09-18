//! The `amicu-modal` error type.

use tonic::{Code, Status};

/// Failures from building a channel, authenticating, or calling an RPC
/// against Modal's control plane.
#[derive(Debug, thiserror::Error)]
pub enum ModalError {
    /// A credential (`MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET`) is missing.
    #[error(transparent)]
    MissingCredential(#[from] crate::credentials::MissingCredential),

    /// A token contains bytes that are not valid in a gRPC metadata value
    /// (must be visible ASCII). The token itself is never included here.
    #[error("{which} is not a valid gRPC metadata value (must be visible ASCII)")]
    InvalidCredential {
        /// `"MODAL_TOKEN_ID"` or `"MODAL_TOKEN_SECRET"` — never the value.
        which: &'static str,
    },

    /// The configured endpoint (default or `AMICU_MODAL_ENDPOINT`) is not a
    /// usable gRPC destination.
    #[error("invalid Modal endpoint {endpoint:?}: {source}")]
    InvalidEndpoint {
        /// The endpoint string that failed to parse.
        endpoint: String,
        #[source]
        source: tonic::transport::Error,
    },

    /// The transport channel to Modal could not be established.
    #[error("failed to connect to Modal at {endpoint}: {source}")]
    Connect {
        /// The endpoint that was being connected to.
        endpoint: String,
        #[source]
        source: tonic::transport::Error,
    },

    /// Modal rejected the request's credentials.
    #[error(
        "Modal rejected the request as unauthenticated; check that MODAL_TOKEN_ID and \
         MODAL_TOKEN_SECRET are set and valid"
    )]
    Unauthenticated,

    /// An RPC returned a non-OK, non-UNAUTHENTICATED gRPC status.
    #[error("Modal RPC {rpc} failed: {status}")]
    Rpc {
        /// The RPC method name (e.g. `"ClientHello"`).
        rpc: &'static str,
        #[source]
        status: Status,
    },
}

impl ModalError {
    /// Maps a `tonic::Status` from `rpc` into a `ModalError`, giving
    /// `UNAUTHENTICATED` a message that points at the two env vars instead
    /// of the raw gRPC status text.
    pub(crate) fn from_status(rpc: &'static str, status: Status) -> Self {
        if status.code() == Code::Unauthenticated {
            Self::Unauthenticated
        } else {
            Self::Rpc { rpc, status }
        }
    }
}
