//! Typed region and data-residency values.

use serde::{Deserialize, Serialize};

/// The data-residency jurisdiction a [`Region`] falls under.
///
/// Residency is a first-class constraint: the planner rejects any provider or
/// region whose jurisdiction violates a declared `data_class` residency or
/// provider allowlist. Modeling it as a typed value rather than a bare string
/// lets that check be reasoned about rather than string-matched.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Residency {
    /// European Union / EEA.
    Eu,
    /// United Kingdom.
    Uk,
    /// United States.
    Us,
    /// Canada.
    Ca,
    /// Asia-Pacific.
    Apac,
    /// A jurisdiction not covered by the named variants.
    Other(String),
}

impl Residency {
    /// Whether this jurisdiction is the European Union.
    #[must_use]
    pub fn is_eu(&self) -> bool {
        matches!(self, Residency::Eu)
    }
}

/// A provider region, tagged with the residency jurisdiction it belongs to.
///
/// `code` is the provider-agnostic region identifier used in the DSL and
/// planner; `residency` is the typed jurisdiction the planner filters on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    /// Region code, e.g. `"eu-central-1"` or `"fin-north"`.
    pub code: String,
    /// The data-residency jurisdiction of this region.
    pub residency: Residency,
    /// Human-readable location, e.g. `"Frankfurt"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// ISO 3166-1 alpha-2 country code, e.g. `"DE"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

impl Region {
    /// A region with the given code and residency jurisdiction.
    #[must_use]
    pub fn new(code: impl Into<String>, residency: Residency) -> Self {
        Self {
            code: code.into(),
            residency,
            display_name: None,
            country: None,
        }
    }

    /// Set the human-readable location name.
    #[must_use]
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// Set the ISO country code.
    #[must_use]
    pub fn with_country(mut self, country: impl Into<String>) -> Self {
        self.country = Some(country.into());
        self
    }

    /// Whether this region resides in the European Union.
    #[must_use]
    pub fn is_eu(&self) -> bool {
        self.residency.is_eu()
    }
}
