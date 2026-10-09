use std::fmt;

use amicu_core::diagnostic::{DiagnosticCode, catalog};

/// Failure translating between HCL text and the canonical [`Graph`].
///
/// [`Graph`]: amicu_core::graph::Graph
#[derive(Debug)]
pub enum HclError {
    /// The input was not syntactically valid HCL.
    Parse(hcl::Error),
    /// The HCL parsed but did not match the expected resource schema.
    Schema(String),
    /// A graph carried a resource id or label key that is not a valid,
    /// round-trip-safe HCL identifier, so it cannot be formatted without
    /// silent corruption. `resource` locates the offender; `value` is the
    /// offending string.
    InvalidIdentifier { resource: String, value: String },
    /// A secret-designated field (`api_key`, `token`, `password`, or an
    /// `env` map value) was given as a literal instead of a `secret(...)`
    /// reference (amicu-planning#119). Structural, hard rejection: `code` is
    /// [`catalog::POLICY_SECRET_LITERAL_REJECTED`], `resource` locates the
    /// block, `field` names the offending attribute.
    SecretLiteral {
        code: DiagnosticCode,
        resource: String,
        field: String,
    },
    /// An attribute or nested block the schema does not define
    /// (amicu-planning#157). `code` is [`catalog::SCHEMA_UNKNOWN_ATTRIBUTE`],
    /// `context` locates the enclosing block (e.g. `deployment chat.llama_cpp`),
    /// `kind` is `"attribute"` or `"block"`, `name` the offending key, and
    /// `suggestion` a known key within a small edit distance, if any.
    UnknownField {
        code: DiagnosticCode,
        context: String,
        kind: &'static str,
        name: String,
        suggestion: Option<String>,
    },
}

impl HclError {
    pub(crate) fn schema(msg: impl Into<String>) -> Self {
        Self::Schema(msg.into())
    }

    pub(crate) fn invalid_identifier(
        resource: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self::InvalidIdentifier {
            resource: resource.into(),
            value: value.into(),
        }
    }

    pub(crate) fn secret_literal(resource: impl Into<String>, field: impl Into<String>) -> Self {
        Self::SecretLiteral {
            code: catalog::POLICY_SECRET_LITERAL_REJECTED,
            resource: resource.into(),
            field: field.into(),
        }
    }

    pub(crate) fn unknown_field(
        context: impl Into<String>,
        kind: &'static str,
        name: impl Into<String>,
        suggestion: Option<String>,
    ) -> Self {
        Self::UnknownField {
            code: catalog::SCHEMA_UNKNOWN_ATTRIBUTE,
            context: context.into(),
            kind,
            name: name.into(),
            suggestion,
        }
    }

    /// The standardized diagnostic code for this error, when it has one.
    #[must_use]
    pub const fn code(&self) -> Option<DiagnosticCode> {
        match self {
            Self::SecretLiteral { code, .. } | Self::UnknownField { code, .. } => Some(*code),
            Self::Parse(_) | Self::Schema(_) | Self::InvalidIdentifier { .. } => None,
        }
    }
}

impl fmt::Display for HclError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "HCL syntax error: {e}"),
            Self::Schema(msg) => write!(f, "invalid resource: {msg}"),
            Self::InvalidIdentifier { resource, value } => write!(
                f,
                "cannot format {resource}: `{value}` is not a valid, round-trip-safe HCL \
                 identifier (it must be non-empty, start with a letter or underscore, and \
                 contain no `.` or whitespace)"
            ),
            Self::SecretLiteral {
                code,
                resource,
                field,
            } => write!(
                f,
                "{code}: {resource}: `{field}` is a secret-designated field and must be a \
                 secret reference, e.g. `{field} = secret(\"vault://...\")`, not a literal value"
            ),
            Self::UnknownField {
                code,
                context,
                kind,
                name,
                suggestion,
            } => {
                write!(f, "{code}: {context}: unknown {kind} `{name}`")?;
                if let Some(s) = suggestion {
                    write!(f, "; did you mean `{s}`?")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for HclError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            Self::Schema(_)
            | Self::InvalidIdentifier { .. }
            | Self::SecretLiteral { .. }
            | Self::UnknownField { .. } => None,
        }
    }
}

impl From<hcl::Error> for HclError {
    fn from(e: hcl::Error) -> Self {
        Self::Parse(e)
    }
}
