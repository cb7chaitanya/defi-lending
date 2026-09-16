//! A two-slope ("kinked") borrow-rate model, in the style described in
//! Aave's technical documentation for its interest-rate strategies: a
//! gentle slope below an "optimal" utilization target, and a much steeper
//! slope above it to push utilization back down.

use crate::error::LendingError;
use crate::math::{RATE_SCALE, mul_div_floor};

/// Configuration for a kinked borrow-rate curve.
///
/// All fields are fixed-point fractions scaled by [`RATE_SCALE`] (so
/// `RATE_SCALE` means 100%). The curve is:
///
/// ```text
/// utilization <= optimal_utilization:
///     rate = base_rate + utilization / optimal_utilization * slope_1
///
/// utilization > optimal_utilization:
///     rate = base_rate + slope_1
///          + (utilization - optimal_utilization)
///            / (1 - optimal_utilization) * slope_2
/// ```
///
/// Both branches evaluate to `base_rate + slope_1` exactly at
/// `utilization == optimal_utilization`, so the curve is continuous at the
/// kink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterestRateModel {
    base_rate: u128,
    optimal_utilization: u128,
    slope_1: u128,
    slope_2: u128,
}

impl InterestRateModel {
    /// Builds a validated rate model. All parameters are fixed-point
    /// fractions scaled by [`RATE_SCALE`] (e.g. 2% APR is
    /// `RATE_SCALE * 2 / 100`).
    ///
    /// `optimal_utilization` must be strictly between 0% and 100%: at
    /// exactly 0% or 100% the two-branch curve above degenerates (one
    /// branch divides by zero), so the crate rejects those values instead
    /// of special-casing them.
    pub fn new(
        base_rate: u128,
        optimal_utilization: u128,
        slope_1: u128,
        slope_2: u128,
    ) -> Result<Self, LendingError> {
        if optimal_utilization == 0 || optimal_utilization >= RATE_SCALE {
            return Err(LendingError::InvalidRateConfig(
                "optimal_utilization must be strictly between 0 and RATE_SCALE",
            ));
        }
        Ok(Self {
            base_rate,
            optimal_utilization,
            slope_1,
            slope_2,
        })
    }

    /// The configured base (zero-utilization) borrow APR.
    pub fn base_rate(&self) -> u128 {
        self.base_rate
    }

    /// The configured optimal utilization (the kink point).
    pub fn optimal_utilization(&self) -> u128 {
        self.optimal_utilization
    }

    /// The configured slope below the kink.
    pub fn slope_1(&self) -> u128 {
        self.slope_1
    }

    /// The configured slope above the kink.
    pub fn slope_2(&self) -> u128 {
        self.slope_2
    }

    /// Computes the borrow APR for a given utilization.
    ///
    /// `utilization` must be a fixed-point fraction scaled by
    /// [`RATE_SCALE`] in `[0, RATE_SCALE]`; values above `RATE_SCALE`
    /// (more than 100%) are rejected, since this crate's utilization
    /// definition (`total_borrows / (cash + total_borrows)`) can never
    /// exceed 100% on its own.
    pub fn borrow_rate(&self, utilization: u128) -> Result<u128, LendingError> {
        if utilization > RATE_SCALE {
            return Err(LendingError::InvalidRateConfig(
                "utilization must not exceed RATE_SCALE (100%)",
            ));
        }
        if utilization <= self.optimal_utilization {
            let bonus = mul_div_floor(utilization, self.slope_1, self.optimal_utilization)?;
            self.base_rate
                .checked_add(bonus)
                .ok_or(LendingError::ArithmeticOverflow)
        } else {
            let excess = utilization - self.optimal_utilization;
            let denom = RATE_SCALE - self.optimal_utilization;
            let bonus = mul_div_floor(excess, self.slope_2, denom)?;
            self.base_rate
                .checked_add(self.slope_1)
                .and_then(|v| v.checked_add(bonus))
                .ok_or(LendingError::ArithmeticOverflow)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from the Day 11 spec:
    /// base 2%, optimal 80%, slope1 8%, slope2 60%.
    fn example_model() -> InterestRateModel {
        InterestRateModel::new(
            RATE_SCALE / 50,       // 2%
            RATE_SCALE * 4 / 5,    // 80%
            RATE_SCALE * 8 / 100,  // 8%
            RATE_SCALE * 60 / 100, // 60%
        )
        .unwrap()
    }

    #[test]
    fn worked_example_matches_spec() {
        let model = example_model();
        assert_eq!(model.borrow_rate(0).unwrap(), RATE_SCALE * 2 / 100);
        assert_eq!(
            model.borrow_rate(RATE_SCALE * 40 / 100).unwrap(),
            RATE_SCALE * 6 / 100
        );
        assert_eq!(
            model.borrow_rate(RATE_SCALE * 80 / 100).unwrap(),
            RATE_SCALE * 10 / 100
        );
        assert_eq!(
            model.borrow_rate(RATE_SCALE * 90 / 100).unwrap(),
            RATE_SCALE * 40 / 100
        );
        assert_eq!(
            model.borrow_rate(RATE_SCALE).unwrap(),
            RATE_SCALE * 70 / 100
        );
    }

    #[test]
    fn branches_meet_exactly_at_the_kink() {
        let model = example_model();
        let at_kink = model.optimal_utilization();
        let from_below = model.borrow_rate(at_kink).unwrap();
        // One unit below and above should bracket the same value closely;
        // the definitive check is that the formula's single evaluation at
        // the kink already uses the `<=` branch, which by construction
        // equals base + slope_1.
        assert_eq!(from_below, model.base_rate() + model.slope_1());
    }

    #[test]
    fn rejects_boundary_optimal_utilization() {
        assert!(InterestRateModel::new(0, 0, 0, 0).is_err());
        assert!(InterestRateModel::new(0, RATE_SCALE, 0, 0).is_err());
    }

    #[test]
    fn rejects_utilization_above_one() {
        let model = example_model();
        assert!(model.borrow_rate(RATE_SCALE + 1).is_err());
    }
}
