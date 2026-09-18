//! Per-asset risk configuration for Day 12's collateral model.
//!
//! Every parameter here (`max_ltv`, `liquidation_threshold`,
//! `quantity_scale`) is a **per-asset policy choice**, not a universal
//! protocol constant — see the README's Day 12 section for why real
//! protocols set these per-asset based on that asset's liquidity and
//! volatility, and why this crate treats them the same way rather than
//! hard-coding a single risk parameter for every collateral type.

use crate::error::LendingError;
use crate::math::RATE_SCALE;
use crate::oracle::AssetId;

/// Configuration for one collateral asset.
///
/// `max_ltv` and `liquidation_threshold` are fixed-point fractions scaled
/// by [`RATE_SCALE`] (so `RATE_SCALE` means 100%). `quantity_scale` is the
/// number of smallest on-chain units equal to one whole unit of the asset
/// (e.g. `1_000_000_000` for a 9-decimal token) — an explicit
/// quantity-scaling convention, used exactly like a token decimals count
/// but avoiding a separate checked power-of-ten helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollateralConfig {
    asset: AssetId,
    max_ltv: u128,
    liquidation_threshold: u128,
    quantity_scale: u128,
    enabled: bool,
}

impl CollateralConfig {
    /// Builds a validated collateral configuration.
    ///
    /// Rejects (all as [`LendingError::InvalidCollateralConfig`]):
    /// - `quantity_scale == 0` (undefined valuation);
    /// - `max_ltv` or `liquidation_threshold` outside `[0, RATE_SCALE]`;
    /// - `max_ltv > liquidation_threshold` (a borrower could otherwise be
    ///   allowed to borrow past the point at which they're liquidatable);
    /// - `enabled == true` with `liquidation_threshold == 0` (an enabled
    ///   asset that contributes nothing to liquidation-adjusted collateral
    ///   would silently make every position backed by it unliquidatable
    ///   while still counting as collateral for valuation — a zero
    ///   threshold is only sensible for an asset that is fully disabled).
    pub fn new(
        asset: AssetId,
        max_ltv: u128,
        liquidation_threshold: u128,
        quantity_scale: u128,
        enabled: bool,
    ) -> Result<Self, LendingError> {
        if quantity_scale == 0 {
            return Err(LendingError::InvalidCollateralConfig(
                "quantity_scale must be nonzero",
            ));
        }
        if max_ltv > RATE_SCALE || liquidation_threshold > RATE_SCALE {
            return Err(LendingError::InvalidCollateralConfig(
                "max_ltv and liquidation_threshold must each be within [0, RATE_SCALE]",
            ));
        }
        if max_ltv > liquidation_threshold {
            return Err(LendingError::InvalidCollateralConfig(
                "max_ltv must not exceed liquidation_threshold",
            ));
        }
        if enabled && liquidation_threshold == 0 {
            return Err(LendingError::InvalidCollateralConfig(
                "enabled collateral must have a nonzero liquidation_threshold",
            ));
        }
        Ok(Self {
            asset,
            max_ltv,
            liquidation_threshold,
            quantity_scale,
            enabled,
        })
    }

    /// The asset this configuration applies to.
    pub fn asset(&self) -> AssetId {
        self.asset
    }

    /// The maximum loan-to-value ratio for new borrowing against this
    /// collateral, scaled by [`RATE_SCALE`].
    pub fn max_ltv(&self) -> u128 {
        self.max_ltv
    }

    /// The loan-to-value ratio above which a position backed by this
    /// collateral becomes liquidatable, scaled by [`RATE_SCALE`].
    pub fn liquidation_threshold(&self) -> u128 {
        self.liquidation_threshold
    }

    /// The number of smallest on-chain units equal to one whole unit of
    /// this asset.
    pub fn quantity_scale(&self) -> u128 {
        self.quantity_scale
    }

    /// Whether new deposits of this asset are currently accepted.
    /// Disabling an asset never affects existing balances: they still
    /// count toward valuation and can still be withdrawn.
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// Configuration for the market's single borrow-able ("debt") asset, used
/// only to value outstanding debt in the common quote unit. `quantity_scale`
/// has the same meaning as on [`CollateralConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebtAssetConfig {
    asset: AssetId,
    quantity_scale: u128,
}

impl DebtAssetConfig {
    /// Builds a validated debt-asset configuration. Rejects
    /// `quantity_scale == 0`.
    pub fn new(asset: AssetId, quantity_scale: u128) -> Result<Self, LendingError> {
        if quantity_scale == 0 {
            return Err(LendingError::InvalidCollateralConfig(
                "quantity_scale must be nonzero",
            ));
        }
        Ok(Self {
            asset,
            quantity_scale,
        })
    }

    /// The market's debt asset.
    pub fn asset(&self) -> AssetId {
        self.asset
    }

    /// The number of smallest on-chain units equal to one whole unit of
    /// the debt asset.
    pub fn quantity_scale(&self) -> u128 {
        self.quantity_scale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: AssetId = AssetId(1);

    #[test]
    fn rejects_max_ltv_above_liquidation_threshold() {
        let result =
            CollateralConfig::new(SOL, RATE_SCALE * 90 / 100, RATE_SCALE * 80 / 100, 1, true);
        assert!(matches!(
            result,
            Err(LendingError::InvalidCollateralConfig(_))
        ));
    }

    #[test]
    fn rejects_ratios_above_rate_scale() {
        let result = CollateralConfig::new(SOL, RATE_SCALE + 1, RATE_SCALE, 1, true);
        assert!(matches!(
            result,
            Err(LendingError::InvalidCollateralConfig(_))
        ));
    }

    #[test]
    fn rejects_zero_liquidation_threshold_when_enabled() {
        let result = CollateralConfig::new(SOL, 0, 0, 1, true);
        assert!(matches!(
            result,
            Err(LendingError::InvalidCollateralConfig(_))
        ));
    }

    #[test]
    fn allows_zero_liquidation_threshold_when_disabled() {
        let result = CollateralConfig::new(SOL, 0, 0, 1, false);
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_zero_quantity_scale() {
        let result = CollateralConfig::new(SOL, 0, RATE_SCALE, 0, false);
        assert!(matches!(
            result,
            Err(LendingError::InvalidCollateralConfig(_))
        ));
        assert!(matches!(
            DebtAssetConfig::new(SOL, 0),
            Err(LendingError::InvalidCollateralConfig(_))
        ));
    }

    #[test]
    fn accepts_valid_configuration() {
        let config = CollateralConfig::new(
            SOL,
            RATE_SCALE * 70 / 100,
            RATE_SCALE * 80 / 100,
            1_000_000_000,
            true,
        )
        .unwrap();
        assert_eq!(config.max_ltv(), RATE_SCALE * 70 / 100);
        assert_eq!(config.liquidation_threshold(), RATE_SCALE * 80 / 100);
        assert!(config.enabled());
    }
}
