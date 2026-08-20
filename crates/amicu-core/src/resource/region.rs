//! Typed region and data-residency values.
//!
//! Shared by the DSL/core resources and the provider capability descriptor so
//! residency is one canonical typed value on both sides rather than a bare
//! string that must be reconciled.

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

    /// The canonical lowercase token for this jurisdiction, matching the DSL and
    /// canonical JSON spelling of the named variants; [`Residency::Other`]
    /// yields its raw string. Inverse of [`Residency::from_token`].
    #[must_use]
    pub fn as_token(&self) -> &str {
        match self {
            Residency::Eu => "eu",
            Residency::Uk => "uk",
            Residency::Us => "us",
            Residency::Ca => "ca",
            Residency::Apac => "apac",
            Residency::Other(s) => s,
        }
    }

    /// Parse a jurisdiction token, mapping any unrecognized token to
    /// [`Residency::Other`]. Inverse of [`Residency::as_token`]; used where the
    /// DSL supplies a bare string with no enum tagging. Not a total inverse:
    /// a named token always parses to its named variant, so `Other("eu")` is
    /// not reproduced — such values never reach this path (the parser only
    /// emits `Other` for genuinely unknown tokens).
    #[must_use]
    pub fn from_token(token: impl Into<String>) -> Self {
        let token = token.into();
        match token.as_str() {
            "eu" => Residency::Eu,
            "uk" => Residency::Uk,
            "us" => Residency::Us,
            "ca" => Residency::Ca,
            "apac" => Residency::Apac,
            _ => Residency::Other(token),
        }
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

    /// A region known only by its `code`, with residency left unresolved.
    ///
    /// Used where only a bare code is available (e.g. parsed from the DSL, which
    /// carries no jurisdiction): the residency is a sentinel to be resolved later
    /// against the provider catalog, not a claim that the region is
    /// [`Residency::Other`].
    #[must_use]
    pub fn from_code(code: impl Into<String>) -> Self {
        Self::new(code, Residency::Other(String::new()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residency_token_round_trips() {
        for r in [
            Residency::Eu,
            Residency::Uk,
            Residency::Us,
            Residency::Ca,
            Residency::Apac,
            Residency::Other("antarctica".into()),
        ] {
            assert_eq!(Residency::from_token(r.as_token()), r);
        }
    }

    #[test]
    fn unknown_token_becomes_other() {
        assert_eq!(
            Residency::from_token("latam"),
            Residency::Other("latam".into())
        );
    }
}
