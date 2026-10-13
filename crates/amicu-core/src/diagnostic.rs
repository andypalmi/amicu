//! Standardized diagnostic error-code catalog and report contract.
//!
//! Diagnostic codes follow the shape `AMICU-<CATEGORY>-<NUMBER>` across eight
//! standardized categories. The JSON produced by [`DiagnosticReport`] is the
//! stable contract consumed by `--output json` on every automation command.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The eight standardized diagnostic categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// HCL parsing / syntax.
    Parse,
    /// Structural / schema validation.
    Schema,
    /// References and dependencies.
    Ref,
    /// Runtime / artifact / hardware capability.
    Cap,
    /// Memory / VRAM and capacity fit.
    Mem,
    /// Governance, budget, residency.
    Policy,
    /// State, locking, drift.
    State,
    /// Provider provisioning / availability.
    Provider,
}

impl Category {
    /// The uppercase code prefix for this category (without a trailing dash).
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Category::Parse => "AMICU-PARSE",
            Category::Schema => "AMICU-SCHEMA",
            Category::Ref => "AMICU-REF",
            Category::Cap => "AMICU-CAP",
            Category::Mem => "AMICU-MEM",
            Category::Policy => "AMICU-POLICY",
            Category::State => "AMICU-STATE",
            Category::Provider => "AMICU-PROVIDER",
        }
    }

    /// The bare category token as it appears between the two dashes.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Category::Parse => "PARSE",
            Category::Schema => "SCHEMA",
            Category::Ref => "REF",
            Category::Cap => "CAP",
            Category::Mem => "MEM",
            Category::Policy => "POLICY",
            Category::State => "STATE",
            Category::Provider => "PROVIDER",
        }
    }

    /// All categories, in declaration order.
    #[must_use]
    pub const fn all() -> [Category; 8] {
        [
            Category::Parse,
            Category::Schema,
            Category::Ref,
            Category::Cap,
            Category::Mem,
            Category::Policy,
            Category::State,
            Category::Provider,
        ]
    }

    fn from_token(token: &str) -> Option<Category> {
        Category::all().into_iter().find(|c| c.token() == token)
    }
}

/// Error returned when a diagnostic code string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCodeError(String);

impl fmt::Display for ParseCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid diagnostic code: {}", self.0)
    }
}

impl std::error::Error for ParseCodeError {}

/// A diagnostic code carrying its category and a numeric identifier.
///
/// Serializes and displays as `AMICU-<CATEGORY>-<NNN>`, zero-padded to three
/// digits (e.g. `AMICU-CAP-001`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiagnosticCode {
    category: Category,
    number: u16,
}

impl DiagnosticCode {
    /// Construct a code from a category and number.
    #[must_use]
    pub const fn new(category: Category, number: u16) -> Self {
        Self { category, number }
    }

    /// The category of this code.
    #[must_use]
    pub const fn category(self) -> Category {
        self.category
    }

    /// The numeric identifier of this code.
    #[must_use]
    pub const fn number(self) -> u16 {
        self.number
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{:03}", self.category.prefix(), self.number)
    }
}

impl FromStr for DiagnosticCode {
    type Err = ParseCodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseCodeError(s.to_owned());
        let rest = s.strip_prefix("AMICU-").ok_or_else(err)?;
        let (token, number) = rest.rsplit_once('-').ok_or_else(err)?;
        let category = Category::from_token(token).ok_or_else(err)?;
        // Reject non-canonical numeric forms (empty, signs, overflow) but
        // accept any zero-padding.
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(err());
        }
        let number: u16 = number.parse().map_err(|_| err())?;
        Ok(DiagnosticCode::new(category, number))
    }
}

impl Serialize for DiagnosticCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DiagnosticCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A blocking problem: the operation cannot proceed.
    Error,
    /// A non-blocking concern the user should review.
    Warning,
    /// Informational context.
    Info,
}

/// The location in a source file a diagnostic refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct SourceLocation {
    /// Source file path.
    pub file: String,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number.
    pub column: u32,
}

/// A single JSON-patch-like edit that would resolve a diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct Patch {
    /// The patch operation (e.g. `replace`, `add`, `remove`).
    pub op: String,
    /// A JSON Pointer to the target location.
    pub path: String,
    /// The value to apply.
    #[cfg_attr(feature = "utoipa", schema(value_type = Object))]
    pub value: serde_json::Value,
}

/// A suggested fix attached to a diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct SuggestedFix {
    /// Human-readable description of the fix.
    pub description: String,
    /// The proposed patch.
    pub patch: Patch,
}

/// A single diagnostic in a [`DiagnosticReport`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct Diagnostic {
    /// The standardized diagnostic code.
    ///
    /// Serialized as the `AMICU-<CATEGORY>-<NNN>` string form.
    #[cfg_attr(feature = "utoipa", schema(value_type = String, example = "AMICU-CAP-001"))]
    pub code: DiagnosticCode,
    /// Severity of this diagnostic.
    pub severity: Severity,
    /// The resource address this diagnostic applies to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// The attribute path within the resource, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Human-readable message.
    pub message: String,
    /// Source location, if the diagnostic maps to a specific location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceLocation>,
    /// Suggested fixes; omitted from output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_fixes: Vec<SuggestedFix>,
}

impl Diagnostic {
    /// Construct a minimal diagnostic with a code, severity and message.
    #[must_use]
    pub fn new(code: DiagnosticCode, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code,
            severity,
            resource: None,
            path: None,
            message: message.into(),
            source: None,
            suggested_fixes: Vec::new(),
        }
    }
}

/// The top-level diagnostic report emitted by `--output json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DiagnosticReport {
    /// Contract version. Currently always `"v1"`.
    pub version: String,
    /// Whether the analyzed input is valid (no error-severity diagnostics).
    pub valid: bool,
    /// The diagnostics, in report order.
    pub diagnostics: Vec<Diagnostic>,
}

impl DiagnosticReport {
    /// The current contract version string.
    pub const VERSION: &'static str = "v1";

    /// Build a report from a set of diagnostics, deriving `valid` from the
    /// absence of any [`Severity::Error`] entry.
    #[must_use]
    pub fn new(diagnostics: Vec<Diagnostic>) -> Self {
        let valid = !diagnostics.iter().any(|d| d.severity == Severity::Error);
        Self {
            version: Self::VERSION.to_owned(),
            valid,
            diagnostics,
        }
    }
}

/// Seeded catalog of standardized diagnostic codes.
///
/// This is a representative, reviewable catalog with at least one code per
/// category; it is intentionally not exhaustive.
pub mod catalog {
    use super::{Category, DiagnosticCode};

    /// `AMICU-PARSE-001` - HCL block is not closed.
    pub const PARSE_UNCLOSED_BLOCK: DiagnosticCode = DiagnosticCode::new(Category::Parse, 1);
    // --- amicu-planning#35: generated HCL customize/plan display ---
    /// `AMICU-PARSE-002` - HCL text is not syntactically valid; see the
    /// diagnostic message for detail. A generic parse-failure code, distinct
    /// from the more specific [`PARSE_UNCLOSED_BLOCK`], used whenever an
    /// `amicu_hcl::HclError` (any variant) needs to be surfaced as a
    /// [`super::Diagnostic`] inside a [`super::DiagnosticReport`] rather than
    /// as a raw HTTP error - e.g. `POST /recommend/customize`, whose
    /// `DiagnosticsPanel`-first UI never wants a separate error-string code
    /// path for a malformed submission.
    pub const PARSE_INVALID_SYNTAX: DiagnosticCode = DiagnosticCode::new(Category::Parse, 2);
    /// `AMICU-SCHEMA-001` - required attribute is missing.
    pub const SCHEMA_MISSING_REQUIRED: DiagnosticCode = DiagnosticCode::new(Category::Schema, 1);
    /// `AMICU-SCHEMA-002` - the same resource id is declared in more than one
    /// config file. Raised by `amicu-engine` checks.
    pub const SCHEMA_DUPLICATE_ID: DiagnosticCode = DiagnosticCode::new(Category::Schema, 2);
    /// `AMICU-SCHEMA-003` - an attribute or nested block that the schema does
    /// not define appears inside a resource (or its nested block), or at the
    /// top level of the file (amicu-planning#157). Error, not a warning: a
    /// typo such as `contex_length` would otherwise be silently dropped and
    /// the field would quietly fall back to its default. Raised by the HCL
    /// front end (`amicu-hcl::HclError::UnknownField`).
    pub const SCHEMA_UNKNOWN_ATTRIBUTE: DiagnosticCode = DiagnosticCode::new(Category::Schema, 3);
    /// `AMICU-REF-001` - reference points at an unknown resource.
    pub const REF_UNKNOWN_RESOURCE: DiagnosticCode = DiagnosticCode::new(Category::Ref, 1);
    /// `AMICU-CAP-001` - runtime requires a GGUF model artifact.
    pub const CAP_RUNTIME_ARTIFACT_MISMATCH: DiagnosticCode = DiagnosticCode::new(Category::Cap, 1);
    // --- amicu-planning#150: runtime escape hatch (extra_args / env) ---
    /// `AMICU-CAP-002` - an `extra_args` flag duplicates a flag already
    /// emitted by a typed field (e.g. `--ctx-size`, which `context_length`
    /// already renders). Error, not a warning: the raw flag would either
    /// silently double-pass the flag to `llama-server` (last one wins, which
    /// one is order-dependent and easy to get wrong) or be rejected by the
    /// binary itself, so the ambiguity is rejected before launch instead.
    pub const CAP_EXTRA_ARG_DUPLICATES_TYPED: DiagnosticCode =
        DiagnosticCode::new(Category::Cap, 2);
    /// `AMICU-CAP-003` - an `extra_args` flag is on the fixed denylist
    /// (`--host`, `--api-key`, `--api-key-file`, `--ssl-key-file`). Error:
    /// `--host` conflicts with amicu owning the bind address (see
    /// `ServiceSupervisor::start`'s endpoint derivation), and the others put
    /// secret material - or a path to it - directly into argv, which is
    /// visible to any process that can read `/proc/<pid>/cmdline` or a
    /// process listing, defeating the point of secret-designated `env`.
    pub const CAP_EXTRA_ARG_DENYLISTED: DiagnosticCode = DiagnosticCode::new(Category::Cap, 3);
    /// `AMICU-CAP-004` - `extra_args` is non-empty. Info, not a blocker: the
    /// estimator cannot interpret raw flags (e.g. a KV-cache-dtype or
    /// speculative-decoding flag hidden in `extra_args`), so the memory/fit
    /// verdict for this deployment is unverified. Mirrored in the plan's
    /// `CostPerformance.assumptions` (`amicu_engine::planner`).
    pub const CAP_EXTRA_ARGS_PRESENT: DiagnosticCode = DiagnosticCode::new(Category::Cap, 4);
    // --- amicu-planning#151: typed chat template fields ---
    /// `AMICU-CAP-005` - a `llama_cpp` settings block sets both
    /// `chat_template` and `chat_template_file`. Error: they render mutually
    /// exclusive flags (`--chat-template` vs `--chat-template-file`), so
    /// having both set is ambiguous - `render_command` emits neither rather
    /// than guessing which one should win, and `plan_launch` refuses to
    /// build a launch at all.
    pub const CAP_CHAT_TEMPLATE_CONFLICT: DiagnosticCode = DiagnosticCode::new(Category::Cap, 5);
    // --- amicu-planning#152: vLLM command renderer ---
    /// `AMICU-CAP-006` - a deployment's model artifact is GGUF but the
    /// runtime is vLLM. Warning, not an error: vLLM can load a single-file
    /// GGUF checkpoint, but the support is experimental (upstream vLLM docs),
    /// unlike llama.cpp where GGUF is the native, fully-supported format.
    /// Numbered 006, not 005: `AMICU-CAP-005` is
    /// `CAP_CHAT_TEMPLATE_CONFLICT`, registered by the concurrently-landing
    /// amicu-planning#151 typed-fields PR (feat/typed-runtime-b, #146).
    pub const CAP_VLLM_GGUF_EXPERIMENTAL: DiagnosticCode = DiagnosticCode::new(Category::Cap, 6);
    // --- amicu-planning#151 PR C: typed speculative-decoding block ---
    /// `AMICU-CAP-007` - a `llama_cpp` deployment's `speculative { model = ... }`
    /// draft reference names the deployment's own primary model. Error:
    /// speculative decoding is only worth anything with a *smaller, cheaper*
    /// draft model, so drafting with the target model itself is never intended -
    /// `llama-server` would load the same weights a second time (doubling
    /// memory for no speedup) and the fit check would count them twice.
    pub const CAP_SPECULATIVE_DRAFT_IS_PRIMARY: DiagnosticCode =
        DiagnosticCode::new(Category::Cap, 7);
    // --- amicu-planning#180: provider-agnostic `scaling {}` block ---
    /// `AMICU-CAP-008` - a deployment's `scaling {}` block has invalid
    /// values (`max_replicas`/`target_concurrency` below 1, a zero
    /// `scaledown_window`, or `min_replicas` above `max_replicas`). Error.
    pub const CAP_SCALING_INVALID: DiagnosticCode = DiagnosticCode::new(Category::Cap, 8);
    /// `AMICU-CAP-009` - a deployment sets `scaling {}` but its hardware
    /// profile's provider has no scaling mapping yet (only `modal` does;
    /// amicu-planning#181 tracks the rest). Error rather than a silent no-op:
    /// the block's point is to cap spend, so ignoring it would be a lie.
    pub const CAP_SCALING_UNSUPPORTED_PROVIDER: DiagnosticCode =
        DiagnosticCode::new(Category::Cap, 9);
    // --- amicu-planning#191: never silently drop the KV cache ---
    /// `AMICU-CAP-010` - a vLLM deployment's model does not declare
    /// `n_layers`/`n_kv_heads`/`head_dim`, so the KV cache is left out of the
    /// memory estimate and the fit check covers weights only. Warning, not an
    /// error: the deployment may well fit, but at long `context_length` the KV
    /// cache can dominate, so a pass here is not evidence of fit.
    pub const CAP_KV_CACHE_NOT_ESTIMATED: DiagnosticCode = DiagnosticCode::new(Category::Cap, 10);
    /// `AMICU-MEM-001` - model does not fit in available VRAM.
    pub const MEM_VRAM_EXCEEDED: DiagnosticCode = DiagnosticCode::new(Category::Mem, 1);
    /// `AMICU-MEM-002` - a single artifact/weight file exceeds the available
    /// size budget, independent of aggregate VRAM.
    pub const MEM_ARTIFACT_OVERSIZE: DiagnosticCode = DiagnosticCode::new(Category::Mem, 2);
    /// `AMICU-POLICY-001` - deployment exceeds the configured budget.
    pub const POLICY_BUDGET_EXCEEDED: DiagnosticCode = DiagnosticCode::new(Category::Policy, 1);
    // --- amicu-planning#24: region / data-residency validation ---
    /// `AMICU-POLICY-002` - a deployment's region violates the configured
    /// residency / region allowlist (or matches a deny rule's region).
    pub const POLICY_REGION_RESIDENCY: DiagnosticCode = DiagnosticCode::new(Category::Policy, 2);
    /// `AMICU-POLICY-003` - a deployment's provider is not allowed (or matches
    /// a deny rule's provider).
    pub const POLICY_PROVIDER_NOT_ALLOWED: DiagnosticCode =
        DiagnosticCode::new(Category::Policy, 3);
    // --- amicu-planning#119: HCL secret safety ---
    /// `AMICU-POLICY-004` - a secret-designated field (`api_key`, `token`,
    /// `password`, or an `env` map value) was given as a literal instead of a
    /// `secret(...)` reference. Structural, zero-false-positive: this is a
    /// hard parse-time rejection, surfaced through [`crate::graph::Graph`]'s
    /// HCL front end (`amicu-hcl::HclError::SecretLiteral`) rather than as a
    /// `Diagnostic`, since it blocks parsing itself.
    pub const POLICY_SECRET_LITERAL_REJECTED: DiagnosticCode =
        DiagnosticCode::new(Category::Policy, 4);
    /// `AMICU-POLICY-005` - a genuinely free-text field (`description`,
    /// `display_name`) or a comment contains a high-confidence secret shape
    /// (AWS access key, OpenAI-style key, PEM header, JWT, or a high-entropy
    /// token). Warning only, never blocks; suppressible with a trailing
    /// `# amicu:allow` comment on the same line.
    pub const POLICY_SECRET_PATTERN_SUSPECTED: DiagnosticCode =
        DiagnosticCode::new(Category::Policy, 5);
    // --- amicu-planning#150: runtime escape hatch (extra_args / env) ---
    /// `AMICU-POLICY-006` - a runtime settings block's `env` map (the
    /// llama.cpp / vLLM `extra_args`/`env` escape hatch, amicu-planning#150)
    /// has a value that matches a high-confidence secret shape (AWS access
    /// key, OpenAI-style key, PEM header, JWT, or a high-entropy token) -
    /// the same detectors [`POLICY_SECRET_PATTERN_SUSPECTED`] uses. Warning
    /// only, never blocks: unlike `credential.env` (`AMICU-POLICY-004`), a
    /// runtime `env` value is not schema-directed to be secret-designated,
    /// so this is a heuristic nudge toward moving the value into a
    /// `credential`, not a structural rejection.
    pub const POLICY_RUNTIME_ENV_SECRET_SUSPECTED: DiagnosticCode =
        DiagnosticCode::new(Category::Policy, 6);
    /// `AMICU-STATE-001` - state is locked by another operation.
    pub const STATE_LOCKED: DiagnosticCode = DiagnosticCode::new(Category::State, 1);
    /// `AMICU-PROVIDER-001` - requested capacity is unavailable in region.
    pub const PROVIDER_CAPACITY_UNAVAILABLE: DiagnosticCode =
        DiagnosticCode::new(Category::Provider, 1);
    /// `AMICU-PROVIDER-002` - a provider/model artifact download failed
    /// transiently; retrying resumes the transfer.
    pub const PROVIDER_DOWNLOAD_FAILED: DiagnosticCode = DiagnosticCode::new(Category::Provider, 2);
    /// `AMICU-PROVIDER-003` - provider capability/pricing data is stale or could
    /// not be refreshed; the last snapshot is surfaced, never fabricated.
    pub const PROVIDER_METADATA_STALE: DiagnosticCode = DiagnosticCode::new(Category::Provider, 3);
    /// `AMICU-PROVIDER-010` - the request names an offering the provider does
    /// not offer.
    pub const PROVIDER_UNKNOWN_OFFERING: DiagnosticCode =
        DiagnosticCode::new(Category::Provider, 10);
    /// `AMICU-PROVIDER-011` - the request names a region the provider does not
    /// serve.
    pub const PROVIDER_UNKNOWN_REGION: DiagnosticCode = DiagnosticCode::new(Category::Provider, 11);
    /// `AMICU-PROVIDER-012` - the handle refers to no known provisioned
    /// instance.
    pub const PROVIDER_UNKNOWN_INSTANCE: DiagnosticCode =
        DiagnosticCode::new(Category::Provider, 12);
    /// `AMICU-PROVIDER-013` - the provider backend failed transiently.
    pub const PROVIDER_BACKEND: DiagnosticCode = DiagnosticCode::new(Category::Provider, 13);
    /// `AMICU-PROVIDER-014` - the resolved credential was rejected by the
    /// provider.
    pub const PROVIDER_CREDENTIALS_REJECTED: DiagnosticCode =
        DiagnosticCode::new(Category::Provider, 14);
    /// `AMICU-PROVIDER-015` - the operation is permanently unsupported by this
    /// adapter (e.g. a catalog-only adapter with no apply-time backend yet).
    /// Never retryable.
    pub const PROVIDER_UNSUPPORTED_OPERATION: DiagnosticCode =
        DiagnosticCode::new(Category::Provider, 15);

    /// Every catalog entry, for discovery and coverage tests.
    #[must_use]
    pub const fn all() -> [DiagnosticCode; 34] {
        [
            PARSE_UNCLOSED_BLOCK,
            PARSE_INVALID_SYNTAX,
            SCHEMA_MISSING_REQUIRED,
            SCHEMA_DUPLICATE_ID,
            // amicu-planning#157 addition:
            SCHEMA_UNKNOWN_ATTRIBUTE,
            REF_UNKNOWN_RESOURCE,
            CAP_RUNTIME_ARTIFACT_MISMATCH,
            // amicu-planning#150 additions:
            CAP_EXTRA_ARG_DUPLICATES_TYPED,
            CAP_EXTRA_ARG_DENYLISTED,
            CAP_EXTRA_ARGS_PRESENT,
            // amicu-planning#151 additions:
            CAP_CHAT_TEMPLATE_CONFLICT,
            // amicu-planning#152 addition:
            CAP_VLLM_GGUF_EXPERIMENTAL,
            // amicu-planning#151 PR C addition:
            CAP_SPECULATIVE_DRAFT_IS_PRIMARY,
            // amicu-planning#180 additions:
            CAP_SCALING_INVALID,
            CAP_SCALING_UNSUPPORTED_PROVIDER,
            // amicu-planning#191 addition:
            CAP_KV_CACHE_NOT_ESTIMATED,
            MEM_VRAM_EXCEEDED,
            MEM_ARTIFACT_OVERSIZE,
            POLICY_BUDGET_EXCEEDED,
            // amicu-planning#24 additions:
            POLICY_REGION_RESIDENCY,
            POLICY_PROVIDER_NOT_ALLOWED,
            // amicu-planning#119 additions:
            POLICY_SECRET_LITERAL_REJECTED,
            POLICY_SECRET_PATTERN_SUSPECTED,
            // amicu-planning#150 addition:
            POLICY_RUNTIME_ENV_SECRET_SUSPECTED,
            STATE_LOCKED,
            PROVIDER_CAPACITY_UNAVAILABLE,
            PROVIDER_DOWNLOAD_FAILED,
            PROVIDER_METADATA_STALE,
            PROVIDER_UNKNOWN_OFFERING,
            PROVIDER_UNKNOWN_REGION,
            PROVIDER_UNKNOWN_INSTANCE,
            PROVIDER_BACKEND,
            PROVIDER_CREDENTIALS_REJECTED,
            // amicu-planning#158 addition:
            PROVIDER_UNSUPPORTED_OPERATION,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn code_display_is_zero_padded_uppercase() {
        assert_eq!(
            catalog::CAP_RUNTIME_ARTIFACT_MISMATCH.to_string(),
            "AMICU-CAP-001"
        );
        assert_eq!(
            DiagnosticCode::new(Category::Provider, 42).to_string(),
            "AMICU-PROVIDER-042"
        );
        assert_eq!(
            DiagnosticCode::new(Category::Mem, 123).to_string(),
            "AMICU-MEM-123"
        );
    }

    #[test]
    fn code_roundtrips_through_string() {
        for code in catalog::all() {
            let s = code.to_string();
            let parsed: DiagnosticCode = s.parse().unwrap();
            assert_eq!(parsed, code);
        }
        // Zero-padding is tolerated on parse.
        assert_eq!(
            "AMICU-CAP-1".parse::<DiagnosticCode>().unwrap(),
            DiagnosticCode::new(Category::Cap, 1)
        );
    }

    #[test]
    fn catalog_codes_are_distinct() {
        let mut codes: Vec<String> = catalog::all().iter().map(ToString::to_string).collect();
        let total = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), total, "catalog contains a duplicate code");
        assert_eq!(
            catalog::POLICY_REGION_RESIDENCY.to_string(),
            "AMICU-POLICY-002"
        );
        assert_eq!(
            catalog::POLICY_PROVIDER_NOT_ALLOWED.to_string(),
            "AMICU-POLICY-003"
        );
        assert_eq!(catalog::MEM_ARTIFACT_OVERSIZE.to_string(), "AMICU-MEM-002");
        assert_eq!(
            catalog::PROVIDER_DOWNLOAD_FAILED.to_string(),
            "AMICU-PROVIDER-002"
        );
        assert_eq!(
            catalog::PROVIDER_METADATA_STALE.to_string(),
            "AMICU-PROVIDER-003"
        );
        assert_eq!(
            catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED.to_string(),
            "AMICU-CAP-002"
        );
        assert_eq!(
            catalog::CAP_EXTRA_ARG_DENYLISTED.to_string(),
            "AMICU-CAP-003"
        );
        assert_eq!(catalog::CAP_EXTRA_ARGS_PRESENT.to_string(), "AMICU-CAP-004");
        assert_eq!(
            catalog::CAP_CHAT_TEMPLATE_CONFLICT.to_string(),
            "AMICU-CAP-005"
        );
        assert_eq!(
            catalog::CAP_VLLM_GGUF_EXPERIMENTAL.to_string(),
            "AMICU-CAP-006"
        );
        assert_eq!(
            catalog::CAP_KV_CACHE_NOT_ESTIMATED.to_string(),
            "AMICU-CAP-010"
        );
        assert_eq!(
            catalog::POLICY_RUNTIME_ENV_SECRET_SUSPECTED.to_string(),
            "AMICU-POLICY-006"
        );
    }

    #[test]
    fn provider_codes_render_expected_strings() {
        assert_eq!(
            catalog::PROVIDER_CAPACITY_UNAVAILABLE.to_string(),
            "AMICU-PROVIDER-001"
        );
        assert_eq!(
            catalog::PROVIDER_DOWNLOAD_FAILED.to_string(),
            "AMICU-PROVIDER-002"
        );
        assert_eq!(
            catalog::PROVIDER_UNKNOWN_OFFERING.to_string(),
            "AMICU-PROVIDER-010"
        );
        assert_eq!(
            catalog::PROVIDER_UNKNOWN_REGION.to_string(),
            "AMICU-PROVIDER-011"
        );
        assert_eq!(
            catalog::PROVIDER_UNKNOWN_INSTANCE.to_string(),
            "AMICU-PROVIDER-012"
        );
        assert_eq!(catalog::PROVIDER_BACKEND.to_string(), "AMICU-PROVIDER-013");
        assert_eq!(
            catalog::PROVIDER_CREDENTIALS_REJECTED.to_string(),
            "AMICU-PROVIDER-014"
        );
        // All provider codes are present in the catalog.
        for code in [
            catalog::PROVIDER_CAPACITY_UNAVAILABLE,
            catalog::PROVIDER_DOWNLOAD_FAILED,
            catalog::PROVIDER_UNKNOWN_OFFERING,
            catalog::PROVIDER_UNKNOWN_REGION,
            catalog::PROVIDER_UNKNOWN_INSTANCE,
            catalog::PROVIDER_BACKEND,
            catalog::PROVIDER_CREDENTIALS_REJECTED,
        ] {
            assert!(catalog::all().contains(&code));
        }
    }

    #[test]
    fn code_parse_rejects_garbage() {
        assert!("CAP-001".parse::<DiagnosticCode>().is_err());
        assert!("AMICU-NOPE-001".parse::<DiagnosticCode>().is_err());
        assert!("AMICU-CAP-".parse::<DiagnosticCode>().is_err());
        assert!("AMICU-CAP-xx".parse::<DiagnosticCode>().is_err());
    }

    #[test]
    fn every_category_has_distinct_prefix() {
        let mut prefixes: Vec<&str> = Category::all().iter().map(|c| c.prefix()).collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        assert_eq!(prefixes.len(), 8);
    }

    #[test]
    fn report_matches_prd_example_shape() {
        let report = DiagnosticReport {
            version: "v1".to_owned(),
            valid: false,
            diagnostics: vec![Diagnostic {
                code: catalog::CAP_RUNTIME_ARTIFACT_MISMATCH,
                severity: Severity::Error,
                resource: Some("deployment.support_chat_local".to_owned()),
                path: Some("runtime".to_owned()),
                message: "Runtime 'llama_cpp' requires a GGUF model artifact.".to_owned(),
                source: Some(SourceLocation {
                    file: "amicu.hcl".to_owned(),
                    line: 18,
                    column: 3,
                }),
                suggested_fixes: vec![SuggestedFix {
                    description: "Select a compatible GGUF artifact.".to_owned(),
                    patch: Patch {
                        op: "replace".to_owned(),
                        path: "/resources/model/support_chat/format".to_owned(),
                        value: json!("gguf"),
                    },
                }],
            }],
        };

        let expected = json!({
            "version": "v1",
            "valid": false,
            "diagnostics": [
                {
                    "code": "AMICU-CAP-001",
                    "severity": "error",
                    "resource": "deployment.support_chat_local",
                    "path": "runtime",
                    "message": "Runtime 'llama_cpp' requires a GGUF model artifact.",
                    "source": {"file": "amicu.hcl", "line": 18, "column": 3},
                    "suggested_fixes": [
                        {
                            "description": "Select a compatible GGUF artifact.",
                            "patch": {
                                "op": "replace",
                                "path": "/resources/model/support_chat/format",
                                "value": "gguf"
                            }
                        }
                    ]
                }
            ]
        });

        assert_eq!(serde_json::to_value(&report).unwrap(), expected);
    }

    #[test]
    fn minimal_diagnostic_omits_optional_fields() {
        let report = DiagnosticReport::new(vec![Diagnostic::new(
            catalog::STATE_LOCKED,
            Severity::Warning,
            "State is locked.",
        )]);

        let value = serde_json::to_value(&report).unwrap();
        let diag = &value["diagnostics"][0];
        assert!(diag.get("resource").is_none());
        assert!(diag.get("path").is_none());
        assert!(diag.get("source").is_none());
        assert!(diag.get("suggested_fixes").is_none());
        // No error-severity diagnostics => valid.
        assert_eq!(value["valid"], json!(true));
        assert_eq!(
            diag,
            &json!({
                "code": "AMICU-STATE-001",
                "severity": "warning",
                "message": "State is locked."
            })
        );
    }

    #[test]
    fn report_roundtrips_serde() {
        let report = DiagnosticReport::new(vec![Diagnostic::new(
            catalog::CAP_RUNTIME_ARTIFACT_MISMATCH,
            Severity::Error,
            "boom",
        )]);
        let json = serde_json::to_string(&report).unwrap();
        let back: DiagnosticReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
        assert!(!back.valid);
    }
}
