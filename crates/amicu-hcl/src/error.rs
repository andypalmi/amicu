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
}

impl HclError {
    pub(crate) fn schema(msg: impl Into<String>) -> Self {
        Self::Schema(msg.into())
    }
}

impl fmt::Display for HclError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "HCL syntax error: {e}"),
            Self::Schema(msg) => write!(f, "invalid resource: {msg}"),
        }
    }
}

impl std::error::Error for HclError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            Self::Schema(_) => None,
        }
    }
}

impl From<hcl::Error> for HclError {
    fn from(e: hcl::Error) -> Self {
        Self::Parse(e)
    }
}
