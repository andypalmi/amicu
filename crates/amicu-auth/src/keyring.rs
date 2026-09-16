//! Active + retiring signing keys, and the JWKS built from their public halves.
//!
//! Rotation keeps the previous key(s) published so tokens they signed keep
//! verifying until they expire; a new `kid` becomes the signer.

use crate::jwks::Jwks;
use crate::key::SigningKey;

/// The service's signing keys. The first entry is active (signs new tokens);
/// the rest are retiring keys retained for verification/publication.
#[derive(Debug)]
pub struct Keyring {
    keys: Vec<SigningKey>,
}

impl Keyring {
    /// A keyring with a single active key.
    #[must_use]
    pub fn new(active: SigningKey) -> Self {
        Self { keys: vec![active] }
    }

    /// Build a keyring with one active key and zero or more already-retiring
    /// keys (verify-only from the start; they never sign new tokens —
    /// `active()` is always `active`). Order among `retiring` is preserved but
    /// otherwise immaterial: every key in it is equally verify-only. Used at
    /// boot to load a durable keyring where some retiring key(s) are already
    /// known (rotation mid-flight) — see `amicu-server`'s `signing_key`
    /// module.
    #[must_use]
    pub fn with_retiring(active: SigningKey, retiring: Vec<SigningKey>) -> Self {
        let mut keys = Vec::with_capacity(1 + retiring.len());
        keys.push(active);
        keys.extend(retiring);
        Self { keys }
    }

    /// The key that signs new tokens.
    #[must_use]
    pub fn active(&self) -> &SigningKey {
        &self.keys[0]
    }

    /// All key ids currently published, active first.
    #[must_use]
    pub fn kids(&self) -> Vec<String> {
        self.keys.iter().map(|k| k.kid().to_string()).collect()
    }

    /// Introduce a new active key. The previous active key is retained (and
    /// still published) so its tokens verify until they expire.
    pub fn rotate_in(&mut self, new_active: SigningKey) {
        self.keys.insert(0, new_active);
    }

    /// Drop a retiring key by kid once all tokens it signed have expired.
    /// The active key (index 0) cannot be retired.
    pub fn retire(&mut self, kid: &str) {
        if self.active().kid() == kid {
            return;
        }
        self.keys.retain(|k| k.kid() != kid);
    }

    /// The public JWKS (active + retiring keys).
    #[must_use]
    pub fn jwks(&self) -> Jwks {
        Jwks {
            keys: self.keys.iter().map(SigningKey::public_jwk).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_keeps_old_key_and_makes_new_active() {
        let first = SigningKey::generate().unwrap();
        let first_kid = first.kid().to_string();
        let mut ring = Keyring::new(first);

        let second = SigningKey::generate().unwrap();
        let second_kid = second.kid().to_string();
        ring.rotate_in(second);

        assert_eq!(ring.active().kid(), second_kid);
        assert_eq!(ring.jwks().keys.len(), 2);
        assert!(ring.kids().contains(&first_kid));
    }

    #[test]
    fn retire_drops_old_key_but_never_active() {
        let a = SigningKey::generate().unwrap();
        let a_kid = a.kid().to_string();
        let mut ring = Keyring::new(a);
        let b = SigningKey::generate().unwrap();
        let b_kid = b.kid().to_string();
        ring.rotate_in(b);

        ring.retire(&a_kid);
        assert_eq!(ring.jwks().keys.len(), 1);
        assert_eq!(ring.active().kid(), b_kid);

        // Retiring the active key is a no-op.
        ring.retire(&b_kid);
        assert_eq!(ring.jwks().keys.len(), 1);
    }

    #[test]
    fn with_retiring_builds_active_plus_retiring_set() {
        let active = SigningKey::generate().unwrap();
        let active_kid = active.kid().to_string();
        let r1 = SigningKey::generate().unwrap();
        let r1_kid = r1.kid().to_string();
        let r2 = SigningKey::generate().unwrap();
        let r2_kid = r2.kid().to_string();

        let ring = Keyring::with_retiring(active, vec![r1, r2]);

        assert_eq!(ring.active().kid(), active_kid);
        assert_eq!(ring.jwks().keys.len(), 3);
        let kids = ring.kids();
        assert!(kids.contains(&active_kid));
        assert!(kids.contains(&r1_kid));
        assert!(kids.contains(&r2_kid));
    }
}
