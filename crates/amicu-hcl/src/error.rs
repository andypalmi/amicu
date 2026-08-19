use std::fmt;

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
        }
    }
}

impl std::error::Error for HclError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            Self::Schema(_) | Self::InvalidIdentifier { .. } => None,
        }
    }
}

impl From<hcl::Error> for HclError {
    fn from(e: hcl::Error) -> Self {
        Self::Parse(e)
    }
}
