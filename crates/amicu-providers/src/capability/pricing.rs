//! Provider pricing: on-demand and spot rates, and billing granularity.

use std::collections::BTreeMap;

use amicu_core::resource::Money;
use serde::{Deserialize, Serialize};

/// The time unit a [`Rate`] is charged against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateUnit {
    PerSecond,
    PerMinute,
    PerHour,
    PerMonth,
}

/// The smallest increment a provider bills usage in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingGranularity {
    PerSecond,
    PerMinute,
    PerHour,
    PerDay,
    PerMonth,
}

/// A single price: a monetary amount per unit of time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rate {
    /// Amount and ISO currency; reuses [`amicu_core::resource::Money`].
    pub price: Money,
    /// The time unit the amount applies to.
    pub unit: RateUnit,
}

impl Rate {
    /// A rate of `price` per `unit`.
    #[must_use]
    pub fn new(price: Money, unit: RateUnit) -> Self {
        Self { price, unit }
    }
}

/// Pricing for a compute offering: an on-demand rate and, when the provider
/// offers interruptible capacity, a spot rate.
///
/// Both rates are exposed so cost estimation can produce a range
/// spanning the interruptible floor and the on-demand ceiling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    /// The guaranteed on-demand rate.
    pub on_demand: Rate,
    /// The interruptible spot rate, when offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot: Option<Rate>,
    /// Per-region price multipliers applied on top of both rates when a
    /// deployment pins a region (e.g. Modal charges 1.15x for a broad region
    /// and 1.75x for a narrow one). Keyed by region code; a region with no
    /// entry - and an unpinned deployment - is charged the base rate (1.0x).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub region_multipliers: BTreeMap<String, f64>,
}

impl Pricing {
    /// Pricing with only an on-demand rate.
    #[must_use]
    pub fn on_demand(rate: Rate) -> Self {
        Self {
            on_demand: rate,
            spot: None,
            region_multipliers: BTreeMap::new(),
        }
    }

    /// Attach a spot / interruptible rate.
    #[must_use]
    pub fn with_spot(mut self, rate: Rate) -> Self {
        self.spot = Some(rate);
        self
    }

    /// Charge `multiplier` times the base rate in `region`.
    #[must_use]
    pub fn with_region_multiplier(mut self, region: impl Into<String>, multiplier: f64) -> Self {
        self.region_multipliers.insert(region.into(), multiplier);
        self
    }

    /// The price multiplier for `region`: the registered one, or `1.0` for an
    /// unpinned (`None`) or unlisted region.
    #[must_use]
    pub fn region_multiplier(&self, region: Option<&str>) -> f64 {
        region
            .and_then(|r| self.region_multipliers.get(r))
            .copied()
            .unwrap_or(1.0)
    }

    /// The on-demand rate.
    #[must_use]
    pub fn on_demand_rate(&self) -> &Rate {
        &self.on_demand
    }

    /// The spot rate, if the offering is interruptible.
    #[must_use]
    pub fn spot_rate(&self) -> Option<&Rate> {
        self.spot.as_ref()
    }

    /// Whether an interruptible spot rate is available.
    #[must_use]
    pub fn has_spot(&self) -> bool {
        self.spot.is_some()
    }
}
