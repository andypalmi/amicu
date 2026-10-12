//! Durable signing-key loading: `AMICU_SIGNING_KEY_PEM` (+`_FILE`) and
//! `AMICU_SIGNING_KEYS_RETIRING_PEM` (+`_FILE`) → a [`Keyring`] shared by
//! every replica that carries the same secret-injected env
//! (andypalmi/amicu-planning#140). See the crate docs and
//! `docs/ops/signing-key-rotation.md` for the operational rotation
//! procedure.
//!
//! # Security discipline
//!
//! - The raw PEM value is wrapped in [`RedactedEnvString`] the instant it
//!   would otherwise touch a bare `String`-typed, `Debug`+`Clone`-deriving
//!   `Config` field: its `Debug` renders `SecretValue(***)` (delegating to
//!   [`amicu_core::secret::SecretValue`]), so an accidental `{config:?}` (a
//!   log line, a panic message, a failed test assertion) cannot leak key
//!   material, and its `Clone` never leaves an un-zeroized copy lying around.
//! - Every error variant here echoes only non-secret data: env/flag names,
//!   file paths, `kid`s (derived from the PUBLIC key), and (for a bad key)
//!   [`AuthError`]'s already-redacted `Display`. Never the PEM text itself.
//! - [`load_keyring`] logs (via `eprintln!`, matching the rest of this
//!   crate's startup banner style) ONLY the active key's `kid`, the retiring
//!   count, and their `kid`s — never PEM/key bytes.
//! - Unlike `smtp.rs`'s `non_empty_env` (which treats a set-but-empty env var
//!   as absent), a set-but-empty `AMICU_SIGNING_KEY_PEM`/`_FILE` value is
//!   NOT treated as absent here — it fails loud as an invalid key. This is
//!   deliberate: an operator who accidentally blanks the secret in
//!   production must get a crash, not a silent fall-through to an ephemeral,
//!   per-boot key that would differ across replicas.

use std::collections::HashSet;

use amicu_auth::error::AuthError;
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_core::secret::SecretValue;
use zeroize::Zeroizing;

use crate::{Config, Environment};

/// A `clap`-parsable, redacted wrapper around a raw signing-key PEM value
/// read from `AMICU_SIGNING_KEY_PEM` / `AMICU_SIGNING_KEYS_RETIRING_PEM`.
///
/// `Config` derives `Debug` and `Clone` like every other field. Wrapping the
/// raw PEM in [`SecretValue`] here — instead of a bare `String` — means:
/// - an accidental `{config:?}` renders `SecretValue(***)`, never the key;
/// - `Clone` produces an independent, separately zeroize-on-drop copy rather
///   than aliasing a `String` that never zeroizes at all.
///
/// This mirrors the discipline [`amicu_auth::key::SigningKey`] documents for
/// the PARSED key, applied one step earlier, at the `clap` field boundary,
/// before parsing even happens.
#[derive(Clone)]
pub struct RedactedEnvString(SecretValue);

impl RedactedEnvString {
    /// Wrap a raw value (used by `clap`'s value parser, and directly by
    /// tests/callers that build a [`Config`] programmatically).
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(SecretValue::new(value.into()))
    }

    /// Borrow the wrapped secret.
    #[must_use]
    pub fn secret(&self) -> &SecretValue {
        &self.0
    }
}

impl std::str::FromStr for RedactedEnvString {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl std::fmt::Debug for RedactedEnvString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

/// A failure loading the signing keyring from the environment. Never carries
/// PEM/key material — only env/flag names, file paths, `kid`s, and
/// [`AuthError`]'s own redacted display.
#[derive(Debug)]
pub enum SigningKeyConfigError {
    /// Both the value and file variant of the same logical key (active or
    /// retiring) were set. Exactly one source is allowed. Variable names are
    /// echoed (not secrets).
    ConflictingSource {
        /// The `_PEM` value env var name.
        value_var: &'static str,
        /// The `_PEM_FILE` path env var name.
        file_var: &'static str,
    },
    /// A `_FILE` path could not be read. The path and the IO error's message
    /// are echoed (a path is not a secret; `std::io::Error`'s `Display`
    /// carries no key material).
    FileRead {
        /// The `_FILE` env var name.
        var: &'static str,
        /// The offending path (not a secret).
        path: String,
        /// The IO error's message (not a secret).
        error: String,
    },
    /// A PEM value/file was not valid UTF-8.
    InvalidUtf8 {
        /// The offending env var name.
        var: &'static str,
    },
    /// A PEM block was not a valid PKCS#8 Ed25519 private key.
    InvalidKey {
        /// The offending env var name.
        var: &'static str,
        /// The underlying (already-redacted) auth error.
        source: AuthError,
    },
    /// Two loaded keys (active+retiring, or two retiring) share a `kid` — the
    /// same key material was supplied twice. The `kid` (derived from the
    /// PUBLIC key) is echoed; it is not secret.
    DuplicateKid(String),
    /// `AMICU_ENV=production` but no active key was configured. The caller
    /// (`amicu_server::build_app_with_database_url`) maps this to the flat
    /// `Error::MissingSigningKey`.
    MissingActiveKey,
    /// The `_FILE` variable's value looks like inline PEM key material, not a
    /// file path (the classic mix-up: setting the `_FILE` var to actual key
    /// text instead of `AMICU_SIGNING_KEY_PEM`/`AMICU_SIGNING_KEYS_RETIRING_PEM`
    /// directly). Deliberately does NOT echo the value anywhere - if it really
    /// is a private key, printing any part of it would defeat the entire
    /// redaction discipline this module exists to enforce.
    FileValueLooksLikeKey {
        /// The `_FILE` env var name whose value looked like inline key
        /// material.
        var: &'static str,
        /// The non-`_FILE` env var name the operator probably meant to set.
        value_var: &'static str,
    },
    /// A signing-key PEM value was set (non-empty after normalisation) but
    /// contained no `-----BEGIN` marker, so zero PEM blocks were parsed. This
    /// is almost always a misconfiguration (unarmored secret, shell-quoting
    /// mangled it, etc.) and is deliberately a hard error rather than a
    /// silent "no retiring keys": the RETIRING variable's entire job during a
    /// rotation is to keep an outgoing key verifying, and silently loading
    /// zero of them means the old key just stops verifying with no error
    /// signal at all. An explicitly-EMPTY value (nothing but whitespace) is
    /// NOT affected by this - that stays the valid "no retiring keys" case.
    NoPemBlocks {
        /// The offending env var name.
        var: &'static str,
    },
}

impl std::fmt::Display for SigningKeyConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConflictingSource {
                value_var,
                file_var,
            } => write!(
                f,
                "both {value_var} and {file_var} are set; exactly one source per key is allowed"
            ),
            Self::FileRead { var, path, error } => {
                write!(
                    f,
                    "{var} points at '{path}' which could not be read: {error}"
                )
            }
            Self::InvalidUtf8 { var } => write!(f, "{var} is not valid UTF-8"),
            Self::InvalidKey { var, source } => {
                write!(
                    f,
                    "{var} is not a valid PKCS#8 Ed25519 private key: {source}"
                )
            }
            Self::DuplicateKid(kid) => write!(
                f,
                "duplicate signing key: kid '{kid}' appears more than once across the active/retiring set"
            ),
            Self::MissingActiveKey => write!(
                f,
                "no active signing key configured (set AMICU_SIGNING_KEY_PEM or \
                 AMICU_SIGNING_KEY_PEM_FILE)"
            ),
            Self::FileValueLooksLikeKey { var, value_var } => write!(
                f,
                "{var} looks like inline PEM key material, not a file path - did you mean to \
                 set {value_var} instead? (value withheld)"
            ),
            Self::NoPemBlocks { var } => write!(
                f,
                "{var} is set but contains no '-----BEGIN' PEM marker (0 keys parsed) - this is \
                 almost always a misconfiguration, not an intentionally-empty value"
            ),
        }
    }
}

impl std::error::Error for SigningKeyConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidKey { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Heuristic: does `path` look like inline PEM key material rather than a
/// filesystem path? Catches the classic mix-up of setting a `_FILE` env var
/// to actual key text instead of the plain `_PEM` var. Deliberately
/// conservative (a few false positives on a bizarre-but-real path are an
/// acceptable cost against ever echoing real key material in an error).
fn looks_like_inline_key(path: &str) -> bool {
    path.contains("PRIVATE KEY") || path.contains('\n') || path.contains("\\n") || path.len() > 512
}

/// Resolve one logical key's raw PEM source: at most one of `value`/`file`
/// may be set.
fn resolve_source(
    value: Option<&RedactedEnvString>,
    file: Option<&str>,
    value_var: &'static str,
    file_var: &'static str,
) -> Result<Option<SecretValue>, SigningKeyConfigError> {
    match (value, file) {
        (Some(_), Some(_)) => Err(SigningKeyConfigError::ConflictingSource {
            value_var,
            file_var,
        }),
        (Some(v), None) => Ok(Some(v.secret().clone())),
        (None, Some(path)) => {
            // Guard BEFORE ever reading/echoing `path`: if it looks like
            // inline key material rather than a filesystem path, the
            // operator almost certainly meant to set `value_var` instead.
            // Reject without echoing anything derived from `path` - if it
            // really is a private key, even a fragment of it in an error
            // would defeat this whole module's redaction discipline.
            if looks_like_inline_key(path) {
                return Err(SigningKeyConfigError::FileValueLooksLikeKey {
                    var: file_var,
                    value_var,
                });
            }
            let contents =
                std::fs::read_to_string(path).map_err(|e| SigningKeyConfigError::FileRead {
                    var: file_var,
                    path: path.to_string(),
                    error: e.to_string(),
                })?;
            Ok(Some(SecretValue::new(contents)))
        }
        (None, None) => Ok(None),
    }
}

/// Normalise a PEM value read from the environment: real or literal-escaped
/// CRLF/LF all become a real `\n`. Secret-manager / Terraform / shell-quoting
/// paths that cannot embed a real newline in an env var commonly emit the
/// two-character escape `\` + `n` (or `\r\n`) instead of an actual newline; a
/// PEM that already contains real newlines is unaffected (that literal
/// escape sequence does not occur inside a normal multi-line PEM body).
fn normalize_pem(
    raw: &SecretValue,
    var: &'static str,
) -> Result<SecretValue, SigningKeyConfigError> {
    let text = raw
        .expose_secret_str()
        .map_err(|_| SigningKeyConfigError::InvalidUtf8 { var })?;
    // N-1 fix: each `.replace()` step below used to produce a plain,
    // never-zeroized `String` holding the (still-normalizing) key material,
    // with every earlier copy simply leaked to freed-but-unwiped heap memory
    // as the chain progressed. Reassigning through a `Zeroizing<String>`
    // instead means each reassignment's implicit drop of the PREVIOUS value
    // wipes it before the next copy is made. The final `.as_bytes().to_vec()`
    // clones the finished, normalized bytes into the `SecretValue` that owns
    // them from here on; `normalized` itself is then dropped (at the end of
    // this function) and zeroized like every intermediate before it - no
    // `unsafe` needed to move the bytes out without a final zeroize-defeating
    // extraction.
    let mut normalized = Zeroizing::new(text.replace("\\r\\n", "\n"));
    normalized = Zeroizing::new(normalized.replace("\\n", "\n"));
    normalized = Zeroizing::new(normalized.replace("\r\n", "\n"));
    normalized = Zeroizing::new(normalized.replace('\r', "\n"));
    Ok(SecretValue::new(normalized.as_bytes().to_vec()))
}

/// Split a normalised blob that may contain zero or more concatenated PKCS#8
/// PEM blocks into individual block strings, splitting on `-----BEGIN`. Each
/// returned block starts with `-----BEGIN` and is trimmed; empty fragments
/// (e.g. leading boilerplate before the first block) are dropped.
///
/// N-1 fix: each block is a `Zeroizing<String>` rather than a bare `String`,
/// so the untrimmed intermediate `format!` copy is wiped the instant the
/// trimmed final copy is built, and the returned blocks stay zeroize-on-drop
/// end to end until each is handed to `SecretValue::new` in `load_keyring`.
fn split_pem_blocks(normalized: &str) -> Vec<Zeroizing<String>> {
    normalized
        .split("-----BEGIN")
        .skip(1)
        .map(|chunk| {
            let untrimmed = Zeroizing::new(format!("-----BEGIN{chunk}"));
            Zeroizing::new(untrimmed.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Load the durable signing keyring from `config` (see the module docs and
/// `docs/ops/signing-key-rotation.md`).
///
/// In `production`, a missing active key is
/// [`SigningKeyConfigError::MissingActiveKey`]. In `development`, a missing
/// active key falls back to an ephemeral [`SigningKey::generate`], with a
/// loud warning — unchanged dev UX from before this change.
///
/// # Errors
/// See [`SigningKeyConfigError`].
pub(crate) fn load_keyring(config: &Config) -> Result<Keyring, SigningKeyConfigError> {
    let active_raw = resolve_source(
        config.signing_key_pem.as_ref(),
        config.signing_key_pem_file.as_deref(),
        "AMICU_SIGNING_KEY_PEM",
        "AMICU_SIGNING_KEY_PEM_FILE",
    )?;
    let retiring_raw = resolve_source(
        config.signing_keys_retiring_pem.as_ref(),
        config.signing_keys_retiring_pem_file.as_deref(),
        "AMICU_SIGNING_KEYS_RETIRING_PEM",
        "AMICU_SIGNING_KEYS_RETIRING_PEM_FILE",
    )?;

    let active = match active_raw {
        Some(raw) => {
            let normalized = normalize_pem(&raw, "AMICU_SIGNING_KEY_PEM")?;
            SigningKey::from_pkcs8_pem(&normalized).map_err(|source| {
                SigningKeyConfigError::InvalidKey {
                    var: "AMICU_SIGNING_KEY_PEM",
                    source,
                }
            })?
        }
        None if config.environment == Environment::Production => {
            return Err(SigningKeyConfigError::MissingActiveKey);
        }
        None => {
            eprintln!(
                "amicu-server: WARNING - AMICU_SIGNING_KEY_PEM/_FILE not set; generating an \
                 ephemeral signing key for THIS BOOT ONLY (development). Tokens will not verify \
                 across replicas or survive a restart. Set AMICU_SIGNING_KEY_PEM in production \
                 (AMICU_ENV=production requires it)."
            );
            SigningKey::generate().map_err(|source| SigningKeyConfigError::InvalidKey {
                var: "AMICU_SIGNING_KEY_PEM",
                source,
            })?
        }
    };

    let retiring = match retiring_raw {
        Some(raw) => {
            let normalized = normalize_pem(&raw, "AMICU_SIGNING_KEYS_RETIRING_PEM")?;
            let text =
                normalized
                    .expose_secret_str()
                    .map_err(|_| SigningKeyConfigError::InvalidUtf8 {
                        var: "AMICU_SIGNING_KEYS_RETIRING_PEM",
                    })?;
            let blocks = split_pem_blocks(text);
            if blocks.is_empty() {
                if text.trim().is_empty() {
                    // Explicitly empty: a legitimate "no retiring keys".
                    Vec::new()
                } else {
                    // Something was set, but it contains no PEM marker at
                    // all - almost certainly a misconfiguration. Fail loud
                    // rather than silently loading zero retiring keys: the
                    // whole point of this variable during a rotation is to
                    // keep an outgoing key verifying.
                    return Err(SigningKeyConfigError::NoPemBlocks {
                        var: "AMICU_SIGNING_KEYS_RETIRING_PEM",
                    });
                }
            } else {
                blocks
                    .into_iter()
                    .map(|block| {
                        // `block` (a `Zeroizing<String>`) is dropped - and
                        // zeroized - at the end of this closure; only the
                        // fresh `Vec<u8>` copy `SecretValue` takes ownership
                        // of outlives it.
                        let secret = SecretValue::new(block.as_bytes().to_vec());
                        SigningKey::from_pkcs8_pem(&secret).map_err(|source| {
                            SigningKeyConfigError::InvalidKey {
                                var: "AMICU_SIGNING_KEYS_RETIRING_PEM",
                                source,
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
        }
        None => Vec::new(),
    };

    let mut seen_kids = HashSet::with_capacity(1 + retiring.len());
    seen_kids.insert(active.kid().to_string());
    for key in &retiring {
        if !seen_kids.insert(key.kid().to_string()) {
            return Err(SigningKeyConfigError::DuplicateKid(key.kid().to_string()));
        }
    }

    eprintln!(
        "amicu-server: signing keyring loaded (active kid={}, {} retiring key(s): [{}]).",
        active.kid(),
        retiring.len(),
        retiring
            .iter()
            .map(SigningKey::kid)
            .collect::<Vec<_>>()
            .join(", "),
    );

    Ok(Keyring::with_retiring(active, retiring))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MinTier;

    // Disposable, test-only Ed25519 PKCS#8 keys, freshly generated for this
    // test suite via `openssl genpkey -algorithm ed25519`. Not used for
    // anything real; safe to commit.
    const FIXTURE_PEM_1: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIAGwK8jzAuW3roJK6/gne5fnNmdODLMeRlQnkzMBP/cU\n-----END PRIVATE KEY-----\n";
    const FIXTURE_PEM_2: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIBinCJ18h+LO/AdMnriwJVdk7ionOYmeAsHnxB6+qNZM\n-----END PRIVATE KEY-----\n";
    const FIXTURE_PEM_3: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIPGKWILwOzKA0PWjJbtorLOwW780l4LJXmnN7r0PuDGp\n-----END PRIVATE KEY-----\n";

    fn base_config() -> Config {
        Config {
            bind: Some("127.0.0.1:0".parse().unwrap()),
            issuer: "http://localhost:8080".to_string(),
            audience: "amicu".to_string(),
            base_url: None,
            trusted_proxies: String::new(),
            min_tier: MinTier::Community,
            rate_limit_max: 30,
            rate_limit_window_secs: 60,
            rate_limit_timeout_ms: 1_000,
            email_rate_limit_max: 5,
            signin_email_rate_limit_max: 100,
            email_rate_limit_window_secs: 900,
            max_workspaces_per_user: amicu_store::DEFAULT_MAX_WORKSPACES_PER_USER,
            environment: Environment::Development,
            db_max_connections: 5,
            db_min_connections: 0,
            db_acquire_timeout_secs: 10,
            readyz_timeout_ms: 2_000,
            signing_key_pem: None,
            signing_key_pem_file: None,
            signing_keys_retiring_pem: None,
            signing_keys_retiring_pem_file: None,
            internal_audience: None,
            internal_caller_emails: String::new(),
            outbox_drain_batch_size: amicu_api::DEFAULT_DRAIN_BATCH_SIZE,
            outbox_drain_budget_ms: amicu_api::DEFAULT_DRAIN_BUDGET_MS,
            outbox_inline_send: false,
            outbox_inline_budget_ms: amicu_api::DEFAULT_INLINE_BUDGET_MS,
            signups_enabled: true,
        }
    }

    #[test]
    fn normalize_pem_converts_literal_backslash_n_escape() {
        let all_one_line = FIXTURE_PEM_1.replace('\n', "\\n");
        assert!(!all_one_line.contains('\n'), "fixture setup sanity check");

        let normalized = normalize_pem(&SecretValue::new(all_one_line), "AMICU_SIGNING_KEY_PEM")
            .expect("normalize");
        let key = SigningKey::from_pkcs8_pem(&normalized).expect("parses after normalization");

        let direct = SigningKey::from_pkcs8_pem(&SecretValue::new(FIXTURE_PEM_1.to_string()))
            .expect("parses the raw fixture directly");
        assert_eq!(key.kid(), direct.kid());
    }

    #[test]
    fn normalize_pem_leaves_real_newlines_unaffected() {
        let normalized = normalize_pem(
            &SecretValue::new(FIXTURE_PEM_1.to_string()),
            "AMICU_SIGNING_KEY_PEM",
        )
        .expect("normalize");
        let key = SigningKey::from_pkcs8_pem(&normalized).expect("parses");
        let direct = SigningKey::from_pkcs8_pem(&SecretValue::new(FIXTURE_PEM_1.to_string()))
            .expect("parses the raw fixture directly");
        assert_eq!(key.kid(), direct.kid());
    }

    #[test]
    fn split_pem_blocks_splits_two_concatenated_keys() {
        let blob = format!("{FIXTURE_PEM_2}\n{FIXTURE_PEM_3}");
        let blocks = split_pem_blocks(&blob);
        assert_eq!(blocks.len(), 2);
        for block in &blocks {
            assert!(block.starts_with("-----BEGIN"), "{}", block.as_str());
        }
        let k1 = SigningKey::from_pkcs8_pem(&SecretValue::new(blocks[0].as_bytes().to_vec()))
            .expect("parse block 1");
        let k2 = SigningKey::from_pkcs8_pem(&SecretValue::new(blocks[1].as_bytes().to_vec()))
            .expect("parse block 2");
        assert_ne!(k1.kid(), k2.kid());
    }

    #[test]
    fn duplicate_kid_active_and_retiring_is_rejected() {
        let mut config = base_config();
        config.signing_key_pem = Some(RedactedEnvString::new(FIXTURE_PEM_1));
        config.signing_keys_retiring_pem = Some(RedactedEnvString::new(FIXTURE_PEM_1));

        let err = load_keyring(&config).expect_err("duplicate kid must be rejected");
        assert!(
            matches!(err, SigningKeyConfigError::DuplicateKid(_)),
            "{err}"
        );
    }

    #[test]
    fn duplicate_kid_among_retiring_is_rejected() {
        let mut config = base_config();
        config.signing_key_pem = Some(RedactedEnvString::new(FIXTURE_PEM_2));
        config.signing_keys_retiring_pem = Some(RedactedEnvString::new(format!(
            "{FIXTURE_PEM_3}\n{FIXTURE_PEM_3}"
        )));

        let err = load_keyring(&config).expect_err("duplicate kid among retiring must be rejected");
        assert!(
            matches!(err, SigningKeyConfigError::DuplicateKid(_)),
            "{err}"
        );
    }

    #[test]
    fn same_pem_yields_same_kid() {
        let a = SigningKey::from_pkcs8_pem(&SecretValue::new(FIXTURE_PEM_1.to_string())).unwrap();
        let b = SigningKey::from_pkcs8_pem(&SecretValue::new(FIXTURE_PEM_1.to_string())).unwrap();
        assert_eq!(a.kid(), b.kid());
    }

    #[test]
    fn different_pem_yields_different_kid() {
        let a = SigningKey::from_pkcs8_pem(&SecretValue::new(FIXTURE_PEM_1.to_string())).unwrap();
        let b = SigningKey::from_pkcs8_pem(&SecretValue::new(FIXTURE_PEM_2.to_string())).unwrap();
        assert_ne!(a.kid(), b.kid());
    }

    #[test]
    fn production_missing_active_key_is_hard_error() {
        let mut config = base_config();
        config.environment = Environment::Production;

        let err = load_keyring(&config).expect_err("production requires an active key");
        assert!(
            matches!(err, SigningKeyConfigError::MissingActiveKey),
            "{err}"
        );
    }

    #[test]
    fn development_missing_active_key_falls_back_to_generated() {
        let config = base_config();
        let keyring = load_keyring(&config).expect("development falls back to a generated key");
        assert_ne!(keyring.active().kid(), "");
    }

    #[test]
    fn conflicting_value_and_file_for_active_key_is_error() {
        let mut config = base_config();
        config.signing_key_pem = Some(RedactedEnvString::new(FIXTURE_PEM_1));
        config.signing_key_pem_file = Some("/does/not/matter.pem".to_string());

        let err = load_keyring(&config).expect_err("conflicting sources must be rejected");
        assert!(
            matches!(err, SigningKeyConfigError::ConflictingSource { .. }),
            "{err}"
        );
    }

    #[test]
    fn file_variant_loads_key_from_disk_matching_value_variant() {
        let path =
            std::env::temp_dir().join(format!("amicu-test-signing-key-{}.pem", std::process::id()));
        std::fs::write(&path, FIXTURE_PEM_1).expect("write temp fixture file");

        let mut config = base_config();
        config.signing_key_pem_file = Some(path.to_string_lossy().to_string());

        let result = load_keyring(&config);
        let _ = std::fs::remove_file(&path);

        let keyring = result.expect("file-backed key must load");

        let mut value_config = base_config();
        value_config.signing_key_pem = Some(RedactedEnvString::new(FIXTURE_PEM_1));
        let value_keyring = load_keyring(&value_config).expect("value-backed key must load");

        assert_eq!(keyring.active().kid(), value_keyring.active().kid());
    }

    #[test]
    fn redacted_env_string_debug_never_leaks_the_value() {
        let wrapped = RedactedEnvString::new("SECRET-MARKER-XYZ");
        let rendered = format!("{wrapped:?}");
        assert!(!rendered.contains("SECRET-MARKER-XYZ"), "{rendered}");
    }

    /// HIGH-1 fix: setting the `_FILE` var to inline PEM text (instead of a
    /// path) is rejected WITHOUT ever echoing the value anywhere in the
    /// error - neither `Display` nor `Debug`.
    #[test]
    fn file_var_set_to_inline_pem_is_rejected_without_echoing_it() {
        let mut config = base_config();
        config.signing_key_pem_file = Some(FIXTURE_PEM_1.to_string());

        let err = load_keyring(&config).expect_err("inline PEM in a _FILE var must be rejected");
        assert!(
            matches!(err, SigningKeyConfigError::FileValueLooksLikeKey { .. }),
            "{err}"
        );

        let display = format!("{err}");
        let debug = format!("{err:?}");
        // The fixture's base64 body must not appear anywhere in the rendered
        // error, in either form.
        let body_fragment = "MC4CAQAwBQYDK2VwBCIEIAGwK8jzAuW3roJK6";
        assert!(!display.contains(body_fragment), "{display}");
        assert!(!display.contains("PRIVATE KEY"), "{display}");
        assert!(!debug.contains(body_fragment), "{debug}");
        assert!(!debug.contains("PRIVATE KEY"), "{debug}");
    }

    /// Same guard, on the retiring `_FILE` var.
    #[test]
    fn retiring_file_var_set_to_inline_pem_is_rejected() {
        let mut config = base_config();
        config.signing_key_pem = Some(RedactedEnvString::new(FIXTURE_PEM_1));
        config.signing_keys_retiring_pem_file = Some(FIXTURE_PEM_2.to_string());

        let err = load_keyring(&config).expect_err("inline PEM in a _FILE var must be rejected");
        assert!(
            matches!(err, SigningKeyConfigError::FileValueLooksLikeKey { .. }),
            "{err}"
        );
    }

    /// MEDIUM-1 fix: a retiring value that's set but contains no PEM marker
    /// at all is a hard error, not a silent "zero retiring keys".
    #[test]
    fn retiring_garbage_value_is_hard_error() {
        let mut config = base_config();
        config.signing_key_pem = Some(RedactedEnvString::new(FIXTURE_PEM_1));
        config.signing_keys_retiring_pem = Some(RedactedEnvString::new("totally-not-a-pem"));

        let err = load_keyring(&config).expect_err("garbage retiring value must be rejected");
        assert!(
            matches!(err, SigningKeyConfigError::NoPemBlocks { .. }),
            "{err}"
        );
    }

    /// An explicitly-EMPTY retiring value must keep working as "no retiring
    /// keys" - the `NoPemBlocks` guard must not regress this case.
    #[test]
    fn retiring_explicitly_empty_value_still_loads_with_zero_retiring_keys() {
        let mut config = base_config();
        config.signing_key_pem = Some(RedactedEnvString::new(FIXTURE_PEM_1));
        config.signing_keys_retiring_pem = Some(RedactedEnvString::new(""));

        let keyring = load_keyring(&config).expect("explicitly-empty retiring value is valid");
        assert_eq!(keyring.jwks().keys.len(), 1, "only the active key");
    }

    /// LOW-2: the `#[derive(Debug)]` on `Config` cannot leak signing-key
    /// material through `RedactedEnvString`, end to end, at the `Config`
    /// level (not just the wrapper type in isolation).
    #[test]
    fn config_debug_never_leaks_signing_key_material() {
        let mut config = base_config();
        config.signing_key_pem = Some(RedactedEnvString::new(
            "-----BEGIN PRIVATE KEY-----\nSENTINEL-MARKER-VALUE\n-----END PRIVATE KEY-----\n",
        ));
        config.signing_keys_retiring_pem = Some(RedactedEnvString::new(
            "-----BEGIN PRIVATE KEY-----\nOTHER-SENTINEL-VALUE\n-----END PRIVATE KEY-----\n",
        ));

        let rendered = format!("{config:?}");
        assert!(!rendered.contains("SENTINEL-MARKER-VALUE"), "{rendered}");
        assert!(!rendered.contains("OTHER-SENTINEL-VALUE"), "{rendered}");
    }

    /// HIGH-1 fix: `clap` renders `[env: VAR=<current value>]` in `--help`
    /// unless `hide_env_values` is set on the arg - which would print the
    /// live private key straight to stdout for anyone running
    /// `amicu-server --help` with a secret already exported in the shell.
    /// Asserts on the built `clap::Command` (the workspace forbids `unsafe`,
    /// so this cannot use `std::env::set_var` + a real `--help` capture).
    #[test]
    fn help_output_never_renders_signing_key_env_values() {
        use clap::CommandFactory;

        let cmd = Config::command();
        for name in [
            "signing_key_pem",
            "signing_key_pem_file",
            "signing_keys_retiring_pem",
            "signing_keys_retiring_pem_file",
        ] {
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_id() == name)
                .expect(name);
            assert!(
                arg.is_hide_env_values_set(),
                "{name} must hide its env value in --help"
            );
        }
    }

    /// Companion to the above: the two VALUE args (not the `_FILE` args) are
    /// also fully hidden from `--help` output, so passing key material via
    /// argv is never advertised as a supported usage pattern - the flags
    /// still work (clap needs them to bind `env`), they're just not listed.
    #[test]
    fn signing_key_value_args_are_hidden_from_help_listing() {
        use clap::CommandFactory;

        let cmd = Config::command();
        for name in ["signing_key_pem", "signing_keys_retiring_pem"] {
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_id() == name)
                .expect(name);
            assert!(arg.is_hide_set(), "{name} must be hidden from --help");
        }
    }

    /// A malformed-but-PEM-shaped value (has a `-----BEGIN`/`-----END` armor
    /// but garbage body, so it is NOT caught by the `looks_like_inline_key`/
    /// `NoPemBlocks` guards) hits `SigningKeyConfigError::InvalidKey`, whose
    /// `Display`/`Debug` must not contain any fragment of the offending
    /// value - only the env var name and `AuthError`'s own fixed, redacted
    /// message ("key generation failure").
    #[test]
    fn malformed_pem_error_never_echoes_the_offending_value() {
        const MALFORMED: &str = "-----BEGIN PRIVATE KEY-----\nSENTINEL-NOT-VALID-BASE64!!!\n-----END PRIVATE KEY-----\n";
        let mut config = base_config();
        config.signing_key_pem = Some(RedactedEnvString::new(MALFORMED));

        let err = load_keyring(&config).expect_err("malformed PEM must be rejected");
        assert!(
            matches!(err, SigningKeyConfigError::InvalidKey { .. }),
            "{err}"
        );

        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(!display.contains("SENTINEL-NOT-VALID-BASE64"), "{display}");
        assert!(!debug.contains("SENTINEL-NOT-VALID-BASE64"), "{debug}");
        // Only the env var name and AuthError's fixed message should appear.
        assert!(display.contains("AMICU_SIGNING_KEY_PEM"), "{display}");
    }
}
