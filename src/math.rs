//! Fixed-point constants and checked ratio arithmetic shared by the
//! interest-rate model and the market accounting core.
//!
//! Every quantity in this crate that is conceptually a fraction (a rate,
//! a utilization, a reserve factor, an exchange rate, the borrow index) is
//! represented as an integer scaled by [`RATE_SCALE`] or [`INDEX_SCALE`].
//! No floating-point arithmetic is used anywhere in the accounting path,
//! and every multiplication/division is performed through checked `u128`
//! helpers so overflow is reported as an error instead of panicking or
//! silently wrapping, per Rust's checked-arithmetic conventions
//! (<https://doc.rust-lang.org/std/primitive.u128.html#method.checked_mul>).

use crate::error::LendingError;

/// Fixed-point scale for rates, utilization and the reserve factor.
///
/// `RATE_SCALE` represents 100% (i.e. a value of `1.0`). For example a 2%
/// APR is stored as `RATE_SCALE / 50`.
pub const RATE_SCALE: u128 = 1_000_000_000;

/// Fixed-point scale for the cumulative borrow index.
///
/// The index starts at `INDEX_SCALE` (representing `1.0`) and only ever
/// grows as interest accrues. See [`crate::market`] for how scaled debt
/// balances are combined with the index to recover a borrower's current
/// debt without iterating over every account.
pub const INDEX_SCALE: u128 = 1_000_000_000_000_000_000;

/// Seconds in a 365-day year — the convention this crate uses to convert
/// an annual percentage rate (APR) into a per-second accrual rate. A
/// "quarter" / "three months" in the worked examples and tests means
/// exactly `SECONDS_PER_YEAR / 4` seconds, so that ratio arithmetic stays
/// exact instead of picking up calendar-month rounding noise.
pub const SECONDS_PER_YEAR: u64 = 365 * 24 * 60 * 60;

/// `floor(a * b / denom)`, computed with a checked `u128` intermediate.
///
/// Returns [`LendingError::ArithmeticOverflow`] if `a * b` does not fit in
/// a `u128`, and [`LendingError::EmptyMarket`] if `denom == 0` (the caller
/// is dividing by a quantity — e.g. total shares or total assets — that is
/// only zero on an empty market).
pub fn mul_div_floor(a: u128, b: u128, denom: u128) -> Result<u128, LendingError> {
    if denom == 0 {
        return Err(LendingError::EmptyMarket);
    }
    let product = a.checked_mul(b).ok_or(LendingError::ArithmeticOverflow)?;
    Ok(product / denom)
}

/// `ceil(a * b / denom)`, computed with a checked `u128` intermediate.
///
/// Same error conditions as [`mul_div_floor`].
pub fn mul_div_ceil(a: u128, b: u128, denom: u128) -> Result<u128, LendingError> {
    if denom == 0 {
        return Err(LendingError::EmptyMarket);
    }
    let product = a.checked_mul(b).ok_or(LendingError::ArithmeticOverflow)?;
    let quotient = product / denom;
    let remainder = product % denom;
    if remainder == 0 {
        Ok(quotient)
    } else {
        quotient
            .checked_add(1)
            .ok_or(LendingError::ArithmeticOverflow)
    }
}

/// Utilization, defined for this educational model as
/// `total_borrows / (cash + total_borrows)`, clamped to `[0, RATE_SCALE]`
/// and defined as `0` on an empty market (`cash == 0 && total_borrows ==
/// 0`) instead of dividing by zero.
///
/// See the crate-level README for why this denominator was chosen over
/// alternatives (e.g. some real protocols subtract reserves from cash).
pub fn utilization_raw(cash: u128, total_borrows: u128) -> Result<u128, LendingError> {
    let denom = cash
        .checked_add(total_borrows)
        .ok_or(LendingError::ArithmeticOverflow)?;
    if denom == 0 {
        return Ok(0);
    }
    mul_div_floor(total_borrows, RATE_SCALE, denom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_floor_rounds_down() {
        assert_eq!(mul_div_floor(7, 3, 2).unwrap(), 10); // 21/2 = 10.5 -> 10
    }

    #[test]
    fn mul_div_ceil_rounds_up() {
        assert_eq!(mul_div_ceil(7, 3, 2).unwrap(), 11); // 21/2 = 10.5 -> 11
    }

    #[test]
    fn mul_div_ceil_exact_no_bump() {
        assert_eq!(mul_div_ceil(4, 3, 2).unwrap(), 6); // 12/2 = 6 exactly
    }

    #[test]
    fn utilization_empty_market_is_zero() {
        assert_eq!(utilization_raw(0, 0).unwrap(), 0);
    }

    #[test]
    fn utilization_matches_scenario_one() {
        // cash = 600, borrows = 400 -> utilization 40%.
        assert_eq!(utilization_raw(600, 400).unwrap(), RATE_SCALE * 2 / 5);
    }
}
