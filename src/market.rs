//! The accounting core of a single-asset lending market.
//!
//! See the crate-level README for the full accounting model, rounding
//! policy, and worked examples. In short:
//!
//! ```text
//! supplier_assets = cash + total_borrows - protocol_reserves
//! utilization      = total_borrows / (cash + total_borrows)
//! ```
//!
//! Suppliers hold *supply shares* against `supplier_assets` (a
//! Compound-cToken-style exchange rate, see
//! <https://docs.compound.finance/v2/ctokens/>). Borrowers hold *debt
//! shares* (a scaled balance) against a cumulative `borrow_index`, in the
//! style described for Aave's reserve indexes, so the protocol never has
//! to iterate over every borrower to accrue interest.

use std::collections::BTreeMap;

use crate::error::LendingError;
use crate::math::{
    INDEX_SCALE, RATE_SCALE, SECONDS_PER_YEAR, mul_div_ceil, mul_div_floor, utilization_raw,
};
use crate::rate_model::InterestRateModel;

/// An opaque account identifier.
///
/// This is deliberately minimal for an educational library: a bare `u64`
/// newtype with no relationship to any real wallet or on-chain account
/// layout. Solana account plumbing is out of scope for Day 11.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(pub u64);

/// A snapshot of the state that interest accrual updates, produced by a
/// pure simulation and later applied verbatim by a mutating call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AccrualSnapshot {
    total_borrows: u128,
    protocol_reserves: u128,
    borrow_index: u128,
}

/// The full projected effect of a `supply` operation, returned by both the
/// pure preview and the mutating call (which is checked to produce the
/// identical value — see [`LendingMarket::supply_at`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupplyPreview {
    /// Supply shares minted to the depositor for this operation.
    pub shares_minted: u128,
    /// The depositor's total supply-share balance after this operation.
    pub user_shares_after: u128,
    /// Market cash after this operation.
    pub cash_after: u128,
    /// Total outstanding supply shares after this operation.
    pub total_supply_shares_after: u128,
    /// `cash + total_borrows - protocol_reserves` after this operation.
    pub supplier_assets_after: u128,
    /// Total borrows after interest accrual (unchanged by a supply).
    pub total_borrows_after: u128,
    /// Protocol reserves after interest accrual (unchanged by a supply).
    pub protocol_reserves_after: u128,
    /// Borrow index after interest accrual (unchanged by a supply).
    pub borrow_index_after: u128,
}

/// The full projected effect of a `withdraw` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawPreview {
    /// Supply shares burned from the withdrawer for this operation.
    pub shares_burned: u128,
    /// The withdrawer's total supply-share balance after this operation.
    pub user_shares_after: u128,
    /// Assets paid out to the withdrawer.
    pub assets_out: u128,
    /// Market cash after this operation.
    pub cash_after: u128,
    /// Total outstanding supply shares after this operation.
    pub total_supply_shares_after: u128,
    /// `cash + total_borrows - protocol_reserves` after this operation.
    pub supplier_assets_after: u128,
    /// Total borrows after interest accrual (unchanged by a withdrawal).
    pub total_borrows_after: u128,
    /// Protocol reserves after interest accrual (unchanged by a withdrawal).
    pub protocol_reserves_after: u128,
    /// Borrow index after interest accrual (unchanged by a withdrawal).
    pub borrow_index_after: u128,
}

/// The full projected effect of a `borrow` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorrowPreview {
    /// Debt shares minted to the borrower for this operation.
    pub debt_shares_minted: u128,
    /// The borrower's total debt-share balance after this operation.
    pub user_debt_shares_after: u128,
    /// Market cash after this operation.
    pub cash_after: u128,
    /// Total borrows after this operation (including any prior accrual).
    pub total_borrows_after: u128,
    /// Total outstanding debt shares after this operation.
    pub total_debt_shares_after: u128,
    /// Protocol reserves after interest accrual (unchanged by a borrow).
    pub protocol_reserves_after: u128,
    /// Borrow index after interest accrual (unchanged by a borrow).
    pub borrow_index_after: u128,
}

/// The full projected effect of a `repay` (partial or full) operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepayPreview {
    /// Debt shares burned from the borrower for this operation.
    pub debt_shares_burned: u128,
    /// The borrower's total debt-share balance after this operation.
    pub user_debt_shares_after: u128,
    /// Assets paid in by the repayer.
    pub assets_in: u128,
    /// Market cash after this operation.
    pub cash_after: u128,
    /// Total borrows after this operation.
    pub total_borrows_after: u128,
    /// Total outstanding debt shares after this operation.
    pub total_debt_shares_after: u128,
    /// Protocol reserves after interest accrual (unchanged by a repay).
    pub protocol_reserves_after: u128,
    /// Borrow index after interest accrual (unchanged by a repay).
    pub borrow_index_after: u128,
}

/// The accounting core of a single-asset lending market.
///
/// This models `cash`, `total_borrows`, `protocol_reserves` and derives
/// `supplier_assets` from them; see the module docs for the equations.
/// Collateral, prices, health factors and liquidation are intentionally
/// out of scope — see the README.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LendingMarket {
    rate_model: InterestRateModel,
    reserve_factor: u128,

    cash: u128,
    total_borrows: u128,
    protocol_reserves: u128,
    borrow_index: u128,

    total_supply_shares: u128,
    total_debt_shares: u128,

    last_accrual_timestamp: u64,

    supply_shares: BTreeMap<AccountId, u128>,
    debt_shares: BTreeMap<AccountId, u128>,
}

impl LendingMarket {
    /// Creates a new, empty market.
    ///
    /// `reserve_factor` is a fixed-point fraction scaled by `RATE_SCALE`
    /// (must be in `[0, RATE_SCALE]`, i.e. 0%..=100%). `initial_timestamp`
    /// seeds the accrual clock; every later `_at` call must use a
    /// timestamp `>= ` the market's current `last_accrual_timestamp`.
    pub fn new(
        rate_model: InterestRateModel,
        reserve_factor: u128,
        initial_timestamp: u64,
    ) -> Result<Self, LendingError> {
        if reserve_factor > RATE_SCALE {
            return Err(LendingError::InvalidReserveFactor);
        }
        Ok(Self {
            rate_model,
            reserve_factor,
            cash: 0,
            total_borrows: 0,
            protocol_reserves: 0,
            borrow_index: INDEX_SCALE,
            total_supply_shares: 0,
            total_debt_shares: 0,
            last_accrual_timestamp: initial_timestamp,
            supply_shares: BTreeMap::new(),
            debt_shares: BTreeMap::new(),
        })
    }

    // ---- plain getters (no time dependence) -----------------------------

    /// Tokens currently held by the market.
    pub fn cash(&self) -> u128 {
        self.cash
    }

    /// Total outstanding supply shares.
    pub fn total_supply_shares(&self) -> u128 {
        self.total_supply_shares
    }

    /// Total outstanding debt shares (scaled debt units).
    pub fn total_debt_shares(&self) -> u128 {
        self.total_debt_shares
    }

    /// The configured reserve factor.
    pub fn reserve_factor(&self) -> u128 {
        self.reserve_factor
    }

    /// The configured interest-rate model.
    pub fn rate_model(&self) -> &InterestRateModel {
        &self.rate_model
    }

    /// The timestamp interest was last accrued to.
    pub fn last_accrual_timestamp(&self) -> u64 {
        self.last_accrual_timestamp
    }

    /// A user's raw supply-share balance.
    pub fn supply_shares_of(&self, user: AccountId) -> u128 {
        self.supply_shares.get(&user).copied().unwrap_or(0)
    }

    /// A user's raw debt-share balance (scaled debt units).
    pub fn debt_shares_of(&self, user: AccountId) -> u128 {
        self.debt_shares.get(&user).copied().unwrap_or(0)
    }

    /// Iterates over every account holding a nonzero supply-share balance.
    /// Intended for tests that check aggregate invariants; not needed for
    /// normal market operation, which never iterates accounts.
    pub fn supply_share_accounts(&self) -> impl Iterator<Item = (&AccountId, &u128)> {
        self.supply_shares.iter()
    }

    /// Iterates over every account holding a nonzero debt-share balance.
    /// Intended for tests that check aggregate invariants; not needed for
    /// normal market operation, which never iterates accounts.
    pub fn debt_share_accounts(&self) -> impl Iterator<Item = (&AccountId, &u128)> {
        self.debt_shares.iter()
    }

    // ---- pure accrual simulation -----------------------------------------

    /// Simulates accruing interest from `last_accrual_timestamp` to
    /// `timestamp`, without mutating `self`. Returns the state as it
    /// *would* be after accrual.
    ///
    /// This is the single source of truth for interest math: every
    /// mutating operation and every `_at` query goes through this
    /// function, so the timing semantics are identical everywhere.
    ///
    /// - Backwards timestamps fail atomically with
    ///   [`LendingError::BackwardsTimestamp`] and touch nothing.
    /// - `timestamp == last_accrual_timestamp` (a same-instant checkpoint)
    ///   and `total_borrows == 0` (nothing to accrue interest on) are both
    ///   no-ops that return the current state unchanged. This is why
    ///   repeated, arbitrary read-only checkpoints never change the
    ///   economic outcome: reads never advance `last_accrual_timestamp`,
    ///   only the mutating `*_at` methods do (see
    ///   [`LendingMarket::apply_accrual`]), and re-observing the same
    ///   instant is idempotent by construction.
    fn simulate_accrual(&self, timestamp: u64) -> Result<AccrualSnapshot, LendingError> {
        if timestamp < self.last_accrual_timestamp {
            return Err(LendingError::BackwardsTimestamp);
        }

        let unchanged = AccrualSnapshot {
            total_borrows: self.total_borrows,
            protocol_reserves: self.protocol_reserves,
            borrow_index: self.borrow_index,
        };

        if timestamp == self.last_accrual_timestamp || self.total_borrows == 0 {
            return Ok(unchanged);
        }

        let elapsed = timestamp - self.last_accrual_timestamp;
        let utilization = utilization_raw(self.cash, self.total_borrows)?;
        let rate = self.rate_model.borrow_rate(utilization)?;

        // interest = opening_borrows * rate * elapsed / (RATE_SCALE * SECONDS_PER_YEAR)
        let denom = RATE_SCALE
            .checked_mul(SECONDS_PER_YEAR as u128)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let numerator = self
            .total_borrows
            .checked_mul(rate)
            .ok_or(LendingError::ArithmeticOverflow)?
            .checked_mul(elapsed as u128)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let interest = numerator / denom;

        if interest == 0 {
            return Ok(unchanged);
        }

        // The reserve factor applies only to newly accrued interest; the
        // floor here means any sub-unit remainder from the split is never
        // taken by the protocol, so it is implicitly owned by suppliers
        // (total_borrows still grows by the full `interest`, while
        // protocol_reserves grows by the floored share).
        let reserve_increment = mul_div_floor(interest, self.reserve_factor, RATE_SCALE)?;

        let total_borrows = self
            .total_borrows
            .checked_add(interest)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let protocol_reserves = self
            .protocol_reserves
            .checked_add(reserve_increment)
            .ok_or(LendingError::ArithmeticOverflow)?;

        // borrow_index grows by the same ratio total_borrows grew by, so
        // that `debt_shares * borrow_index / INDEX_SCALE` tracks
        // `total_borrows` for every scaled balance without iterating
        // accounts (self.total_borrows != 0 here, see the early return
        // above).
        let borrow_index = self
            .borrow_index
            .checked_mul(total_borrows)
            .ok_or(LendingError::ArithmeticOverflow)?
            / self.total_borrows;

        Ok(AccrualSnapshot {
            total_borrows,
            protocol_reserves,
            borrow_index,
        })
    }

    /// Commits a previously computed [`AccrualSnapshot`] and advances the
    /// accrual checkpoint to `timestamp`. Infallible by construction: all
    /// fallible work happens in [`LendingMarket::simulate_accrual`].
    fn apply_accrual(&mut self, snap: AccrualSnapshot, timestamp: u64) {
        self.total_borrows = snap.total_borrows;
        self.protocol_reserves = snap.protocol_reserves;
        self.borrow_index = snap.borrow_index;
        self.last_accrual_timestamp = timestamp;
    }

    fn supplier_assets_from(cash: u128, snap: &AccrualSnapshot) -> Result<u128, LendingError> {
        cash.checked_add(snap.total_borrows)
            .and_then(|v| v.checked_sub(snap.protocol_reserves))
            .ok_or(LendingError::ArithmeticOverflow)
    }

    // ---- time-aware queries -----------------------------------------------

    /// Total borrows (principal plus accrued interest) as of `timestamp`.
    pub fn total_borrows_at(&self, timestamp: u64) -> Result<u128, LendingError> {
        Ok(self.simulate_accrual(timestamp)?.total_borrows)
    }

    /// Protocol reserves as of `timestamp`.
    pub fn protocol_reserves_at(&self, timestamp: u64) -> Result<u128, LendingError> {
        Ok(self.simulate_accrual(timestamp)?.protocol_reserves)
    }

    /// `cash + total_borrows - protocol_reserves` as of `timestamp`.
    pub fn supplier_assets_at(&self, timestamp: u64) -> Result<u128, LendingError> {
        let snap = self.simulate_accrual(timestamp)?;
        Self::supplier_assets_from(self.cash, &snap)
    }

    /// `total_borrows / (cash + total_borrows)` as of `timestamp`, scaled
    /// by `RATE_SCALE`. See the README for why this denominator (and not
    /// one that subtracts reserves) was chosen.
    pub fn utilization_at(&self, timestamp: u64) -> Result<u128, LendingError> {
        let snap = self.simulate_accrual(timestamp)?;
        utilization_raw(self.cash, snap.total_borrows)
    }

    /// The instantaneous borrow APR implied by `utilization_at(timestamp)`.
    pub fn borrow_rate_at(&self, timestamp: u64) -> Result<u128, LendingError> {
        self.rate_model.borrow_rate(self.utilization_at(timestamp)?)
    }

    /// The cumulative borrow index (scaled by `INDEX_SCALE`) as of
    /// `timestamp`. Monotonically non-decreasing in `timestamp`.
    pub fn borrow_index_at(&self, timestamp: u64) -> Result<u128, LendingError> {
        Ok(self.simulate_accrual(timestamp)?.borrow_index)
    }

    /// The supply exchange rate (`supplier_assets / total_supply_shares`)
    /// as of `timestamp`, scaled by `INDEX_SCALE`. On an empty market
    /// (`total_supply_shares == 0`) this returns `INDEX_SCALE` (1.0), the
    /// rate the very first supplier will mint at.
    pub fn supply_exchange_rate_at(&self, timestamp: u64) -> Result<u128, LendingError> {
        if self.total_supply_shares == 0 {
            return Ok(INDEX_SCALE);
        }
        let assets = self.supplier_assets_at(timestamp)?;
        mul_div_floor(assets, INDEX_SCALE, self.total_supply_shares)
    }

    /// A supplier's proportional claim on `supplier_assets_at(timestamp)`,
    /// floored. This is the account's accounting entitlement — it may
    /// exceed [`LendingMarket::cash`] if the market's cash is currently
    /// lent out; see the README's "Solvency vs. liquidity" section.
    pub fn claimable_assets_at(
        &self,
        user: AccountId,
        timestamp: u64,
    ) -> Result<u128, LendingError> {
        let shares = self.supply_shares_of(user);
        if shares == 0 || self.total_supply_shares == 0 {
            return Ok(0);
        }
        let assets = self.supplier_assets_at(timestamp)?;
        mul_div_floor(shares, assets, self.total_supply_shares)
    }

    /// A borrower's current debt (`debt_shares * borrow_index /
    /// INDEX_SCALE`, ceiled) as of `timestamp`.
    pub fn debt_of_at(&self, user: AccountId, timestamp: u64) -> Result<u128, LendingError> {
        let shares = self.debt_shares_of(user);
        if shares == 0 {
            return Ok(0);
        }
        let snap = self.simulate_accrual(timestamp)?;
        mul_div_ceil(shares, snap.borrow_index, INDEX_SCALE)
    }

    // ---- planning (pure) ---------------------------------------------------
    //
    // Each `plan_*` function is `&self`-only and either returns the
    // *complete* next state (as an `AccrualSnapshot` plus a `*Preview`) or
    // an `Err`, touching nothing. The corresponding mutating method and
    // the corresponding `preview_*` method both call the same `plan_*`
    // function, which is what guarantees preview/execution agreement and
    // atomic failure.

    fn plan_supply(
        &self,
        timestamp: u64,
        user: AccountId,
        amount: u128,
    ) -> Result<(AccrualSnapshot, SupplyPreview), LendingError> {
        if amount == 0 {
            return Err(LendingError::ZeroAmount);
        }
        let snap = self.simulate_accrual(timestamp)?;
        let supplier_assets_before = Self::supplier_assets_from(self.cash, &snap)?;

        let shares_minted = if self.total_supply_shares == 0 {
            // Initial supply mints one-for-one.
            amount
        } else {
            if supplier_assets_before == 0 {
                return Err(LendingError::EmptyMarket);
            }
            mul_div_floor(amount, self.total_supply_shares, supplier_assets_before)?
        };
        if shares_minted == 0 {
            return Err(LendingError::RoundsToZero);
        }

        let cash_after = self
            .cash
            .checked_add(amount)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let total_supply_shares_after = self
            .total_supply_shares
            .checked_add(shares_minted)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let user_shares_after = self
            .supply_shares_of(user)
            .checked_add(shares_minted)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let supplier_assets_after = cash_after
            .checked_add(snap.total_borrows)
            .and_then(|v| v.checked_sub(snap.protocol_reserves))
            .ok_or(LendingError::ArithmeticOverflow)?;

        Ok((
            snap,
            SupplyPreview {
                shares_minted,
                user_shares_after,
                cash_after,
                total_supply_shares_after,
                supplier_assets_after,
                total_borrows_after: snap.total_borrows,
                protocol_reserves_after: snap.protocol_reserves,
                borrow_index_after: snap.borrow_index,
            },
        ))
    }

    fn plan_withdraw(
        &self,
        timestamp: u64,
        user: AccountId,
        amount: u128,
    ) -> Result<(AccrualSnapshot, WithdrawPreview), LendingError> {
        if amount == 0 {
            return Err(LendingError::ZeroAmount);
        }
        let snap = self.simulate_accrual(timestamp)?;
        if self.total_supply_shares == 0 {
            return Err(LendingError::InsufficientSupplyShares);
        }
        let supplier_assets_before = Self::supplier_assets_from(self.cash, &snap)?;
        if supplier_assets_before == 0 {
            return Err(LendingError::EmptyMarket);
        }

        // Burn shares upward: never lets a withdrawer take out more value
        // than the shares they surrender, at the expense of the remaining
        // suppliers.
        let shares_to_burn =
            mul_div_ceil(amount, self.total_supply_shares, supplier_assets_before)?;
        if shares_to_burn == 0 {
            return Err(LendingError::RoundsToZero);
        }

        let user_shares = self.supply_shares_of(user);
        if shares_to_burn > user_shares {
            return Err(LendingError::InsufficientSupplyShares);
        }
        // Available-liquidity restriction: a valid accounting claim can
        // still exceed what is currently withdrawable. See the README.
        if amount > self.cash {
            return Err(LendingError::InsufficientCash);
        }

        let cash_after = self.cash - amount;
        let total_supply_shares_after = self.total_supply_shares - shares_to_burn;
        let user_shares_after = user_shares - shares_to_burn;
        let supplier_assets_after = cash_after
            .checked_add(snap.total_borrows)
            .and_then(|v| v.checked_sub(snap.protocol_reserves))
            .ok_or(LendingError::ArithmeticOverflow)?;

        Ok((
            snap,
            WithdrawPreview {
                shares_burned: shares_to_burn,
                user_shares_after,
                assets_out: amount,
                cash_after,
                total_supply_shares_after,
                supplier_assets_after,
                total_borrows_after: snap.total_borrows,
                protocol_reserves_after: snap.protocol_reserves,
                borrow_index_after: snap.borrow_index,
            },
        ))
    }

    fn plan_borrow(
        &self,
        timestamp: u64,
        user: AccountId,
        amount: u128,
    ) -> Result<(AccrualSnapshot, BorrowPreview), LendingError> {
        if amount == 0 {
            return Err(LendingError::ZeroAmount);
        }
        let snap = self.simulate_accrual(timestamp)?;
        // Available-liquidity restriction: a borrower can never draw out
        // more cash than the market currently holds.
        if amount > self.cash {
            return Err(LendingError::InsufficientCash);
        }

        // Record debt shares upward: the borrower's scaled balance always
        // covers at least the assets actually disbursed.
        let debt_shares_minted = mul_div_ceil(amount, INDEX_SCALE, snap.borrow_index)?;
        if debt_shares_minted == 0 {
            return Err(LendingError::RoundsToZero);
        }

        let cash_after = self.cash - amount;
        let total_borrows_after = snap
            .total_borrows
            .checked_add(amount)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let total_debt_shares_after = self
            .total_debt_shares
            .checked_add(debt_shares_minted)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let user_debt_shares_after = self
            .debt_shares_of(user)
            .checked_add(debt_shares_minted)
            .ok_or(LendingError::ArithmeticOverflow)?;

        Ok((
            snap,
            BorrowPreview {
                debt_shares_minted,
                user_debt_shares_after,
                cash_after,
                total_borrows_after,
                total_debt_shares_after,
                protocol_reserves_after: snap.protocol_reserves,
                borrow_index_after: snap.borrow_index,
            },
        ))
    }

    fn plan_repay(
        &self,
        timestamp: u64,
        user: AccountId,
        amount: u128,
    ) -> Result<(AccrualSnapshot, RepayPreview), LendingError> {
        if amount == 0 {
            return Err(LendingError::ZeroAmount);
        }
        let snap = self.simulate_accrual(timestamp)?;
        let user_debt_shares = self.debt_shares_of(user);
        if user_debt_shares == 0 {
            return Err(LendingError::InsufficientDebt);
        }

        // Burn shares downward: a partial repayment never erases more
        // debt (in share terms) than the assets actually paid cover.
        let shares_to_burn = mul_div_floor(amount, INDEX_SCALE, snap.borrow_index)?;
        if shares_to_burn == 0 {
            return Err(LendingError::RoundsToZero);
        }
        if shares_to_burn > user_debt_shares {
            // Repaying more than is owed via a partial repayment is
            // rejected; use `repay_all_at` to close out a position.
            return Err(LendingError::InsufficientDebt);
        }

        let cash_after = self
            .cash
            .checked_add(amount)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let total_borrows_after = snap
            .total_borrows
            .checked_sub(amount)
            .ok_or(LendingError::ArithmeticOverflow)?;
        let total_debt_shares_after = self.total_debt_shares - shares_to_burn;
        let user_debt_shares_after = user_debt_shares - shares_to_burn;

        Ok((
            snap,
            RepayPreview {
                debt_shares_burned: shares_to_burn,
                user_debt_shares_after,
                assets_in: amount,
                cash_after,
                total_borrows_after,
                total_debt_shares_after,
                protocol_reserves_after: snap.protocol_reserves,
                borrow_index_after: snap.borrow_index,
            },
        ))
    }

    fn plan_repay_all(
        &self,
        timestamp: u64,
        user: AccountId,
    ) -> Result<(AccrualSnapshot, RepayPreview), LendingError> {
        let snap = self.simulate_accrual(timestamp)?;
        let user_debt_shares = self.debt_shares_of(user);
        if user_debt_shares == 0 {
            return Err(LendingError::InsufficientDebt);
        }

        // Ceil so the borrower always pays enough to fully clear their
        // scaled balance, leaving no debt-share dust on their account.
        let amount_owed = mul_div_ceil(user_debt_shares, snap.borrow_index, INDEX_SCALE)?;

        let cash_after = self
            .cash
            .checked_add(amount_owed)
            .ok_or(LendingError::ArithmeticOverflow)?;
        // The ceil above can, in a market with multiple borrowers whose
        // individual balances already carry independent rounding, push
        // `amount_owed` up to at most 1 unit past this borrower's exact
        // share of `total_borrows`. total_borrows cannot go negative, so
        // any such 1-unit remainder is floored away here; its owner is
        // the remaining suppliers, since it stays in `cash` without a
        // matching `total_borrows` or `protocol_reserves` entry, i.e. it
        // becomes an (at most 1-unit) increase in `supplier_assets`.
        let total_borrows_after = snap.total_borrows.saturating_sub(amount_owed);
        let total_debt_shares_after = self.total_debt_shares - user_debt_shares;

        Ok((
            snap,
            RepayPreview {
                debt_shares_burned: user_debt_shares,
                user_debt_shares_after: 0,
                assets_in: amount_owed,
                cash_after,
                total_borrows_after,
                total_debt_shares_after,
                protocol_reserves_after: snap.protocol_reserves,
                borrow_index_after: snap.borrow_index,
            },
        ))
    }

    // ---- previews (pure) ----------------------------------------------------

    /// Pure preview of [`LendingMarket::supply_at`]. Never mutates
    /// `self`, and is guaranteed to agree with the mutating call given the
    /// same arguments (both are backed by the same planning function).
    pub fn preview_supply_at(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<SupplyPreview, LendingError> {
        self.plan_supply(timestamp, user, amount).map(|(_, p)| p)
    }

    /// Pure preview of [`LendingMarket::withdraw_at`].
    pub fn preview_withdraw_at(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<WithdrawPreview, LendingError> {
        self.plan_withdraw(timestamp, user, amount).map(|(_, p)| p)
    }

    /// Pure preview of [`LendingMarket::borrow_at`].
    pub fn preview_borrow_at(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<BorrowPreview, LendingError> {
        self.plan_borrow(timestamp, user, amount).map(|(_, p)| p)
    }

    /// Pure preview of [`LendingMarket::repay_at`].
    pub fn preview_repay_at(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<RepayPreview, LendingError> {
        self.plan_repay(timestamp, user, amount).map(|(_, p)| p)
    }

    /// Pure preview of [`LendingMarket::repay_all_at`].
    pub fn preview_repay_all_at(
        &self,
        user: AccountId,
        timestamp: u64,
    ) -> Result<RepayPreview, LendingError> {
        self.plan_repay_all(timestamp, user).map(|(_, p)| p)
    }

    // ---- mutating state transitions -----------------------------------------

    /// Supplies `amount` of the underlying asset for `user`, minting
    /// supply shares at the current exchange rate. Accrues interest up to
    /// `timestamp` first.
    pub fn supply_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<SupplyPreview, LendingError> {
        let (snap, preview) = self.plan_supply(timestamp, user, amount)?;
        self.apply_accrual(snap, timestamp);
        self.cash = preview.cash_after;
        self.total_supply_shares = preview.total_supply_shares_after;
        set_balance(&mut self.supply_shares, user, preview.user_shares_after);
        Ok(preview)
    }

    /// Withdraws `amount` of the underlying asset for `user`, burning
    /// supply shares at the current exchange rate. Accrues interest up to
    /// `timestamp` first. Fails if `amount` exceeds either the user's
    /// claim or the market's available cash.
    pub fn withdraw_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<WithdrawPreview, LendingError> {
        let (snap, preview) = self.plan_withdraw(timestamp, user, amount)?;
        self.apply_accrual(snap, timestamp);
        self.cash = preview.cash_after;
        self.total_supply_shares = preview.total_supply_shares_after;
        set_balance(&mut self.supply_shares, user, preview.user_shares_after);
        Ok(preview)
    }

    /// Borrows `amount` of the underlying asset for `user`, minting debt
    /// shares against the current borrow index. Accrues interest up to
    /// `timestamp` first. Fails if `amount` exceeds the market's
    /// available cash.
    pub fn borrow_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<BorrowPreview, LendingError> {
        let (snap, preview) = self.plan_borrow(timestamp, user, amount)?;
        self.apply_accrual(snap, timestamp);
        self.cash = preview.cash_after;
        self.total_borrows = preview.total_borrows_after;
        self.total_debt_shares = preview.total_debt_shares_after;
        set_balance(&mut self.debt_shares, user, preview.user_debt_shares_after);
        Ok(preview)
    }

    /// Repays `amount` of `user`'s debt. Accrues interest up to
    /// `timestamp` first. Rejects repaying more than is currently owed;
    /// use [`LendingMarket::repay_all_at`] to close a position exactly.
    pub fn repay_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<RepayPreview, LendingError> {
        let (snap, preview) = self.plan_repay(timestamp, user, amount)?;
        self.apply_accrual(snap, timestamp);
        self.cash = preview.cash_after;
        self.total_borrows = preview.total_borrows_after;
        self.total_debt_shares = preview.total_debt_shares_after;
        set_balance(&mut self.debt_shares, user, preview.user_debt_shares_after);
        Ok(preview)
    }

    /// Repays the entirety of `user`'s debt, computing the exact amount
    /// owed (ceiled) and burning their full debt-share balance so the
    /// position closes cleanly with no dust. Accrues interest up to
    /// `timestamp` first.
    pub fn repay_all_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
    ) -> Result<RepayPreview, LendingError> {
        let (snap, preview) = self.plan_repay_all(timestamp, user)?;
        self.apply_accrual(snap, timestamp);
        self.cash = preview.cash_after;
        self.total_borrows = preview.total_borrows_after;
        self.total_debt_shares = preview.total_debt_shares_after;
        set_balance(&mut self.debt_shares, user, preview.user_debt_shares_after);
        Ok(preview)
    }
}

fn set_balance(map: &mut BTreeMap<AccountId, u128>, user: AccountId, balance: u128) {
    if balance == 0 {
        map.remove(&user);
    } else {
        map.insert(user, balance);
    }
}
