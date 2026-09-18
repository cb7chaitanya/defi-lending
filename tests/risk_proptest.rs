//! Property-based tests for the Day 12 collateral/risk layer.
//!
//! Bounds here are chosen generously (balances/prices well under
//! `u128::MAX`, percentages within `[0, RATE_SCALE]`) precisely so
//! overflow is *not* expected to occur in these algebraic properties —
//! overflow itself is exercised deliberately, with hand-picked huge
//! inputs, in `tests/risk_adversarial.rs`. No property here uses a
//! guessed tolerance: every inequality is either a direct consequence of
//! `mul_div_floor`/`mul_div_ceil`'s definitions (property 10) or of the
//! `max_ltv <= liquidation_threshold` configuration invariant (property
//! 5), both stated in the assertions' comments.

use defi_lending::{
    AccountId, AssetId, CollateralConfig, DebtAssetConfig, HealthFactor, InterestRateModel,
    LendingMarket, PRICE_SCALE, PriceBook, PriceQuote, RATE_SCALE, RiskMarket,
};
use proptest::prelude::*;

const SOL: AssetId = AssetId(1);
const OTHER: AssetId = AssetId(2);
const USDC: AssetId = AssetId(0);
const LENDER: AccountId = AccountId(100);
const ALICE: AccountId = AccountId(1);
const BOB: AccountId = AccountId(2);
const T0: u64 = 1_700_000_000;
const MAX_AGE: u64 = 3600;

fn pct(p: u128) -> u128 {
    RATE_SCALE * p / 100
}

fn usd(whole: u128) -> u128 {
    whole * PRICE_SCALE
}

fn flat_rate(apr_pct: u128) -> InterestRateModel {
    InterestRateModel::new(pct(apr_pct), pct(50), 0, 0).unwrap()
}

fn new_risk_market() -> RiskMarket {
    let mut market = LendingMarket::new(flat_rate(5), 0, T0).unwrap();
    market.supply_at(LENDER, T0, u128::MAX / 4).unwrap();
    RiskMarket::new(market, DebtAssetConfig::new(USDC, 1).unwrap())
}

fn price_quote(asset: AssetId, whole_dollars: u128, observed_at: u64) -> PriceQuote {
    PriceQuote::new(asset, usd(whole_dollars), observed_at, MAX_AGE).unwrap()
}

fn price_book(quotes: &[PriceQuote]) -> PriceBook {
    let mut book = PriceBook::new();
    for quote in quotes {
        book.set(*quote);
    }
    book
}

/// Converts a quote-unit *value* (as `max_borrowing_capacity_at` and
/// friends return) into a *raw* debt-asset amount suitable for
/// `borrow_at`'s `amount` parameter. Valid only under this file's
/// standard setup, where the debt asset (USDC) always has
/// `quantity_scale == 1` and is quoted at exactly $1 (`price_quote(USDC,
/// 1, ..)`), so `value == raw_amount * PRICE_SCALE` exactly.
fn raw_amount_for_value(value: u128) -> u128 {
    value / PRICE_SCALE
}

/// A liquidation threshold in `(0, RATE_SCALE]` paired with a max LTV in
/// `[0, threshold]` — always a valid [`CollateralConfig`] pair.
fn arb_ltv_pair() -> impl Strategy<Value = (u128, u128)> {
    (1u128..=RATE_SCALE)
        .prop_flat_map(|threshold| (0..=threshold).prop_map(move |ltv| (ltv, threshold)))
}

fn arb_balance() -> impl Strategy<Value = u128> {
    1u128..=1_000_000
}

fn arb_price() -> impl Strategy<Value = u128> {
    1u128..=100_000
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(256)
    ))]

    /// 1. Increasing collateral, everything else fixed, never decreases
    /// borrowing capacity or health factor.
    #[test]
    fn increasing_collateral_never_decreases_capacity_or_health(
        balance in arb_balance(),
        extra in 0u128..1_000_000,
        price in arb_price(),
        (max_ltv, threshold) in arb_ltv_pair(),
    ) {
        let mut risk = new_risk_market();
        let config = CollateralConfig::new(SOL, max_ltv, threshold, 1, true).unwrap();
        risk.configure_collateral(config).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();
        risk.deposit_collateral(BOB, SOL, balance + extra).unwrap();

        let prices = price_book(&[price_quote(SOL, price, T0), price_quote(USDC, 1, T0)]);
        // Same, deliberately small, fixed debt for both — well within
        // even the smaller account's capacity whenever max_ltv > 0;
        // when max_ltv == 0 no debt is taken so both accounts stay debt-free.
        let cap_alice = risk.max_borrowing_capacity_at(ALICE, T0, &prices).unwrap();
        let cap_bob = risk.max_borrowing_capacity_at(BOB, T0, &prices).unwrap();
        prop_assert!(cap_bob >= cap_alice);

        let debt = raw_amount_for_value(cap_alice);
        if debt > 0
            && risk.borrow_at(ALICE, T0, debt, &prices).is_ok()
            && risk.borrow_at(BOB, T0, debt, &prices).is_ok()
        {
            let hf_alice = risk.health_factor_at(ALICE, T0, &prices).unwrap();
            let hf_bob = risk.health_factor_at(BOB, T0, &prices).unwrap();
            if let (HealthFactor::Ratio(a), HealthFactor::Ratio(b)) = (hf_alice, hf_bob) {
                prop_assert!(b >= a);
            }
        }
    }

    /// 2. Increasing debt, everything else fixed, never decreases current
    /// LTV and never increases health factor.
    #[test]
    fn increasing_debt_never_decreases_ltv_or_increases_health(
        balance in 1_000u128..1_000_000,
        price in arb_price(),
        (max_ltv, threshold) in arb_ltv_pair(),
        extra_fraction in 1u128..=100,
    ) {
        let mut risk = new_risk_market();
        let config = CollateralConfig::new(SOL, max_ltv, threshold, 1, true).unwrap();
        risk.configure_collateral(config).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();
        let prices = price_book(&[price_quote(SOL, price, T0), price_quote(USDC, 1, T0)]);

        let cap = raw_amount_for_value(risk.max_borrowing_capacity_at(ALICE, T0, &prices).unwrap());
        prop_assume!(cap >= 2);
        let debt1 = cap / 2;
        prop_assume!(debt1 >= 1);
        let debt2_extra = ((cap - debt1) * extra_fraction / 100).max(1);

        risk.borrow_at(ALICE, T0, debt1, &prices).unwrap();
        let ltv1 = risk.current_ltv_at(ALICE, T0, &prices).unwrap();
        let hf1 = risk.health_factor_at(ALICE, T0, &prices).unwrap();

        if risk.borrow_at(ALICE, T0, debt2_extra, &prices).is_ok() {
            let ltv2 = risk.current_ltv_at(ALICE, T0, &prices).unwrap();
            let hf2 = risk.health_factor_at(ALICE, T0, &prices).unwrap();
            prop_assert!(ltv2 >= ltv1);
            if let (HealthFactor::Ratio(a), HealthFactor::Ratio(b)) = (hf1, hf2) {
                prop_assert!(b <= a);
            }
        }
    }

    /// 3. Increasing a collateral price never decreases collateral value
    /// or health factor.
    #[test]
    fn increasing_collateral_price_never_decreases_value_or_health(
        balance in arb_balance(),
        price1 in arb_price(),
        price_extra in 0u128..100_000,
        (max_ltv, threshold) in arb_ltv_pair(),
    ) {
        let price2 = price1 + price_extra;
        let mut risk = new_risk_market();
        let config = CollateralConfig::new(SOL, max_ltv, threshold, 1, true).unwrap();
        risk.configure_collateral(config).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();

        let prices1 = price_book(&[price_quote(SOL, price1, T0), price_quote(USDC, 1, T0)]);
        let prices2 = price_book(&[price_quote(SOL, price2, T0), price_quote(USDC, 1, T0)]);

        let value1 = risk.total_collateral_value_at(ALICE, T0, &prices1).unwrap();
        let value2 = risk.total_collateral_value_at(ALICE, T0, &prices2).unwrap();
        prop_assert!(value2 >= value1);

        let cap = raw_amount_for_value(risk.max_borrowing_capacity_at(ALICE, T0, &prices1).unwrap());
        if cap > 0 && risk.borrow_at(ALICE, T0, cap, &prices1).is_ok() {
            let hf1 = risk.health_factor_at(ALICE, T0, &prices1).unwrap();
            let hf2 = risk.health_factor_at(ALICE, T0, &prices2).unwrap();
            if let (HealthFactor::Ratio(a), HealthFactor::Ratio(b)) = (hf1, hf2) {
                prop_assert!(b >= a);
            }
        }
    }

    /// 4. Increasing a debt-asset price never increases health factor.
    #[test]
    fn increasing_debt_price_never_increases_health(
        balance in arb_balance(),
        debt_price1 in arb_price(),
        debt_price_extra in 0u128..100_000,
        (max_ltv, threshold) in arb_ltv_pair(),
        fraction in 1u128..=100,
    ) {
        prop_assume!(max_ltv > 0);
        let debt_price2 = debt_price1 + debt_price_extra;
        let mut market = LendingMarket::new(flat_rate(5), 0, T0).unwrap();
        market.supply_at(LENDER, T0, u128::MAX / 4).unwrap();
        let mut risk = RiskMarket::new(market, DebtAssetConfig::new(OTHER, 1).unwrap());
        let config = CollateralConfig::new(SOL, max_ltv, threshold, 1, true).unwrap();
        risk.configure_collateral(config).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();

        // Stable collateral price throughout; only the debt asset's price moves.
        let prices1 = price_book(&[price_quote(SOL, 1, T0), price_quote(OTHER, debt_price1, T0)]);
        let cap = risk.max_borrowing_capacity_at(ALICE, T0, &prices1).unwrap();
        let max_affordable = cap / usd(debt_price1);
        // A tiny max_ltv combined with an expensive debt asset can
        // legitimately leave nothing affordable; that's not a rejection
        // of the input, just a vacuous case for this property.
        let debt_amount = ((max_affordable * fraction / 100).max(1)).min(max_affordable);

        if debt_amount > 0 && risk.borrow_at(ALICE, T0, debt_amount, &prices1).is_ok() {
            let hf1 = risk.health_factor_at(ALICE, T0, &prices1).unwrap();

            let prices2 = price_book(&[price_quote(SOL, 1, T0), price_quote(OTHER, debt_price2, T0)]);
            let hf2 = risk.health_factor_at(ALICE, T0, &prices2).unwrap();

            if let (HealthFactor::Ratio(a), HealthFactor::Ratio(b)) = (hf1, hf2) {
                prop_assert!(b <= a);
            }
        }
    }

    /// 5. Maximum borrowing capacity never exceeds liquidation-adjusted
    /// collateral, for any number of held assets, since `configure_collateral`
    /// enforces `max_ltv <= liquidation_threshold` per asset and both
    /// totals are sums of per-asset terms built from the same collateral
    /// value.
    #[test]
    fn max_capacity_never_exceeds_liquidation_adjusted_collateral(
        balance_sol in arb_balance(),
        balance_other in arb_balance(),
        price_sol in arb_price(),
        price_other in arb_price(),
        (ltv_sol, thr_sol) in arb_ltv_pair(),
        (ltv_other, thr_other) in arb_ltv_pair(),
    ) {
        let mut risk = new_risk_market();
        risk.configure_collateral(CollateralConfig::new(SOL, ltv_sol, thr_sol, 1, true).unwrap()).unwrap();
        risk.configure_collateral(CollateralConfig::new(OTHER, ltv_other, thr_other, 1, true).unwrap()).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance_sol).unwrap();
        risk.deposit_collateral(ALICE, OTHER, balance_other).unwrap();

        let prices = price_book(&[
            price_quote(SOL, price_sol, T0),
            price_quote(OTHER, price_other, T0),
            price_quote(USDC, 1, T0),
        ]);
        let cap = risk.max_borrowing_capacity_at(ALICE, T0, &prices).unwrap();
        let liq_adj = risk.liquidation_adjusted_collateral_at(ALICE, T0, &prices).unwrap();
        prop_assert!(cap <= liq_adj);
    }

    /// 6. A successful collateral-aware borrow leaves debt at or below
    /// maximum borrowing capacity.
    #[test]
    fn successful_borrow_leaves_debt_at_or_below_capacity(
        balance in arb_balance(),
        price in arb_price(),
        (max_ltv, threshold) in arb_ltv_pair(),
        borrow_amount in 1u128..1_000_000,
    ) {
        let mut risk = new_risk_market();
        risk.configure_collateral(CollateralConfig::new(SOL, max_ltv, threshold, 1, true).unwrap()).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();
        let prices = price_book(&[price_quote(SOL, price, T0), price_quote(USDC, 1, T0)]);

        if risk.borrow_at(ALICE, T0, borrow_amount, &prices).is_ok() {
            let debt_value = risk.total_debt_value_at(ALICE, T0, &prices).unwrap();
            let cap = risk.max_borrowing_capacity_at(ALICE, T0, &prices).unwrap();
            prop_assert!(debt_value <= cap);
        }
    }

    /// 7. A successful collateral withdrawal leaves the account within
    /// its configured safety constraints (debt <= capacity, HF not
    /// liquidatable).
    #[test]
    fn successful_withdrawal_leaves_account_safe(
        balance in 10u128..1_000_000,
        price in arb_price(),
        (max_ltv, threshold) in arb_ltv_pair(),
        borrow_fraction in 0u128..=100,
        withdraw_amount in 1u128..1_000,
    ) {
        prop_assume!(max_ltv > 0);
        let mut risk = new_risk_market();
        risk.configure_collateral(CollateralConfig::new(SOL, max_ltv, threshold, 1, true).unwrap()).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();
        let prices = price_book(&[price_quote(SOL, price, T0), price_quote(USDC, 1, T0)]);

        let cap = raw_amount_for_value(risk.max_borrowing_capacity_at(ALICE, T0, &prices).unwrap());
        let debt = cap * borrow_fraction / 100;
        if debt > 0 {
            prop_assume!(risk.borrow_at(ALICE, T0, debt, &prices).is_ok());
        }

        if risk
            .withdraw_collateral_at(ALICE, SOL, T0, withdraw_amount, &prices)
            .is_ok()
        {
            let debt_value = risk.total_debt_value_at(ALICE, T0, &prices).unwrap();
            let cap_after = risk.max_borrowing_capacity_at(ALICE, T0, &prices).unwrap();
            prop_assert!(debt_value <= cap_after);
            prop_assert!(!risk.is_liquidatable_at(ALICE, T0, &prices).unwrap());
        }
    }

    /// 8. Failed operations leave all state unchanged.
    #[test]
    fn failed_operations_leave_state_unchanged(
        balance in arb_balance(),
        price in arb_price(),
        (max_ltv, threshold) in arb_ltv_pair(),
        deposit_amount in 0u128..1_000,
        withdraw_amount in 0u128..1_000_000,
        borrow_amount in 0u128..1_000_000,
        repay_amount in 0u128..1_000,
    ) {
        let mut risk = new_risk_market();
        risk.configure_collateral(CollateralConfig::new(SOL, max_ltv, threshold, 1, true).unwrap()).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();
        let prices = price_book(&[price_quote(SOL, price, T0), price_quote(USDC, 1, T0)]);

        macro_rules! check_atomic {
            ($op:expr) => {{
                let before = risk.clone();
                if $op.is_err() {
                    prop_assert_eq!(&risk, &before);
                }
            }};
        }

        check_atomic!(risk.deposit_collateral(ALICE, SOL, deposit_amount));
        check_atomic!(risk.withdraw_collateral_at(ALICE, SOL, T0, withdraw_amount, &prices));
        check_atomic!(risk.borrow_at(ALICE, T0, borrow_amount, &prices));
        check_atomic!(risk.repay_at(ALICE, T0, repay_amount));
    }

    /// 9. Total valuations equal the checked sum of per-asset valuations.
    #[test]
    fn total_valuations_equal_sum_of_per_asset_valuations(
        balance_sol in arb_balance(),
        balance_other in arb_balance(),
        price_sol in arb_price(),
        price_other in arb_price(),
        (ltv_sol, thr_sol) in arb_ltv_pair(),
        (ltv_other, thr_other) in arb_ltv_pair(),
    ) {
        let mut risk = new_risk_market();
        risk.configure_collateral(CollateralConfig::new(SOL, ltv_sol, thr_sol, 1, true).unwrap()).unwrap();
        risk.configure_collateral(CollateralConfig::new(OTHER, ltv_other, thr_other, 1, true).unwrap()).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance_sol).unwrap();
        risk.deposit_collateral(ALICE, OTHER, balance_other).unwrap();

        let prices = price_book(&[
            price_quote(SOL, price_sol, T0),
            price_quote(OTHER, price_other, T0),
            price_quote(USDC, 1, T0),
        ]);

        let total = risk.total_collateral_value_at(ALICE, T0, &prices).unwrap();
        let sol_value = risk.collateral_value_at(ALICE, SOL, T0, &prices).unwrap();
        let other_value = risk.collateral_value_at(ALICE, OTHER, T0, &prices).unwrap();
        prop_assert_eq!(total, sol_value.checked_add(other_value).unwrap());
    }

    /// 10. Conservative rounding never overstates collateral or
    /// understates debt, versus the exact (unrounded) cross-multiplied
    /// rational inequality: `collateral_value * quantity_scale <=
    /// balance * price` (floor can only round down) and
    /// `debt_value * quantity_scale >= debt_amount * price` (ceil can
    /// only round up). Both follow directly from `mul_div_floor` /
    /// `mul_div_ceil`'s definitions, not an empirically chosen bound.
    #[test]
    fn rounding_never_overstates_collateral_or_understates_debt(
        balance in arb_balance(),
        price in arb_price(),
        (max_ltv, threshold) in arb_ltv_pair(),
        quantity_scale in 1u128..1_000,
    ) {
        let mut risk = new_risk_market();
        risk.configure_collateral(CollateralConfig::new(SOL, max_ltv, threshold, quantity_scale, true).unwrap()).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();
        let prices = price_book(&[price_quote(SOL, price, T0), price_quote(USDC, 1, T0)]);

        let value = risk.collateral_value_at(ALICE, SOL, T0, &prices).unwrap();
        // value = floor(balance * price_scaled / quantity_scale)
        //   => value * quantity_scale <= balance * price_scaled
        let price_scaled = prices.price_at(SOL, T0).unwrap();
        prop_assert!(value * quantity_scale <= balance * price_scaled);

        if risk.borrow_at(ALICE, T0, 1, &prices).is_ok() {
            let debt_amount = risk.market().debt_of_at(ALICE, T0).unwrap();
            let debt_value = risk.total_debt_value_at(ALICE, T0, &prices).unwrap();
            let debt_price = prices.price_at(USDC, T0).unwrap();
            // debt_value = ceil(debt_amount * debt_price / 1)
            //   => debt_value >= debt_amount * debt_price
            prop_assert!(debt_value >= debt_amount * debt_price);
        }
    }

    /// 11 & 12. Preview calls are pure, and agree with execution, over
    /// randomized collateral/borrow sequences.
    #[test]
    fn preview_is_pure_and_agrees_with_execution(
        balance in arb_balance(),
        price in arb_price(),
        (max_ltv, threshold) in arb_ltv_pair(),
        deposit_amount in 1u128..1_000,
        borrow_amount in 1u128..1_000,
    ) {
        let mut risk = new_risk_market();
        risk.configure_collateral(CollateralConfig::new(SOL, max_ltv, threshold, 1, true).unwrap()).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();
        let prices = price_book(&[price_quote(SOL, price, T0), price_quote(USDC, 1, T0)]);

        // Each preview is taken immediately before its matching execution,
        // with no intervening mutation — checking purity and agreement
        // per-operation, since a preview only promises to match execution
        // when nothing else has changed the state in between.
        let before = risk.clone();
        let deposit_preview = risk.preview_deposit_collateral(ALICE, SOL, deposit_amount);
        prop_assert_eq!(&risk, &before, "preview_deposit_collateral must never mutate state");
        if let Ok(preview) = deposit_preview {
            let actual = risk.deposit_collateral(ALICE, SOL, deposit_amount).unwrap();
            prop_assert_eq!(preview, actual);
        }

        let before = risk.clone();
        let borrow_preview = risk.preview_borrow_at(ALICE, T0, borrow_amount, &prices);
        prop_assert_eq!(&risk, &before, "preview_borrow_at must never mutate state");
        if let Ok(preview) = borrow_preview
            && let Ok(actual) = risk.borrow_at(ALICE, T0, borrow_amount, &prices)
        {
            prop_assert_eq!(preview, actual);
        }
    }

    /// 13. User debt-share sums reconcile with the global total after
    /// random borrow/repay sequences performed *through* `RiskMarket`
    /// (not just the raw `LendingMarket`) — proving the wrapper never
    /// duplicates or desynchronizes Day 11's own debt-share accounting.
    #[test]
    fn debt_shares_reconcile_through_risk_market_after_random_sequences(
        balance in 10_000u128..1_000_000,
        price in arb_price(),
        ops in prop::collection::vec((0usize..2, 1u128..1_000), 0..20),
    ) {
        let mut risk = new_risk_market();
        risk.configure_collateral(CollateralConfig::new(SOL, pct(70), pct(80), 1, true).unwrap()).unwrap();
        risk.deposit_collateral(ALICE, SOL, balance).unwrap();
        risk.deposit_collateral(BOB, SOL, balance).unwrap();
        let prices = price_book(&[price_quote(SOL, price, T0), price_quote(USDC, 1, T0)]);

        for (who, amount) in ops {
            let user = if who == 0 { ALICE } else { BOB };
            if who == 0 {
                let _ = risk.borrow_at(user, T0, amount, &prices);
            } else {
                let _ = risk.repay_at(user, T0, amount);
            }
        }

        let sum: u128 = risk
            .market()
            .debt_share_accounts()
            .map(|(_, shares)| *shares)
            .sum();
        prop_assert_eq!(sum, risk.market().total_debt_shares());
    }
}
