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
    /// covered by a more specific error).
    EmptyMarket,
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
        }
    }
}

impl std::error::Error for LendingError {}
