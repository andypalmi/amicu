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
    ///
    /// Internal only: the sign-up flow deliberately never surfaces this to a
    /// client (it would enable email enumeration). It is raised by the user
    /// store and swallowed by [`crate::service::AuthService::sign_up`].
    #[error("user already exists")]
    UserExists,
    /// Password does not meet the minimum strength policy.
    #[error("password does not meet policy")]
    WeakPassword,
    /// Refresh token is unknown, expired, or revoked.
    #[error("refresh token invalid")]
    RefreshInvalid,
    /// Reset token is unknown, expired, or already used.
    ///
    /// Deliberately generic: the password-reset flow renders every rejected
    /// token identically so it cannot be used to probe which tokens (or
    /// accounts) exist.
    #[error("reset token invalid")]
    ResetInvalid,
    /// Email-verification token is unknown, expired, or already used.
    ///
    /// Deliberately generic, like [`ResetInvalid`](AuthError::ResetInvalid): the
    /// verify flow renders every rejected token identically so it cannot be used
    /// to probe which tokens (or accounts) exist.
    #[error("verification token invalid")]
    VerifyInvalid,
    /// Credentials were correct but the account's email is not yet verified.
    ///
    /// Raised only when email verification is required (the
    /// `AMICU_REQUIRE_EMAIL_VERIFICATION` gate) and only AFTER the password has
    /// verified, so it never distinguishes a wrong password from an unverified
    /// account: a wrong password still yields the generic
    /// [`InvalidCredentials`](AuthError::InvalidCredentials).
    #[error("email not verified")]
    EmailUnverified,
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
    /// A persistence/backend operation failed (database unreachable, query
    /// error, and so on).
    ///
    /// Deliberately coarse and carries no query text, key, or token material.
    /// The persistent stores (see `amicu-store`) map driver errors to this; it
    /// maps to a 500 at the API boundary. Distinct from the domain rejections
    /// (`InvalidCredentials`, `RefreshInvalid`, ...) so an infrastructure
    /// outage is never mistaken for an authentication decision.
    #[error("storage backend error")]
    Backend,
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, AuthError>;
