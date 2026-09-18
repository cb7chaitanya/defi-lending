use core::fmt;

/// All failure modes exposed by this crate.
///
/// Every fallible entry point in [`crate::market::LendingMarket`] and
/// [`crate::rate_model::InterestRateModel`] returns one of these instead of
/// panicking, wrapping, or silently truncating. No mutating method commits
/// any partial state when it returns an `Err` — see the crate-level README
/// for the atomicity discipline this crate follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LendingError {
    /// An amount parameter was zero where a nonzero amount is required.
    ZeroAmount,
    /// [`crate::rate_model::InterestRateModel::new`] was given an invalid
    /// configuration (e.g. optimal utilization not strictly between 0 and
    /// 100%, or a utilization argument outside `[0, RATE_SCALE]`).
    InvalidRateConfig(&'static str),
    /// The reserve factor was outside `[0, RATE_SCALE]` (i.e. not a
    /// fraction between 0% and 100%).
    InvalidReserveFactor,
    /// The market does not currently hold enough `cash` to satisfy a
    /// withdrawal or a borrow. This is a *liquidity* failure, distinct
    /// from [`LendingError::InsufficientSupplyShares`], which is a
    /// *solvency* failure. See the README section "Solvency vs. liquidity".
    InsufficientCash,
    /// The account does not hold enough supply shares to burn the
    /// requested amount.
    InsufficientSupplyShares,
    /// The account does not hold enough debt shares to repay the
    /// requested amount (this includes repaying more than is owed).
    InsufficientDebt,
    /// The requested operation is nonzero in asset terms but would round
    /// to zero shares (or zero assets), which would be a silent no-op.
    /// This is rejected rather than allowed to pass through.
    RoundsToZero,
    /// A timestamp was supplied that precedes the market's last accrual
    /// timestamp. Time must be monotonically non-decreasing.
    BackwardsTimestamp,
    /// A checked arithmetic operation would have overflowed.
    ArithmeticOverflow,
    /// The operation is undefined on an empty market (e.g. computing an
    /// exchange rate or a ratio against a zero denominator that is not
    /// covered by a more specific error). Also used for a handful of
    /// Day 12 risk queries whose ratio is genuinely undefined (e.g. a
    /// zero-collateral denominator, or a "liquidation price" query for an
    /// asset that cannot, at any finite price, bring the account to the
    /// liquidation boundary).
    EmptyMarket,

    // ---- Day 12: collateral, oracle, and risk errors -----------------
    /// A collateral operation referenced an [`crate::oracle::AssetId`]
    /// that was never registered with
    /// [`crate::risk::RiskMarket::configure_collateral`].
    UnknownCollateralAsset,
    /// A collateral deposit targeted an asset whose configuration has
    /// `enabled == false`. Existing balances of a disabled asset can
    /// still be withdrawn and still count toward valuation — disabling
    /// only blocks *new* deposits.
    CollateralAssetDisabled,
    /// [`crate::risk::RiskMarket::configure_collateral`] was called twice
    /// for the same [`crate::oracle::AssetId`].
    DuplicateCollateralAsset,
    /// A [`crate::collateral::CollateralConfig`] or
    /// [`crate::collateral::DebtAssetConfig`] failed validation (e.g.
    /// `max_ltv > liquidation_threshold`, a ratio outside
    /// `[0, RATE_SCALE]`, a zero `liquidation_threshold` on enabled
    /// collateral, or a zero quantity scale).
    InvalidCollateralConfig(&'static str),
    /// A collateral withdrawal requested more than the account's current
    /// balance of that asset.
    InsufficientCollateral,
    /// A proposed borrow or collateral withdrawal would leave the
    /// account's debt value above its maximum borrowing capacity
    /// (`sum(collateral_value_i * max_ltv_i)`).
    ExceedsMaxLtv,
    /// A proposed collateral withdrawal would leave the account's health
    /// factor below `1.0` (see [`crate::risk::HealthFactor`]).
    HealthFactorTooLow,
    /// A risk-sensitive operation needed a price for an asset that has no
    /// entry in the supplied [`crate::oracle::PriceBook`].
    MissingPrice,
    /// [`crate::oracle::PriceQuote::new`] was given a zero price — prices
    /// must always be strictly positive.
    ZeroPrice,
    /// A price quote's age (`evaluation_timestamp - observed_at`) exceeds
    /// its configured `max_age_seconds`.
    StalePrice,
    /// A price quote's `observed_at` is later than the timestamp it is
    /// being evaluated at. This crate's oracle model always rejects
    /// future-dated observations — see the README's "Oracle model"
    /// section.
    FuturePriceObservation,
}

impl fmt::Display for LendingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroAmount => write!(f, "amount must be nonzero"),
            Self::InvalidRateConfig(reason) => {
                write!(f, "invalid interest rate configuration: {reason}")
            }
            Self::InvalidReserveFactor => {
                write!(
                    f,
                    "reserve factor must be within [0, RATE_SCALE] (0%..=100%)"
                )
            }
            Self::InsufficientCash => {
                write!(
                    f,
                    "operation requires more cash than is available in the market"
                )
            }
            Self::InsufficientSupplyShares => {
                write!(
                    f,
                    "account does not hold enough supply shares for this operation"
                )
            }
            Self::InsufficientDebt => {
                write!(f, "account does not hold enough debt to repay that amount")
            }
            Self::RoundsToZero => write!(
                f,
                "amount rounds to zero shares or zero assets and would be a silent no-op"
            ),
            Self::BackwardsTimestamp => {
                write!(f, "timestamp precedes the market's last accrual timestamp")
            }
            Self::ArithmeticOverflow => write!(f, "arithmetic overflow in checked computation"),
            Self::EmptyMarket => write!(f, "operation is undefined on an empty market"),
            Self::UnknownCollateralAsset => {
                write!(f, "asset is not configured as collateral on this market")
            }
            Self::CollateralAssetDisabled => {
                write!(f, "collateral asset is disabled for new deposits")
            }
            Self::DuplicateCollateralAsset => {
                write!(f, "collateral asset is already configured")
            }
            Self::InvalidCollateralConfig(reason) => {
                write!(f, "invalid collateral configuration: {reason}")
            }
            Self::InsufficientCollateral => {
                write!(f, "account does not hold enough of that collateral asset")
            }
            Self::ExceedsMaxLtv => write!(
                f,
                "operation would leave debt value above maximum borrowing capacity"
            ),
            Self::HealthFactorTooLow => write!(
                f,
                "operation would leave the account's health factor below 1.0"
            ),
            Self::MissingPrice => write!(f, "no price is available for that asset"),
            Self::ZeroPrice => write!(f, "price must be strictly positive"),
            Self::StalePrice => write!(f, "price observation is older than its maximum age"),
            Self::FuturePriceObservation => {
                write!(f, "price observation timestamp is in the future")
            }
        }
    }
}

impl std::error::Error for LendingError {}
