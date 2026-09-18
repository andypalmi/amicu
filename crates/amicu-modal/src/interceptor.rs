//! gRPC request interceptor: attaches Modal's required auth/identity
//! metadata to every outgoing call.

use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::{Request, Status};

use crate::credentials::Credentials;
use crate::error::ModalError;

/// `x-modal-client-type`: `CLIENT_TYPE_LIBMODAL = 7`
/// (`proto/modal_proto/api.proto` ~line 89), the generic
/// non-language-specific client-library type. `CLIENT_TYPE_LIBMODAL_GO = 9`
/// / `_JS = 8` (~lines 90-91) exist too but are Modal's own Go/JS SDKs
/// (`modal-labs/libmodal`'s `modal-go/client.go:361` sends
/// `CLIENT_TYPE_LIBMODAL_GO`); amicu is neither, so it uses the shared
/// fallback instead of impersonating one of those SDKs.
const CLIENT_TYPE_LIBMODAL: &str = "7";

/// `x-modal-client-version`: libmodal-go hardcodes this exact string
/// (`modal-go/client.go:365`, comment "Behaves like this Python SDK
/// version") rather than its own SDK version — server behavior appears to
/// be gated on this looking like a Python SDK version, not on client
/// identity (`x-modal-client-type` is for that). amicu mirrors the value.
const CLIENT_VERSION: &str = "1.0.0";

/// Adds `x-modal-token-id`, `x-modal-token-secret`, `x-modal-client-type`,
/// and `x-modal-client-version` to every outgoing request's metadata.
#[derive(Clone)]
pub(crate) struct AuthInterceptor {
    token_id: MetadataValue<Ascii>,
    token_secret: MetadataValue<Ascii>,
    client_type: MetadataValue<Ascii>,
    client_version: MetadataValue<Ascii>,
}

impl AuthInterceptor {
    pub(crate) fn new(credentials: &Credentials) -> Result<Self, ModalError> {
        let token_id = MetadataValue::try_from(credentials.token_id.as_str()).map_err(|_| {
            ModalError::InvalidCredential {
                which: "MODAL_TOKEN_ID",
            }
        })?;
        let token_secret =
            MetadataValue::try_from(credentials.token_secret.as_str()).map_err(|_| {
                ModalError::InvalidCredential {
                    which: "MODAL_TOKEN_SECRET",
                }
            })?;
        Ok(Self {
            token_id,
            token_secret,
            client_type: MetadataValue::from_static(CLIENT_TYPE_LIBMODAL),
            client_version: MetadataValue::from_static(CLIENT_VERSION),
        })
    }
}

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let metadata = request.metadata_mut();
        metadata.insert("x-modal-token-id", self.token_id.clone());
        metadata.insert("x-modal-token-secret", self.token_secret.clone());
        metadata.insert("x-modal-client-type", self.client_type.clone());
        metadata.insert("x-modal-client-version", self.client_version.clone());
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            token_id: "ak-test-id".to_owned(),
            token_secret: "as-test-secret".to_owned(),
        }
    }

    #[test]
    fn sets_exactly_the_expected_headers() {
        let mut interceptor = AuthInterceptor::new(&credentials()).expect("valid credentials");
        let request = interceptor.call(Request::new(())).expect("intercept ok");
        let metadata = request.metadata();
        let get = |key: &str| metadata.get(key).and_then(|v| v.to_str().ok());

        assert_eq!(get("x-modal-token-id"), Some("ak-test-id"));
        assert_eq!(get("x-modal-token-secret"), Some("as-test-secret"));
        assert_eq!(get("x-modal-client-type"), Some(CLIENT_TYPE_LIBMODAL));
        assert_eq!(get("x-modal-client-version"), Some(CLIENT_VERSION));
        // Exactly these four — nothing extra leaked (e.g. no auth-token
        // session header, which this PR does not implement).
        assert_eq!(metadata.len(), 4);
    }

    #[test]
    fn rejects_a_token_with_invalid_metadata_bytes() {
        // `AuthInterceptor` deliberately has no `Debug` impl (its fields
        // are the literal token id/secret) — match instead of
        // `.unwrap_err()`, which would require one.
        let mut bad = credentials();
        bad.token_secret = "line\nbreak".to_owned();
        let Err(err) = AuthInterceptor::new(&bad) else {
            panic!("expected an invalid-credential error");
        };
        assert!(matches!(
            err,
            ModalError::InvalidCredential {
                which: "MODAL_TOKEN_SECRET"
            }
        ));
        // The error message never echoes the offending value.
        assert!(!err.to_string().contains("line"));
    }
}
