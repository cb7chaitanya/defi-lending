//! Day 12: collateral valuation, borrowing capacity, liquidation
//! thresholds, and health factors, wrapped around the Day 11
//! [`LendingMarket`] accounting core.
//!
//! [`RiskMarket`] owns a [`LendingMarket`] and adds per-user, per-asset
//! collateral balances beside it. It never duplicates the market's own
//! accounting: interest accrual, the borrow index, debt shares, supply
//! shares, cash/total-borrow bookkeeping, protocol reserves, and the
//! kinked rate model are all still handled entirely by [`LendingMarket`]
//! exactly as in Day 11. A [`RiskMarket`] borrow computes the market's
//! *complete* proposed post-borrow state via
//! [`LendingMarket::preview_borrow_at`], checks it against the account's
//! collateral, and only then commits the identical operation through
//! [`LendingMarket::borrow_at`] — so a successful collateral-aware borrow
//! produces exactly the market-accounting result the plain Day 11 borrow
//! path would have, plus the (separate) collateral bookkeeping this
//! module owns.
//!
//! See the README's Day 12 section for the full model, every rounding
//! rule, and worked examples. In short:
//!
//! ```text
//! collateral_value_i           = collateral_balance_i * price_i / quantity_scale_i   (round down)
//! max_borrowing_capacity        = sum(collateral_value_i * max_ltv_i)                 (round down per term)
//! liquidation_adjusted_collateral = sum(collateral_value_i * liquidation_threshold_i)  (round down per term)
//! total_debt_value              = debt_amount * debt_price / debt_quantity_scale       (round up)
//! current_ltv                   = total_debt_value / total_collateral_value            (round up)
//! health_factor                 = liquidation_adjusted_collateral / total_debt_value   (round down)
//! ```

use std::collections::BTreeMap;

use crate::collateral::{CollateralConfig, DebtAssetConfig};
use crate::error::LendingError;
use crate::market::{
    AccountId, BorrowPreview, LendingMarket, RepayPreview, SupplyPreview, WithdrawPreview,
};
use crate::math::{INDEX_SCALE, RATE_SCALE, mul_div_ceil, mul_div_floor};
use crate::oracle::{AssetId, PriceBook};

/// An account's health, expressed without floating-point infinity.
///
/// `HF > 1` (`Ratio(hf)` with `hf > RATE_SCALE`) is safe. `HF == 1`
/// (`hf == RATE_SCALE`) is the exact liquidation boundary and is treated
/// as **safe, not liquidatable** — consistent throughout this crate's
/// code, tests, and README: liquidation requires the health factor to
/// have dropped *strictly below* 1. `HF < 1` is liquidatable.
/// `NoDebt` means the account has no outstanding debt and therefore
/// cannot be liquidated regardless of its collateral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthFactor {
    /// No outstanding debt: never liquidatable.
    NoDebt,
    /// A finite health-factor ratio scaled by [`RATE_SCALE`]
    /// (`RATE_SCALE` means `HF == 1.0`).
    Ratio(u128),
}

impl HealthFactor {
    /// Whether this health factor represents a liquidatable account:
    /// `Ratio(hf)` with `hf < RATE_SCALE`. `NoDebt` and `Ratio(hf)` with
    /// `hf >= RATE_SCALE` are both `false`.
    pub fn is_liquidatable(&self) -> bool {
        match self {
            HealthFactor::NoDebt => false,
            HealthFactor::Ratio(hf) => *hf < RATE_SCALE,
        }
    }

    /// The underlying `RATE_SCALE`-scaled ratio, if any (`None` for
    /// `NoDebt`).
    pub fn ratio(&self) -> Option<u128> {
        match self {
            HealthFactor::NoDebt => None,
            HealthFactor::Ratio(hf) => Some(*hf),
        }
    }
}

/// The projected effect of a collateral deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollateralDepositPreview {
    /// The asset deposited.
    pub asset: AssetId,
    /// The amount deposited (raw smallest-unit amount).
    pub amount_deposited: u128,
    /// The depositor's balance of `asset` after this operation.
    pub user_balance_after: u128,
}

/// The projected effect of a collateral withdrawal, including the full
/// post-withdrawal risk snapshot that was checked before allowing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollateralWithdrawPreview {
    /// The asset withdrawn.
    pub asset: AssetId,
    /// The amount withdrawn (raw smallest-unit amount).
    pub amount_withdrawn: u128,
    /// The withdrawer's balance of `asset` after this operation.
    pub user_balance_after: u128,
    /// Total collateral value (all assets) after this withdrawal.
    pub total_collateral_value_after: u128,
    /// Maximum borrowing capacity after this withdrawal.
    pub max_borrowing_capacity_after: u128,
    /// Liquidation-adjusted collateral after this withdrawal.
    pub liquidation_adjusted_collateral_after: u128,
    /// Total debt value (unaffected by a collateral withdrawal, but
    /// included so the whole risk picture is visible in one struct).
    pub total_debt_value: u128,
    /// The account's health factor after this withdrawal.
    pub health_factor_after: HealthFactor,
}

/// The projected effect of a risk-checked borrow: the underlying
/// [`LendingMarket`]'s own [`BorrowPreview`], plus the risk figures that
/// were checked before allowing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskBorrowPreview {
    /// The exact result the underlying market's own `borrow_at` produces.
    pub market: BorrowPreview,
    /// Total debt value, in the common quote unit, after this borrow.
    pub total_debt_value_after: u128,
    /// Maximum borrowing capacity (unaffected by borrowing, since
    /// collateral doesn't change) at the time of this borrow.
    pub max_borrowing_capacity: u128,
    /// The account's health factor after this borrow.
    pub health_factor_after: HealthFactor,
}

/// The three aggregate collateral figures every valuation query is built
/// from, computed together in one pass so per-asset prices are fetched
/// and rounded exactly once.
struct CollateralTotals {
    total_value: u128,
    max_borrowing_capacity: u128,
    liquidation_adjusted_collateral: u128,
}

/// Collateral valuation, borrowing capacity, and health-factor accounting
/// wrapped around a Day 11 [`LendingMarket`].
///
/// State is intentionally split in two: the wrapped [`LendingMarket`]
/// remains the sole source of truth for cash, borrows, reserves, supply
/// shares, and debt shares (exactly as in Day 11); [`RiskMarket`] adds
/// only per-user, per-asset collateral balances and per-asset risk
/// configuration beside it. A user's debt is never duplicated or
/// independently tracked here — every debt figure this module computes
/// is derived by calling into the market's own `debt_of_at` /
/// `preview_borrow_at`, which read (or project) the market's real
/// debt-share balance against its real borrow index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskMarket {
    market: LendingMarket,
    debt_asset: DebtAssetConfig,
    collateral_configs: BTreeMap<AssetId, CollateralConfig>,
    collateral_balances: BTreeMap<AccountId, BTreeMap<AssetId, u128>>,
}

impl RiskMarket {
    /// Wraps an existing [`LendingMarket`] with Day 12 collateral/risk
    /// accounting. The market can be pre-seeded with liquidity (via
    /// [`LendingMarket::supply_at`]) before or after wrapping; wrapping
    /// itself does not touch the market's state.
    pub fn new(market: LendingMarket, debt_asset: DebtAssetConfig) -> Self {
        Self {
            market,
            debt_asset,
            collateral_configs: BTreeMap::new(),
            collateral_balances: BTreeMap::new(),
        }
    }

    /// Read-only access to the wrapped Day 11 market — every existing
    /// `LendingMarket` query (`cash`, `total_borrows_at`, `debt_of_at`,
    /// `supply_exchange_rate_at`, ...) remains available unchanged.
    pub fn market(&self) -> &LendingMarket {
        &self.market
    }

    /// The market's configured debt-asset valuation parameters.
    pub fn debt_asset(&self) -> DebtAssetConfig {
        self.debt_asset
    }

    /// Registers a new collateral asset. Rejects a duplicate
    /// [`AssetId`] with [`LendingError::DuplicateCollateralAsset`] rather
    /// than silently overwriting an existing configuration (which could
    /// otherwise change the risk parameters backing an account's existing
    /// debt without their action).
    pub fn configure_collateral(&mut self, config: CollateralConfig) -> Result<(), LendingError> {
        if self.collateral_configs.contains_key(&config.asset()) {
            return Err(LendingError::DuplicateCollateralAsset);
        }
        self.collateral_configs.insert(config.asset(), config);
        Ok(())
    }

    /// The configuration for `asset`, if it has been registered as
    /// collateral.
    pub fn collateral_config(&self, asset: AssetId) -> Option<&CollateralConfig> {
        self.collateral_configs.get(&asset)
    }

    /// `user`'s raw balance of `asset` as collateral (`0` if none).
    pub fn collateral_balance_of(&self, user: AccountId, asset: AssetId) -> u128 {
        self.collateral_balances
            .get(&user)
            .and_then(|balances| balances.get(&asset))
            .copied()
            .unwrap_or(0)
    }

    /// Every asset `user` currently holds a nonzero collateral balance
    /// of. Intended for tests and inspection; normal valuation never
    /// needs to iterate accounts.
    pub fn collateral_balances_of(
        &self,
        user: AccountId,
    ) -> impl Iterator<Item = (&AssetId, &u128)> {
        self.collateral_balances
            .get(&user)
            .into_iter()
            .flat_map(|balances| balances.iter())
    }

    // ---- valuation (pure, read-only) --------------------------------------

    /// The value of one collateral position, in the common quote unit
    /// (scaled by [`crate::oracle::PRICE_SCALE`]), rounded **down** —
    /// never overstate collateral. Returns `Ok(0)` *without* requiring a
    /// price if the account holds none of `asset`: an oracle failure for
    /// an asset a user doesn't hold must never block them.
    pub fn collateral_value_at(
        &self,
        user: AccountId,
        asset: AssetId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<u128, LendingError> {
        let balance = self.collateral_balance_of(user, asset);
        if balance == 0 {
            return Ok(0);
        }
        let config = self
            .collateral_configs
            .get(&asset)
            .ok_or(LendingError::UnknownCollateralAsset)?;
        let price = prices.price_at(asset, timestamp)?;
        mul_div_floor(balance, price, config.quantity_scale())
    }

    /// Computes `total_value`, `max_borrowing_capacity`, and
    /// `liquidation_adjusted_collateral` together over `balances`
    /// (either the account's real holdings, or a hypothetical set with
    /// one balance substituted — see
    /// [`RiskMarket::collateral_totals_with_override_at`]).
    fn collateral_totals_over(
        &self,
        balances: &BTreeMap<AssetId, u128>,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<CollateralTotals, LendingError> {
        let mut total_value: u128 = 0;
        let mut max_borrowing_capacity: u128 = 0;
        let mut liquidation_adjusted_collateral: u128 = 0;

        for (&asset, &balance) in balances.iter() {
            if balance == 0 {
                continue;
            }
            let config = self
                .collateral_configs
                .get(&asset)
                .ok_or(LendingError::UnknownCollateralAsset)?;
            let price = prices.price_at(asset, timestamp)?;
            // Collateral value rounds down: never overstate what backs a
            // loan. Each per-asset max-LTV and liquidation-threshold term
            // is floored too, before summing, so the rounding loss is
            // bounded per-asset rather than compounding across the sum.
            let value = mul_div_floor(balance, price, config.quantity_scale())?;
            let capacity_term = mul_div_floor(value, config.max_ltv(), RATE_SCALE)?;
            let liquidation_term =
                mul_div_floor(value, config.liquidation_threshold(), RATE_SCALE)?;

            total_value = total_value
                .checked_add(value)
                .ok_or(LendingError::ArithmeticOverflow)?;
            max_borrowing_capacity = max_borrowing_capacity
                .checked_add(capacity_term)
                .ok_or(LendingError::ArithmeticOverflow)?;
            liquidation_adjusted_collateral = liquidation_adjusted_collateral
                .checked_add(liquidation_term)
                .ok_or(LendingError::ArithmeticOverflow)?;
        }

        Ok(CollateralTotals {
            total_value,
            max_borrowing_capacity,
            liquidation_adjusted_collateral,
        })
    }

    /// [`RiskMarket::collateral_totals_over`] against `user`'s real
    /// balances, with one balance optionally substituted — used to price
    /// a *hypothetical* post-withdrawal (or post-deposit) account without
    /// ever mutating `self`, so a proposed operation's complete next
    /// state can be validated before it is committed.
    fn collateral_totals_with_override_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
        override_balance: Option<(AssetId, u128)>,
    ) -> Result<CollateralTotals, LendingError> {
        let mut balances = self
            .collateral_balances
            .get(&user)
            .cloned()
            .unwrap_or_default();
        if let Some((asset, balance)) = override_balance {
            if balance == 0 {
                balances.remove(&asset);
            } else {
                balances.insert(asset, balance);
            }
        }
        self.collateral_totals_over(&balances, timestamp, prices)
    }

    fn collateral_totals_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<CollateralTotals, LendingError> {
        self.collateral_totals_with_override_at(user, timestamp, prices, None)
    }

    /// Total collateral value across every asset `user` holds, rounded
    /// down per asset. See [`RiskMarket::collateral_value_at`] for the
    /// per-position rounding rule this sums.
    pub fn total_collateral_value_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<u128, LendingError> {
        Ok(self
            .collateral_totals_at(user, timestamp, prices)?
            .total_value)
    }

    /// `sum(collateral_value_i * max_ltv_i)`, each term rounded down —
    /// never overstate how much an account can borrow.
    pub fn max_borrowing_capacity_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<u128, LendingError> {
        Ok(self
            .collateral_totals_at(user, timestamp, prices)?
            .max_borrowing_capacity)
    }

    /// `sum(collateral_value_i * liquidation_threshold_i)`, each term
    /// rounded down — never overstate the safety cushion backing an
    /// account's debt.
    pub fn liquidation_adjusted_collateral_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<u128, LendingError> {
        Ok(self
            .collateral_totals_at(user, timestamp, prices)?
            .liquidation_adjusted_collateral)
    }

    /// `user`'s total debt value in the common quote unit, rounded
    /// **up** — never understate debt. The underlying debt amount comes
    /// from [`LendingMarket::debt_of_at`] (itself already ceil-rounded,
    /// per Day 11's rounding policy), so both the debt-share-to-asset
    /// conversion and the asset-to-quote-unit conversion round
    /// conservatively in the same direction. Returns `Ok(0)` *without*
    /// requiring a debt-asset price if the account has no debt.
    pub fn total_debt_value_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<u128, LendingError> {
        let debt_amount = self.market.debt_of_at(user, timestamp)?;
        if debt_amount == 0 {
            return Ok(0);
        }
        let price = prices.price_at(self.debt_asset.asset(), timestamp)?;
        mul_div_ceil(debt_amount, price, self.debt_asset.quantity_scale())
    }

    /// `total_debt_value / total_collateral_value`, scaled by
    /// [`RATE_SCALE`], rounded **up** (consistent with this crate's
    /// "never understate risk" convention: current LTV is a risk-exposure
    /// figure, so its rounding direction matches the debt side, not the
    /// collateral side). `Ok(0)` if there is no debt.
    ///
    /// Note this display figure is independent of the raw
    /// cross-multiplied comparisons [`RiskMarket::plan_borrow`] and
    /// [`RiskMarket::plan_withdraw_collateral`] actually enforce — it is
    /// informational only.
    pub fn current_ltv_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<u128, LendingError> {
        let debt_value = self.total_debt_value_at(user, timestamp, prices)?;
        if debt_value == 0 {
            return Ok(0);
        }
        let collateral_value = self.total_collateral_value_at(user, timestamp, prices)?;
        if collateral_value == 0 {
            // Debt with no collateral backing it at all: this can't arise
            // from this module's own operations (borrowing requires
            // max_borrowing_capacity >= debt value, which requires
            // nonzero collateral), so this is a defensive, not a normal,
            // path.
            return Err(LendingError::EmptyMarket);
        }
        mul_div_ceil(debt_value, RATE_SCALE, collateral_value)
    }

    /// `liquidation_adjusted_collateral / total_debt_value`, scaled by
    /// [`RATE_SCALE`], rounded **down** (both the numerator and this
    /// final division round conservatively, so the reported health
    /// factor never overstates safety). [`HealthFactor::NoDebt`] if the
    /// account has no debt.
    pub fn health_factor_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<HealthFactor, LendingError> {
        let debt_value = self.total_debt_value_at(user, timestamp, prices)?;
        let liquidation_adjusted =
            self.liquidation_adjusted_collateral_at(user, timestamp, prices)?;
        health_factor_from(liquidation_adjusted, debt_value)
    }

    /// `max(0, max_borrowing_capacity - total_debt_value)`, saturating at
    /// zero logically (via `saturating_sub`) rather than through
    /// unchecked subtraction.
    pub fn additional_borrowing_capacity_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<u128, LendingError> {
        let capacity = self.max_borrowing_capacity_at(user, timestamp, prices)?;
        let debt_value = self.total_debt_value_at(user, timestamp, prices)?;
        Ok(capacity.saturating_sub(debt_value))
    }

    /// Whether `user`'s account is currently liquidatable
    /// (`health_factor_at(...).is_liquidatable()`).
    pub fn is_liquidatable_at(
        &self,
        user: AccountId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<bool, LendingError> {
        Ok(self
            .health_factor_at(user, timestamp, prices)?
            .is_liquidatable())
    }

    /// The price of `target_asset` (one of `user`'s currently held
    /// collateral assets) at which the account's health factor would be
    /// exactly `1.0`, holding every other collateral value and the debt
    /// value fixed at their current levels.
    ///
    /// Rounds **up**: the returned price is a conservative "at or above
    /// this price the account is still safe on this asset alone" bound —
    /// rounding it down could report a price that looks safe but, once
    /// truncated, is actually already at or past the liquidation
    /// boundary.
    ///
    /// Returns [`LendingError::EmptyMarket`] if `target_asset` is not
    /// held, if its liquidation threshold is zero, or if the account
    /// would remain safe at *any* finite price of `target_asset` (i.e.
    /// the other collateral alone already covers the debt) — there is no
    /// meaningful finite answer in that case.
    pub fn liquidation_price_at(
        &self,
        user: AccountId,
        target_asset: AssetId,
        timestamp: u64,
        prices: &PriceBook,
    ) -> Result<u128, LendingError> {
        let config = self
            .collateral_configs
            .get(&target_asset)
            .ok_or(LendingError::UnknownCollateralAsset)?;
        let balance = self.collateral_balance_of(user, target_asset);
        if balance == 0 || config.liquidation_threshold() == 0 {
            return Err(LendingError::EmptyMarket);
        }

        let debt_value = self.total_debt_value_at(user, timestamp, prices)?;

        let mut other_liquidation_adjusted: u128 = 0;
        if let Some(balances) = self.collateral_balances.get(&user) {
            for (&asset, &other_balance) in balances.iter() {
                if asset == target_asset || other_balance == 0 {
                    continue;
                }
                let other_config = self
                    .collateral_configs
                    .get(&asset)
                    .ok_or(LendingError::UnknownCollateralAsset)?;
                let price = prices.price_at(asset, timestamp)?;
                let value = mul_div_floor(other_balance, price, other_config.quantity_scale())?;
                let term = mul_div_floor(value, other_config.liquidation_threshold(), RATE_SCALE)?;
                other_liquidation_adjusted = other_liquidation_adjusted
                    .checked_add(term)
                    .ok_or(LendingError::ArithmeticOverflow)?;
            }
        }

        if debt_value <= other_liquidation_adjusted {
            return Err(LendingError::EmptyMarket);
        }
        let needed = debt_value - other_liquidation_adjusted;

        // needed = balance * price * threshold / (quantity_scale * RATE_SCALE)
        //   =>  price = ceil(needed * quantity_scale * RATE_SCALE / (balance * threshold))
        let scaled_needed = needed
            .checked_mul(config.quantity_scale())
            .ok_or(LendingError::ArithmeticOverflow)?;
        let denom = balance
            .checked_mul(config.liquidation_threshold())
            .ok_or(LendingError::ArithmeticOverflow)?;
        mul_div_ceil(scaled_needed, RATE_SCALE, denom)
    }

    // ---- collateral deposit / withdrawal -----------------------------------

    fn plan_deposit_collateral(
        &self,
        user: AccountId,
        asset: AssetId,
        amount: u128,
    ) -> Result<CollateralDepositPreview, LendingError> {
        if amount == 0 {
            return Err(LendingError::ZeroAmount);
        }
        let config = self
            .collateral_configs
            .get(&asset)
            .ok_or(LendingError::UnknownCollateralAsset)?;
        if !config.enabled() {
            return Err(LendingError::CollateralAssetDisabled);
        }
        let user_balance_after = self
            .collateral_balance_of(user, asset)
            .checked_add(amount)
            .ok_or(LendingError::ArithmeticOverflow)?;
        Ok(CollateralDepositPreview {
            asset,
            amount_deposited: amount,
            user_balance_after,
        })
    }

    /// Pure preview of [`RiskMarket::deposit_collateral`].
    pub fn preview_deposit_collateral(
        &self,
        user: AccountId,
        asset: AssetId,
        amount: u128,
    ) -> Result<CollateralDepositPreview, LendingError> {
        self.plan_deposit_collateral(user, asset, amount)
    }

    /// Deposits `amount` of `asset` as collateral for `user`. Requires no
    /// price (custodying more collateral can never increase anyone's
    /// risk) and no interest accrual (collateral balances carry no time-
    /// dependent state of their own).
    pub fn deposit_collateral(
        &mut self,
        user: AccountId,
        asset: AssetId,
        amount: u128,
    ) -> Result<CollateralDepositPreview, LendingError> {
        let preview = self.plan_deposit_collateral(user, asset, amount)?;
        self.collateral_balances
            .entry(user)
            .or_default()
            .insert(asset, preview.user_balance_after);
        Ok(preview)
    }

    fn plan_withdraw_collateral(
        &self,
        user: AccountId,
        asset: AssetId,
        timestamp: u64,
        amount: u128,
        prices: &PriceBook,
    ) -> Result<CollateralWithdrawPreview, LendingError> {
        if amount == 0 {
            return Err(LendingError::ZeroAmount);
        }
        // The asset must at least be configured for it to ever have been
        // deposited; withdrawal is allowed even if it has since been
        // disabled (disabling only blocks new deposits).
        self.collateral_configs
            .get(&asset)
            .ok_or(LendingError::UnknownCollateralAsset)?;

        let current_balance = self.collateral_balance_of(user, asset);
        if amount > current_balance {
            return Err(LendingError::InsufficientCollateral);
        }
        let user_balance_after = current_balance - amount;

        // The complete proposed post-withdrawal account: every other
        // collateral balance is untouched, and the market's debt is
        // unaffected by a collateral withdrawal, so only `asset`'s
        // balance needs to change in this hypothetical valuation.
        let after = self.collateral_totals_with_override_at(
            user,
            timestamp,
            prices,
            Some((asset, user_balance_after)),
        )?;
        let debt_value = self.total_debt_value_at(user, timestamp, prices)?;

        if debt_value > after.max_borrowing_capacity {
            return Err(LendingError::ExceedsMaxLtv);
        }
        let health_factor_after =
            health_factor_from(after.liquidation_adjusted_collateral, debt_value)?;
        if health_factor_after.is_liquidatable() {
            return Err(LendingError::HealthFactorTooLow);
        }

        Ok(CollateralWithdrawPreview {
            asset,
            amount_withdrawn: amount,
            user_balance_after,
            total_collateral_value_after: after.total_value,
            max_borrowing_capacity_after: after.max_borrowing_capacity,
            liquidation_adjusted_collateral_after: after.liquidation_adjusted_collateral,
            total_debt_value: debt_value,
            health_factor_after,
        })
    }

    /// Pure preview of [`RiskMarket::withdraw_collateral_at`].
    pub fn preview_withdraw_collateral_at(
        &self,
        user: AccountId,
        asset: AssetId,
        timestamp: u64,
        amount: u128,
        prices: &PriceBook,
    ) -> Result<CollateralWithdrawPreview, LendingError> {
        self.plan_withdraw_collateral(user, asset, timestamp, amount, prices)
    }

    /// Withdraws `amount` of `asset` from `user`'s collateral. This is
    /// risk-increasing (it can only ever reduce or hold constant the
    /// account's safety margin), so it requires valid, current prices for
    /// every asset the account holds plus the debt asset. Rejects
    /// insufficient balance, a post-withdrawal debt value above the
    /// (recomputed) maximum borrowing capacity, and a post-withdrawal
    /// health factor below `1.0`. Every check is computed against the
    /// complete proposed post-withdrawal account before anything is
    /// mutated.
    pub fn withdraw_collateral_at(
        &mut self,
        user: AccountId,
        asset: AssetId,
        timestamp: u64,
        amount: u128,
        prices: &PriceBook,
    ) -> Result<CollateralWithdrawPreview, LendingError> {
        let preview = self.plan_withdraw_collateral(user, asset, timestamp, amount, prices)?;
        let balances = self.collateral_balances.entry(user).or_default();
        if preview.user_balance_after == 0 {
            balances.remove(&asset);
        } else {
            balances.insert(asset, preview.user_balance_after);
        }
        Ok(preview)
    }

    // ---- risk-checked borrowing --------------------------------------------

    fn plan_borrow(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
        prices: &PriceBook,
    ) -> Result<(BorrowPreview, RiskBorrowPreview), LendingError> {
        // The market's own pure preview computes the complete proposed
        // post-borrow market state (cash, total_borrows, debt shares,
        // borrow index) without mutating anything — exactly the Day 11
        // guarantee this wrapper builds on.
        let market_preview = self.market.preview_borrow_at(user, timestamp, amount)?;

        // The account's debt after this borrow, derived the same way
        // `LendingMarket::debt_of_at` derives it (ceil-rounded scaled
        // balance against the borrow index) — never a second, disconnected
        // debt figure.
        let debt_after = mul_div_ceil(
            market_preview.user_debt_shares_after,
            market_preview.borrow_index_after,
            INDEX_SCALE,
        )?;
        let debt_price = prices.price_at(self.debt_asset.asset(), timestamp)?;
        let total_debt_value_after =
            mul_div_ceil(debt_after, debt_price, self.debt_asset.quantity_scale())?;

        // Collateral is unaffected by borrowing, so its valuation is
        // simply the account's current state at this timestamp.
        let totals = self.collateral_totals_at(user, timestamp, prices)?;

        if total_debt_value_after > totals.max_borrowing_capacity {
            return Err(LendingError::ExceedsMaxLtv);
        }
        // No separate post-borrow health-factor check is needed: because
        // `configure_collateral` enforces `max_ltv <= liquidation_threshold`
        // for every asset, `max_borrowing_capacity <= liquidation_adjusted_collateral`
        // always holds (see the property test of the same name), so
        // `total_debt_value_after <= max_borrowing_capacity` already
        // implies `total_debt_value_after <= liquidation_adjusted_collateral`,
        // i.e. `HF >= 1` after the borrow.
        let health_factor_after = health_factor_from(
            totals.liquidation_adjusted_collateral,
            total_debt_value_after,
        )?;

        Ok((
            market_preview,
            RiskBorrowPreview {
                market: market_preview,
                total_debt_value_after,
                max_borrowing_capacity: totals.max_borrowing_capacity,
                health_factor_after,
            },
        ))
    }

    /// Pure preview of [`RiskMarket::borrow_at`]. Never mutates `self`.
    pub fn preview_borrow_at(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
        prices: &PriceBook,
    ) -> Result<RiskBorrowPreview, LendingError> {
        self.plan_borrow(user, timestamp, amount, prices)
            .map(|(_, p)| p)
    }

    /// Borrows `amount` of the market's debt asset for `user`, subject to
    /// collateral checks. Accrues interest on the underlying market
    /// first (via the market's own `preview_borrow_at` / `borrow_at`),
    /// requires valid current prices for every collateral asset the
    /// account holds plus the debt asset, enforces maximum LTV, and
    /// enforces the market's own cash/liquidity and checked-arithmetic
    /// guarantees unchanged. A user with no collateral configured or
    /// deposited has `max_borrowing_capacity == 0` and so can never
    /// borrow a nonzero amount.
    ///
    /// The market-accounting mutation is delegated entirely to
    /// [`LendingMarket::borrow_at`] with the same arguments used for the
    /// risk check above, so the committed result is byte-for-byte the
    /// same the plain Day 11 borrow path would have produced.
    pub fn borrow_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
        prices: &PriceBook,
    ) -> Result<RiskBorrowPreview, LendingError> {
        let (market_preview, risk_preview) = self.plan_borrow(user, timestamp, amount, prices)?;
        let committed = self.market.borrow_at(user, timestamp, amount)?;
        debug_assert_eq!(
            committed, market_preview,
            "a risk-checked borrow must commit exactly what its own preview projected"
        );
        Ok(risk_preview)
    }

    // ---- repayment and plain liquidity passthroughs ------------------------
    //
    // Repayment is risk-*reducing*: it never needs a price, and must keep
    // working even when the oracle is unavailable. It is not wrapped with
    // any additional logic — these methods delegate straight to the
    // market, reusing its rounding rules and debt-share accounting
    // unchanged.

    /// Pure preview of [`RiskMarket::repay_at`].
    pub fn preview_repay_at(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<RepayPreview, LendingError> {
        self.market.preview_repay_at(user, timestamp, amount)
    }

    /// Repays `amount` of `user`'s debt. Delegates entirely to
    /// [`LendingMarket::repay_at`]; no price or collateral check.
    pub fn repay_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<RepayPreview, LendingError> {
        self.market.repay_at(user, timestamp, amount)
    }

    /// Pure preview of [`RiskMarket::repay_all_at`].
    pub fn preview_repay_all_at(
        &self,
        user: AccountId,
        timestamp: u64,
    ) -> Result<RepayPreview, LendingError> {
        self.market.preview_repay_all_at(user, timestamp)
    }

    /// Repays the entirety of `user`'s debt. Delegates entirely to
    /// [`LendingMarket::repay_all_at`]; no price or collateral check.
    pub fn repay_all_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
    ) -> Result<RepayPreview, LendingError> {
        self.market.repay_all_at(user, timestamp)
    }

    /// Pure preview of [`RiskMarket::supply_liquidity_at`].
    pub fn preview_supply_liquidity_at(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<SupplyPreview, LendingError> {
        self.market.preview_supply_at(user, timestamp, amount)
    }

    /// Supplies market liquidity (unrelated to collateral — this is the
    /// Day 11 supplier role). Delegates entirely to
    /// [`LendingMarket::supply_at`].
    pub fn supply_liquidity_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<SupplyPreview, LendingError> {
        self.market.supply_at(user, timestamp, amount)
    }

    /// Pure preview of [`RiskMarket::withdraw_liquidity_at`].
    pub fn preview_withdraw_liquidity_at(
        &self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<WithdrawPreview, LendingError> {
        self.market.preview_withdraw_at(user, timestamp, amount)
    }

    /// Withdraws market liquidity (unrelated to collateral). Delegates
    /// entirely to [`LendingMarket::withdraw_at`].
    pub fn withdraw_liquidity_at(
        &mut self,
        user: AccountId,
        timestamp: u64,
        amount: u128,
    ) -> Result<WithdrawPreview, LendingError> {
        self.market.withdraw_at(user, timestamp, amount)
    }
}

/// Computes a health factor from an already-known liquidation-adjusted
/// collateral value and total debt value, independent of any
/// [`RiskMarket`] or account state. [`RiskMarket::health_factor_at`] is
/// itself built on this function, so the wired and standalone paths share
/// one implementation.
///
/// Rounds down (via the same floor division `health_factor_at` uses):
/// both inputs are expected to already be conservatively rounded (the
/// numerator down, the denominator up), and this final division rounds
/// down again, so the result never overstates safety.
pub fn health_factor_from(
    liquidation_adjusted_collateral: u128,
    total_debt_value: u128,
) -> Result<HealthFactor, LendingError> {
    if total_debt_value == 0 {
        return Ok(HealthFactor::NoDebt);
    }
    let ratio = mul_div_floor(
        liquidation_adjusted_collateral,
        RATE_SCALE,
        total_debt_value,
    )?;
    Ok(HealthFactor::Ratio(ratio))
}

/// One position in the standalone multi-debt valuation helper below.
///
/// **Not** integrated with [`RiskMarket`] or [`LendingMarket`] — the
/// underlying market this crate models remains single-borrow-asset (see
/// the README's "Multiple debt valuation" section for why bolting a
/// second live, interest-accruing debt market on top here would distort
/// the Day 11 accounting this crate exists to demonstrate, rather than
/// extend it cleanly). This type and [`total_debt_value_multi`] exist to
/// show, and test, how a multi-debt valuation *would* compose from
/// already-known debt amounts, independent of any market's own
/// bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebtPosition {
    /// The asset this debt is denominated in.
    pub asset: AssetId,
    /// The raw (smallest-unit) amount owed.
    pub amount: u128,
    /// The number of smallest units equal to one whole unit of `asset`.
    pub quantity_scale: u128,
}

/// Sums the quote-unit value of several independent [`DebtPosition`]s.
/// Each position's conversion rounds **up** individually (never
/// understate debt), before summing with checked addition — the same
/// per-term-then-sum discipline [`RiskMarket`]'s collateral totals use.
/// A position with `amount == 0` is skipped without requiring a price
/// for it, matching [`RiskMarket::total_debt_value_at`]'s zero-debt
/// policy.
pub fn total_debt_value_multi(
    positions: &[DebtPosition],
    timestamp: u64,
    prices: &PriceBook,
) -> Result<u128, LendingError> {
    let mut total: u128 = 0;
    for position in positions {
        if position.amount == 0 {
            continue;
        }
        let price = prices.price_at(position.asset, timestamp)?;
        let value = mul_div_ceil(position.amount, price, position.quantity_scale)?;
        total = total
            .checked_add(value)
            .ok_or(LendingError::ArithmeticOverflow)?;
    }
    Ok(total)
}
