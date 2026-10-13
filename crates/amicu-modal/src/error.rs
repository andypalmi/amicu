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

    /// A `ServerSpec` field is outside the range Modal's own client validates
    /// against (`modal/_server.py:22-35`).
    #[error("invalid Modal server spec: {field} {detail}")]
    InvalidSpec {
        /// The offending `ServerSpec` field name.
        field: &'static str,
        /// What the constraint is, and where it comes from.
        detail: String,
    },

    /// A GPU request string (e.g. `"H100:2"`) could not be parsed into a
    /// type/count pair.
    #[error("invalid Modal GPU request {spec:?}; expected e.g. \"H100\" or \"H100:2\"")]
    InvalidGpu {
        /// The offending request string.
        spec: String,
    },

    /// Modal's image builder did not finish successfully.
    #[error("Modal image build for {image_id} failed: {detail}")]
    ImageBuild {
        /// The image id Modal assigned (`im-…`).
        image_id: String,
        /// Modal's failure text, or a pointer at the build logs.
        detail: String,
    },

    /// `FunctionCreate` succeeded but returned no server endpoint, so there is
    /// nothing to send requests to.
    #[error("Modal created the server function but returned no endpoint URL")]
    MissingServerUrl,

    /// An RPC returned a non-OK, non-UNAUTHENTICATED gRPC status.
    ///
    /// Only the status `code` and `message` are kept, never the raw
    /// `tonic::Status`: its `Debug` output includes the response metadata,
    /// which carries Modal's server-issued `x-modal-auth-token` session JWT.
    #[error("Modal RPC {rpc} failed: {code:?}: {message}")]
    Rpc {
        /// The RPC method name (e.g. `"ClientHello"`).
        rpc: &'static str,
        /// The gRPC status code.
        code: Code,
        /// The status message (server-provided text; no metadata).
        message: String,
    },
}

impl ModalError {
    /// Whether Modal answered that the addressed object does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Rpc {
                code: Code::NotFound,
                ..
            }
        )
    }

    /// Maps a `tonic::Status` from `rpc` into a `ModalError`, giving
    /// `UNAUTHENTICATED` a message that points at the two env vars instead
    /// of the raw gRPC status text. The status's metadata is dropped (see
    /// [`ModalError::Rpc`]).
    pub(crate) fn from_status(rpc: &'static str, status: Status) -> Self {
        let (code, message) = (status.code(), status.message().to_owned());
        // Dropped here: the metadata must not outlive this function.
        drop(status);
        if code == Code::Unauthenticated {
            Self::Unauthenticated
        } else {
            Self::Rpc { rpc, code, message }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_error_never_leaks_response_metadata() {
        const TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.session-jwt-value.sig";
        let mut status = Status::failed_precondition("unable to determine the Python version");
        status
            .metadata_mut()
            .insert("x-modal-auth-token", TOKEN.parse().unwrap());

        let err = ModalError::from_status("FunctionCreate", status);
        match &err {
            ModalError::Rpc { rpc, code, message } => {
                assert_eq!(*rpc, "FunctionCreate");
                assert_eq!(*code, Code::FailedPrecondition);
                assert!(message.contains("Python version"));
            }
            other => panic!("expected Rpc, got {other:?}"),
        }
        let debug = format!("{err:?}");
        let display = format!("{err}");
        assert!(!debug.contains(TOKEN) && !debug.contains("x-modal-auth-token"));
        assert!(!display.contains(TOKEN) && !display.contains("x-modal-auth-token"));
        assert!(display.contains("FunctionCreate"));
    }
}
