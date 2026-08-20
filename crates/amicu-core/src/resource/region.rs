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
///
/// `default_zone` is an optional third level the adapter owns: the sub-region
/// zone (AZ) a zone-addressed provider API targets by default, e.g. `"fr-par-1"`
/// for `"fr-par"`. It is a placement detail, never a planning input — the DSL
/// and planner stay at residency + region, and the zone never reaches the
/// residency filter.
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
    /// Default zone (AZ) for zone-addressed provider APIs, e.g. `"fr-par-1"`.
    /// Adapter-owned; `None` for regions whose APIs address the region directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_zone: Option<String>,
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
            default_zone: None,
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

    /// Set the adapter-owned default zone (AZ).
    #[must_use]
    pub fn with_default_zone(mut self, zone: impl Into<String>) -> Self {
        self.default_zone = Some(zone.into());
        self
    }

    /// Whether this region resides in the European Union.
    #[must_use]
    pub fn is_eu(&self) -> bool {
        self.residency.is_eu()
    }

    /// Resolve the zone a zone-addressed provider API should target: the explicit
    /// `override_zone` when the caller supplied one, else this region's
    /// [`default_zone`](Self::default_zone). `None` when neither is set — the API
    /// addresses the region directly.
    #[must_use]
    pub fn resolve_zone<'a>(&'a self, override_zone: Option<&'a str>) -> Option<&'a str> {
        override_zone.or(self.default_zone.as_deref())
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

    #[test]
    fn region_has_no_zone_by_default() {
        // The zone is additive: existing constructors leave it unset.
        assert!(Region::new("fr-par", Residency::Eu).default_zone.is_none());
        assert!(Region::from_code("fr-par").default_zone.is_none());
    }

    #[test]
    fn resolve_zone_prefers_override_then_default() {
        let region = Region::new("fr-par", Residency::Eu).with_default_zone("fr-par-1");
        // Explicit override wins.
        assert_eq!(region.resolve_zone(Some("fr-par-2")), Some("fr-par-2"));
        // Falls back to the region's default zone.
        assert_eq!(region.resolve_zone(None), Some("fr-par-1"));
        // A region with no default zone and no override addresses the region.
        assert_eq!(
            Region::new("fr-par", Residency::Eu).resolve_zone(None),
            None
        );
    }

    #[test]
    fn zone_does_not_affect_residency() {
        // The zone is a placement detail: it never changes the jurisdiction the
        // planner filters on.
        let plain = Region::new("fr-par", Residency::Eu);
        let zoned = plain.clone().with_default_zone("fr-par-1");
        assert_eq!(plain.residency, zoned.residency);
        assert!(zoned.is_eu());
    }
}
