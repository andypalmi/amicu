//! Auth error taxonomy. Messages are deliberately coarse: no password,
//! token, or key material is ever placed in an error value.

use thiserror::Error;

/// Errors surfaced by issuance and verification.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Token is malformed or fails a structural check.
    #[error("invalid token")]
    InvalidToken,
    /// Signature did not verify against the selected key.
    #[error("token signature verification failed")]
    SignatureInvalid,
    /// `exp` is in the past.
    #[error("token expired")]
    Expired,
    /// `nbf` is in the future.
    #[error("token not yet valid")]
    NotYetValid,
    /// `aud` did not match the verifier's expected audience.
    #[error("token audience rejected")]
    AudienceRejected,
    /// `iss` did not match the verifier's expected issuer.
    #[error("token issuer rejected")]
    IssuerRejected,
    /// The token's `kid` is not present in the JWKS, even after a refetch.
    #[error("unknown key id")]
    UnknownKid,
    /// Fetching the JWKS from the provider failed.
    #[error("jwks fetch failed")]
    JwksFetch,
    /// Email/password did not authenticate.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// Sign-up for an already-registered email.
    #[error("user already exists")]
    UserExists,
    /// Refresh token is unknown, expired, or revoked.
    #[error("refresh token invalid")]
    RefreshInvalid,
    /// Password hashing/verification failed internally.
    #[error("password hashing failure")]
    PasswordHash,
    /// Key generation or PKCS#8 handling failed.
    #[error("key generation failure")]
    KeyGen,
    /// Rate limit tripped on an auth endpoint.
    #[error("rate limit exceeded")]
    RateLimited,
    /// Signing failed.
    #[error("token signing failure")]
    Signing,
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, AuthError>;
