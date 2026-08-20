//! Argon2id password hashing.
//!
//! Hashes are PHC strings (`$argon2id$v=19$...`), safe to store. Verification is
//! constant-time (argon2's `verify_password` uses a constant-time comparison).

use amicu_core::secret::SecretValue;
use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

use crate::error::{AuthError, Result};

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
}
