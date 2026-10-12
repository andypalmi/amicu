//! Argon2id password hashing.
//!
//! Hashes are PHC strings (`$argon2id$v=19$...`), safe to store. Verification is
//! constant-time (argon2's `verify_password` uses a constant-time comparison).

use amicu_core::secret::SecretValue;
use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

use crate::error::{AuthError, Result};

/// Minimum accepted password length, in Unicode scalar values.
///
/// A deliberately conservative floor: it rejects the most trivially guessable
/// secrets without frustrating passphrase users. It is a baseline, not a
/// substitute for a breach-corpus / entropy check.
pub const MIN_PASSWORD_LEN: usize = 8;

/// Maximum accepted password length, in BYTES. Bounds the input fed to Argon2
/// (and the work an unauthenticated caller can force per request); generous
/// enough for any passphrase or password-manager output.
pub const MAX_PASSWORD_LEN: usize = 256;

/// Cap on a password PRESENTED AT SIGN-IN, in bytes. Deliberately far above
/// [`MAX_PASSWORD_LEN`] so accounts created before that limit existed can
/// still sign in; it only bounds Argon2 input (the request body is capped
/// much lower anyway).
pub const MAX_SIGNIN_PASSWORD_LEN: usize = 4096;

/// Enforce the minimum password-strength policy at registration time.
///
/// Applied on sign-up only; sign-in never re-checks policy (an existing
/// password predates any later tightening, and re-checking would leak nothing
/// but would lock out valid users). The check runs before hashing so a weak
/// password never reaches Argon2.
///
/// # Errors
/// [`AuthError::InvalidCredentials`] for an empty password;
/// [`AuthError::PasswordTooLong`] above [`MAX_PASSWORD_LEN`] bytes;
/// [`AuthError::WeakPassword`] for a password below [`MIN_PASSWORD_LEN`] scalar
/// values.
///
// TODO(amicu-planning#96): strengthen beyond a length floor once a breach-corpus
// (k-anonymity HIBP range API) or an entropy estimator (zxcvbn) dependency is
// vetted for the MSRV-1.97, hermetic build. Kept a pure length check for now to
// avoid a network dependency in the auth hot path and to stay offline-testable.
pub fn check_password_policy(password: &SecretValue) -> Result<()> {
    if password.is_empty() {
        return Err(AuthError::InvalidCredentials);
    }
    if password.expose_secret().len() > MAX_PASSWORD_LEN {
        return Err(AuthError::PasswordTooLong);
    }
    // Count scalar values when the password is UTF-8; fall back to byte length
    // (non-UTF-8 secrets only make the floor stricter, never weaker).
    let len = password
        .expose_secret_str()
        .map_or_else(|_| password.expose_secret().len(), |s| s.chars().count());
    if len < MIN_PASSWORD_LEN {
        return Err(AuthError::WeakPassword);
    }
    Ok(())
}

/// Hash a password with Argon2id (default params) and return its PHC string.
///
/// Empty passwords are rejected up front.
///
/// # Errors
/// [`AuthError::InvalidCredentials`] for an empty password;
/// [`AuthError::PasswordHash`] on internal hashing failure.
pub fn hash_password(password: &SecretValue) -> Result<String> {
    if password.is_empty() {
        return Err(AuthError::InvalidCredentials);
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.expose_secret(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AuthError::PasswordHash)
}

/// Verify a password against a stored PHC hash. Constant-time; never leaks which
/// of user/password was wrong.
///
/// # Errors
/// [`AuthError::InvalidCredentials`] if the password is empty or does not match;
/// [`AuthError::PasswordHash`] if the stored hash cannot be parsed.
pub fn verify_password(password: &SecretValue, phc: &str) -> Result<()> {
    if password.is_empty() {
        return Err(AuthError::InvalidCredentials);
    }
    let parsed = PasswordHash::new(phc).map_err(|_| AuthError::PasswordHash)?;
    Argon2::default()
        .verify_password(password.expose_secret(), &parsed)
        .map_err(|_| AuthError::InvalidCredentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_argon2id_and_verifies() {
        let pw = SecretValue::new(b"correct horse battery staple".to_vec());
        let phc = hash_password(&pw).unwrap();
        assert!(phc.starts_with("$argon2id$"), "must use Argon2id: {phc}");
        assert!(verify_password(&pw, &phc).is_ok());
    }

    #[test]
    fn wrong_password_and_empty_rejected() {
        let phc = hash_password(&SecretValue::new(b"right-password".to_vec())).unwrap();
        assert!(matches!(
            verify_password(&SecretValue::new(b"wrong".to_vec()), &phc),
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            verify_password(&SecretValue::new(Vec::new()), &phc),
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            hash_password(&SecretValue::new(Vec::new())),
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[test]
    fn salts_differ_between_hashes() {
        let pw = SecretValue::new(b"same-password".to_vec());
        assert_ne!(hash_password(&pw).unwrap(), hash_password(&pw).unwrap());
    }

    #[test]
    fn policy_rejects_empty_and_short_passwords() {
        assert!(matches!(
            check_password_policy(&SecretValue::new(Vec::new())),
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            check_password_policy(&SecretValue::new(b"short".to_vec())),
            Err(AuthError::WeakPassword)
        ));
        // Exactly one below the floor is still rejected.
        assert!(matches!(
            check_password_policy(&SecretValue::new(vec![b'a'; MIN_PASSWORD_LEN - 1])),
            Err(AuthError::WeakPassword)
        ));
    }

    #[test]
    fn policy_accepts_passwords_at_or_above_floor() {
        check_password_policy(&SecretValue::new(vec![b'a'; MIN_PASSWORD_LEN])).unwrap();
        check_password_policy(&SecretValue::new(b"correct horse battery".to_vec())).unwrap();
    }

    #[test]
    fn policy_counts_scalar_values_not_bytes() {
        // Seven multi-byte characters: 14 bytes but only 7 scalar values, so it
        // must be rejected by the scalar-count floor of 8.
        let pw = SecretValue::new("é".repeat(7).into_bytes());
        assert_eq!(pw.expose_secret().len(), 14);
        assert_eq!(pw.expose_secret_str().unwrap().chars().count(), 7);
        assert!(matches!(
            check_password_policy(&pw),
            Err(AuthError::WeakPassword)
        ));
    }
}
