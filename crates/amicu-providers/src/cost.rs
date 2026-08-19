//! Cost-estimation primitives over provider [`Pricing`].
//!
//! Turns a provider price list plus a usage input into an
//! [`EstimateRange<Money>`]: the on-demand rate drives the upper bound and, when
//! the offering is interruptible, the spot rate drives the lower bound. Every
//! estimate is a formula over a price list — no direct observation — so it
//! carries [`Provenance::ProviderPrice`] and [`Confidence::Estimated`], and
//! records the assumptions it rests on. These are planning estimates, never
//! guarantees.

use amicu_core::estimate::{Assumption, Confidence, EstimateRange, Provenance};
use amicu_core::resource::Money;

use crate::capability::{Pricing, Rate, RateUnit};

/// Hours per month assumed when normalizing a monthly rate to usage hours.
pub const HOURS_PER_MONTH: f64 = 730.0;

/// Error produced when a cost estimate cannot be formed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CostError {
    /// On-demand and spot rates are priced in different currencies; the
    /// primitives never silently mix currencies.
    #[error("currency mismatch: on-demand is {on_demand}, spot is {spot}")]
    CurrencyMismatch {
        /// On-demand currency code.
        on_demand: String,
        /// Spot currency code.
        spot: String,
    },
}

/// A usage input for a cost estimate: a number of billed hours across a count of
/// identical instances.
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    /// Billed duration in hours.
    pub hours: f64,
    /// Number of identical instances running for that duration.
    pub quantity: u32,
}

impl Usage {
    /// `hours` on a single instance.
    #[must_use]
    pub fn for_hours(hours: f64) -> Self {
        Self { hours, quantity: 1 }
    }

    /// A single instance for one month ([`HOURS_PER_MONTH`] hours).
    #[must_use]
    pub fn monthly() -> Self {
        Self::for_hours(HOURS_PER_MONTH)
    }

    /// Set the instance count.
    #[must_use]
    pub fn with_quantity(mut self, quantity: u32) -> Self {
        self.quantity = quantity;
        self
    }
}

/// The `unit` token as it reads in an assumption line.
fn unit_label(unit: RateUnit) -> &'static str {
    match unit {
        RateUnit::PerSecond => "second",
        RateUnit::PerMinute => "minute",
        RateUnit::PerHour => "hour",
        RateUnit::PerMonth => "month",
    }
}

/// The rate's amount normalized to a per-hour figure.
fn per_hour_amount(rate: &Rate) -> f64 {
    let amount = rate.price.amount;
    match rate.unit {
        RateUnit::PerSecond => amount * 3600.0,
        RateUnit::PerMinute => amount * 60.0,
        RateUnit::PerHour => amount,
        RateUnit::PerMonth => amount / HOURS_PER_MONTH,
    }
}

/// Total cost of `usage` at `rate`, in the rate's currency.
fn rate_cost(rate: &Rate, usage: &Usage) -> Money {
    let amount = per_hour_amount(rate) * usage.hours * f64::from(usage.quantity);
    Money::new(amount, rate.price.currency.clone())
}

/// Estimate the cost of running `usage` against `pricing`.
///
/// The upper bound is the guaranteed on-demand cost. The lower bound is the
/// spot cost when the offering is interruptible, otherwise it equals the
/// on-demand cost (the range collapses to a point). The result carries
/// [`Provenance::ProviderPrice`], [`Confidence::Estimated`], and the assumptions
/// used.
///
/// # Errors
///
/// Returns [`CostError::CurrencyMismatch`] when the spot rate is priced in a
/// different currency than the on-demand rate.
pub fn estimate_cost(pricing: &Pricing, usage: &Usage) -> Result<EstimateRange<Money>, CostError> {
    let on_demand = pricing.on_demand_rate();
    let high = rate_cost(on_demand, usage);

    let mut assumptions = vec![
        Assumption::new(format!(
            "usage: {} hours x {} instance(s)",
            usage.hours, usage.quantity
        )),
        Assumption::new(format!(
            "on-demand basis: {} {} per {}",
            on_demand.price.amount,
            on_demand.price.currency,
            unit_label(on_demand.unit),
        )),
    ];

    let mut uses_monthly = on_demand.unit == RateUnit::PerMonth;

    let low = if let Some(spot) = pricing.spot_rate() {
        if spot.price.currency != on_demand.price.currency {
            return Err(CostError::CurrencyMismatch {
                on_demand: on_demand.price.currency.clone(),
                spot: spot.price.currency.clone(),
            });
        }
        uses_monthly |= spot.unit == RateUnit::PerMonth;
        assumptions.push(Assumption::new(format!(
            "spot basis: {} {} per {}",
            spot.price.amount,
            spot.price.currency,
            unit_label(spot.unit),
        )));
        assumptions.push(Assumption::new("spot availability assumed for lower bound"));
        rate_cost(spot, usage)
    } else {
        assumptions.push(Assumption::new(
            "no spot rate offered; lower bound equals on-demand",
        ));
        high.clone()
    };

    if uses_monthly {
        assumptions.push(Assumption::new(format!(
            "{HOURS_PER_MONTH} hours/month assumed for monthly-rate normalization"
        )));
    }

    Ok(
        EstimateRange::new(low, high, Provenance::ProviderPrice, Confidence::Estimated)
            .with_assumptions(assumptions),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(amount: f64) -> Money {
        Money::new(amount, "USD")
    }

    #[test]
    fn on_demand_and_spot_feed_range_bounds() {
        let pricing = Pricing::on_demand(Rate::new(usd(2.0), RateUnit::PerHour))
            .with_spot(Rate::new(usd(0.5), RateUnit::PerHour));

        let estimate = estimate_cost(&pricing, &Usage::monthly()).unwrap();

        // Spot drives the low bound, on-demand the high bound.
        assert_eq!(estimate.low(), &usd(365.0));
        assert_eq!(estimate.high(), &usd(1460.0));
        assert!(estimate.low().amount <= estimate.high().amount);
        assert_eq!(estimate.provenance(), Provenance::ProviderPrice);
        assert_eq!(estimate.confidence(), Confidence::Estimated);
        assert!(
            estimate
                .assumptions()
                .iter()
                .any(|a| a.statement.contains("spot availability assumed"))
        );
    }

    #[test]
    fn on_demand_only_yields_point_range_without_spot_bound() {
        let pricing = Pricing::on_demand(Rate::new(usd(3.0), RateUnit::PerHour));
        let estimate = estimate_cost(&pricing, &Usage::for_hours(10.0)).unwrap();

        assert_eq!(estimate.low(), &usd(30.0));
        assert_eq!(estimate.high(), &usd(30.0));
        assert!(estimate.low().amount <= estimate.high().amount);
        assert!(
            estimate
                .assumptions()
                .iter()
                .any(|a| a.statement.contains("no spot rate"))
        );
        assert!(
            !estimate
                .assumptions()
                .iter()
                .any(|a| a.statement.contains("spot availability"))
        );
    }

    #[test]
    fn quantity_scales_cost() {
        let pricing = Pricing::on_demand(Rate::new(usd(1.0), RateUnit::PerHour));
        let estimate = estimate_cost(&pricing, &Usage::for_hours(10.0).with_quantity(4)).unwrap();
        assert_eq!(estimate.high(), &usd(40.0));
    }

    #[test]
    fn monthly_rate_is_normalized_and_recorded() {
        let pricing = Pricing::on_demand(Rate::new(usd(730.0), RateUnit::PerMonth));
        let estimate = estimate_cost(&pricing, &Usage::for_hours(1.0)).unwrap();
        // 730/month over 730h/month == 1.0/hour.
        assert_eq!(estimate.high(), &usd(1.0));
        assert!(
            estimate
                .assumptions()
                .iter()
                .any(|a| a.statement.contains("hours/month"))
        );
    }

    #[test]
    fn currency_mismatch_is_rejected() {
        let pricing = Pricing::on_demand(Rate::new(usd(2.0), RateUnit::PerHour))
            .with_spot(Rate::new(Money::new(0.5, "EUR"), RateUnit::PerHour));
        let err = estimate_cost(&pricing, &Usage::monthly()).unwrap_err();
        assert_eq!(
            err,
            CostError::CurrencyMismatch {
                on_demand: "USD".into(),
                spot: "EUR".into(),
            }
        );
    }

    #[test]
    fn estimate_round_trips_serde() {
        let pricing = Pricing::on_demand(Rate::new(usd(2.0), RateUnit::PerHour))
            .with_spot(Rate::new(usd(0.5), RateUnit::PerHour));
        let estimate = estimate_cost(&pricing, &Usage::monthly()).unwrap();
        let json = serde_json::to_string(&estimate).unwrap();
        let back: EstimateRange<Money> = serde_json::from_str(&json).unwrap();
        assert_eq!(estimate, back);
        assert!(json.contains("\"provider_price\""));
        assert!(json.contains("\"estimated\""));
    }
}
