//! Day 12 adversarial and regression tests: boundary conditions, oracle
//! failure handling, overflow, atomicity, and preview/execution agreement
//! for the collateral/risk layer.

use defi_lending::{
    AccountId, AssetId, CollateralConfig, DebtAssetConfig, DebtPosition, InterestRateModel,
    LendingError, LendingMarket, PRICE_SCALE, PriceBook, PriceQuote, RATE_SCALE, RiskMarket,
    SECONDS_PER_YEAR, total_debt_value_multi,
};

const SOL: AssetId = AssetId(1);
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

fn sol_config(max_ltv_pct: u128, liquidation_threshold_pct: u128) -> CollateralConfig {
    CollateralConfig::new(
        SOL,
        pct(max_ltv_pct),
        pct(liquidation_threshold_pct),
        1,
        true,
    )
    .unwrap()
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

fn new_risk_market(apr_pct: u128) -> RiskMarket {
    let mut market = LendingMarket::new(flat_rate(apr_pct), 0, T0).unwrap();
    market.supply_at(LENDER, T0, 10_000_000).unwrap();
    RiskMarket::new(market, DebtAssetConfig::new(USDC, 1).unwrap())
}

// ---- collateral deposit ---------------------------------------------------

#[test]
fn zero_collateral_deposit_is_rejected() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    assert_eq!(
        risk.deposit_collateral(ALICE, SOL, 0),
        Err(LendingError::ZeroAmount)
    );
}

#[test]
fn collateral_balance_overflow_is_rejected() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, u128::MAX).unwrap();

    let before = risk.clone();
    assert_eq!(
        risk.deposit_collateral(ALICE, SOL, 1),
        Err(LendingError::ArithmeticOverflow)
    );
    assert_eq!(risk, before, "overflow must not silently wrap the balance");
    assert_eq!(risk.collateral_balance_of(ALICE, SOL), u128::MAX);
}

#[test]
fn unknown_collateral_asset_is_rejected() {
    let mut risk = new_risk_market(5);
    let unknown = AssetId(999);
    assert_eq!(
        risk.deposit_collateral(ALICE, unknown, 10),
        Err(LendingError::UnknownCollateralAsset)
    );
}

#[test]
fn disabled_collateral_blocks_new_deposits_but_not_withdrawal() {
    let mut risk = new_risk_market(5);
    // Deposit while enabled...
    let enabled = CollateralConfig::new(SOL, pct(70), pct(80), 1, true).unwrap();
    risk.configure_collateral(enabled).unwrap();
    risk.deposit_collateral(ALICE, SOL, 5).unwrap();

    // ...cannot re-register as disabled (duplicate), so build a second
    // market to prove a disabled asset simply cannot be deposited at all.
    let mut risk2 = new_risk_market(5);
    let disabled = CollateralConfig::new(SOL, pct(70), pct(80), 1, false).unwrap();
    risk2.configure_collateral(disabled).unwrap();
    assert_eq!(
        risk2.deposit_collateral(ALICE, SOL, 5),
        Err(LendingError::CollateralAssetDisabled)
    );

    // The first (enabled-at-deposit-time) market can still withdraw.
    let prices = price_book(&[price_quote(SOL, 100, T0), price_quote(USDC, 1, T0)]);
    assert!(
        risk.withdraw_collateral_at(ALICE, SOL, T0, 5, &prices)
            .is_ok()
    );
}

// ---- collateral withdrawal -------------------------------------------------

#[test]
fn withdrawing_more_collateral_than_owned_is_rejected() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 5).unwrap();
    let prices = price_book(&[price_quote(SOL, 100, T0), price_quote(USDC, 1, T0)]);

    let before = risk.clone();
    assert_eq!(
        risk.withdraw_collateral_at(ALICE, SOL, T0, 6, &prices),
        Err(LendingError::InsufficientCollateral)
    );
    assert_eq!(risk, before);
}

#[test]
fn withdrawal_exactly_to_the_ltv_boundary_succeeds_one_unit_beyond_fails() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(50, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 10).unwrap();
    let prices = price_book(&[price_quote(SOL, 100, T0), price_quote(USDC, 1, T0)]);
    // value = 1000, cap = 500; borrow comfortably within it.
    risk.borrow_at(ALICE, T0, 400, &prices).unwrap();

    // Withdrawing 2 SOL leaves 8 SOL (value 800, cap 400) == debt exactly.
    let mut at_boundary = risk.clone();
    assert!(
        at_boundary
            .withdraw_collateral_at(ALICE, SOL, T0, 2, &prices)
            .is_ok()
    );
    assert_eq!(
        at_boundary
            .max_borrowing_capacity_at(ALICE, T0, &prices)
            .unwrap(),
        usd(400)
    );

    // Withdrawing 3 SOL leaves 7 SOL (value 700, cap 350) < debt (400).
    let before = risk.clone();
    assert_eq!(
        risk.withdraw_collateral_at(ALICE, SOL, T0, 3, &prices),
        Err(LendingError::ExceedsMaxLtv)
    );
    assert_eq!(risk, before);
}

// ---- collateral-aware borrowing --------------------------------------------

#[test]
fn borrowing_with_no_collateral_is_rejected() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    let prices = price_book(&[price_quote(SOL, 100, T0), price_quote(USDC, 1, T0)]);

    let before = risk.clone();
    assert_eq!(
        risk.borrow_at(ALICE, T0, 1, &prices),
        Err(LendingError::ExceedsMaxLtv)
    );
    assert_eq!(risk, before);
}

#[test]
fn borrowing_exactly_at_max_ltv_succeeds_one_unit_beyond_fails() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    // value = 1600, cap = 1120.

    let mut at_cap = risk.clone();
    assert!(at_cap.borrow_at(ALICE, T0, 1120, &prices).is_ok());

    let before = risk.clone();
    assert_eq!(
        risk.borrow_at(ALICE, T0, 1121, &prices),
        Err(LendingError::ExceedsMaxLtv)
    );
    assert_eq!(risk, before);
}

// ---- oracle / price validation ---------------------------------------------

#[test]
fn missing_price_is_rejected() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let empty = PriceBook::new();
    assert_eq!(
        risk.borrow_at(ALICE, T0, 1, &empty),
        Err(LendingError::MissingPrice)
    );
}

#[test]
fn zero_price_is_rejected_at_construction() {
    assert_eq!(
        PriceQuote::new(SOL, 0, T0, MAX_AGE),
        Err(LendingError::ZeroPrice)
    );
}

#[test]
fn stale_price_blocks_a_borrow() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices = price_book(&[
        PriceQuote::new(SOL, usd(200), T0, 60).unwrap(),
        PriceQuote::new(USDC, usd(1), T0, 60).unwrap(),
    ]);
    let before = risk.clone();
    assert_eq!(
        risk.borrow_at(ALICE, T0 + 61, 1, &prices),
        Err(LendingError::StalePrice)
    );
    assert_eq!(risk, before);
}

#[test]
fn future_dated_price_is_rejected_when_evaluated_earlier() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices = price_book(&[
        PriceQuote::new(SOL, usd(200), T0 + 100, MAX_AGE).unwrap(),
        PriceQuote::new(USDC, usd(1), T0, MAX_AGE).unwrap(),
    ]);
    assert_eq!(
        risk.borrow_at(ALICE, T0, 1, &prices),
        Err(LendingError::FuturePriceObservation)
    );
}

#[test]
fn price_failure_blocks_borrow_and_withdrawal_but_not_repayment_or_deposit() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(ALICE, T0, 500, &prices).unwrap();

    let empty = PriceBook::new();
    assert_eq!(
        risk.borrow_at(ALICE, T0, 1, &empty),
        Err(LendingError::MissingPrice)
    );
    assert_eq!(
        risk.withdraw_collateral_at(ALICE, SOL, T0, 1, &empty),
        Err(LendingError::MissingPrice)
    );

    // Risk-reducing operations keep working without any oracle at all.
    assert!(risk.repay_at(ALICE, T0, 100).is_ok());
    assert!(risk.deposit_collateral(ALICE, SOL, 1).is_ok());
}

// ---- invalid configuration --------------------------------------------------

#[test]
fn max_ltv_above_liquidation_threshold_is_rejected() {
    assert_eq!(
        CollateralConfig::new(SOL, pct(90), pct(80), 1, true),
        Err(LendingError::InvalidCollateralConfig(
            "max_ltv must not exceed liquidation_threshold"
        ))
    );
}

#[test]
fn duplicate_collateral_asset_is_rejected() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    assert_eq!(
        risk.configure_collateral(sol_config(60, 70)),
        Err(LendingError::DuplicateCollateralAsset)
    );
}

// ---- overflow ---------------------------------------------------------------

#[test]
fn collateral_valuation_overflow_is_rejected() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, u128::MAX / 100)
        .unwrap();
    let huge_price = PriceQuote::new(SOL, u128::MAX / 100, T0, MAX_AGE).unwrap();
    let prices = price_book(&[huge_price, price_quote(USDC, 1, T0)]);

    assert_eq!(
        risk.total_collateral_value_at(ALICE, T0, &prices),
        Err(LendingError::ArithmeticOverflow)
    );
}

#[test]
fn debt_valuation_overflow_is_rejected() {
    // Exercised directly against the standalone multi-debt helper, since
    // driving a live market's debt anywhere near u128::MAX is not
    // reachable through normal borrowing.
    let positions = [DebtPosition {
        asset: USDC,
        amount: u128::MAX / 2,
        quantity_scale: 1,
    }];
    let huge_price = PriceQuote::new(USDC, u128::MAX / 2, T0, MAX_AGE).unwrap();
    let mut book = PriceBook::new();
    book.set(huge_price);
    assert_eq!(
        total_debt_value_multi(&positions, T0, &book),
        Err(LendingError::ArithmeticOverflow)
    );
}

#[test]
fn multi_asset_summation_overflow_is_rejected() {
    let asset_a = AssetId(10);
    let asset_b = AssetId(11);
    let mut risk = new_risk_market(5);
    risk.configure_collateral(CollateralConfig::new(asset_a, pct(50), pct(80), 1, true).unwrap())
        .unwrap();
    risk.configure_collateral(CollateralConfig::new(asset_b, pct(50), pct(80), 1, true).unwrap())
        .unwrap();
    risk.deposit_collateral(ALICE, asset_a, 1).unwrap();
    risk.deposit_collateral(ALICE, asset_b, 1).unwrap();

    let huge = u128::MAX / 2 + 1;
    let prices = price_book(&[
        PriceQuote::new(asset_a, huge, T0, MAX_AGE).unwrap(),
        PriceQuote::new(asset_b, huge, T0, MAX_AGE).unwrap(),
        price_quote(USDC, 1, T0),
    ]);

    assert_eq!(
        risk.total_collateral_value_at(ALICE, T0, &prices),
        Err(LendingError::ArithmeticOverflow)
    );
}

// ---- risk direction regressions --------------------------------------------

#[test]
fn interest_can_turn_a_safe_account_liquidatable() {
    let mut risk = new_risk_market(25);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices = price_book(&[price_quote(SOL, 150, T0), price_quote(USDC, 1, T0)]);
    // value = 1200, cap = 840, liq_adjusted = 960. Borrow 800: within the
    // max-LTV cap, HF = 960/800 = 1.2, safe.
    risk.borrow_at(ALICE, T0, 800, &prices).unwrap();
    assert!(!risk.is_liquidatable_at(ALICE, T0, &prices).unwrap());

    // A year at 25% APR pushes debt to 800 + 200 = 1000 > 960.
    let t1 = T0 + SECONDS_PER_YEAR;
    let prices1 = price_book(&[price_quote(SOL, 150, t1), price_quote(USDC, 1, t1)]);
    assert_eq!(risk.market().debt_of_at(ALICE, t1).unwrap(), 1000);
    assert!(risk.is_liquidatable_at(ALICE, t1, &prices1).unwrap());
}

#[test]
fn collateral_price_decline_can_cause_liquidation() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices_before = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(ALICE, T0, 1000, &prices_before).unwrap();
    assert!(!risk.is_liquidatable_at(ALICE, T0, &prices_before).unwrap());

    let prices_after = price_book(&[price_quote(SOL, 100, T0), price_quote(USDC, 1, T0)]);
    // liq_adjusted = 8*100*0.8 = 640 < 1000 debt.
    assert!(risk.is_liquidatable_at(ALICE, T0, &prices_after).unwrap());
}

#[test]
fn debt_asset_appreciation_lowers_health_factor() {
    // Borrow SOL itself (as the market's debt asset) against a separate,
    // stable collateral asset, so appreciation of the *debt* side (not
    // the collateral side) is isolated.
    let stable = AssetId(50);
    let mut market = LendingMarket::new(flat_rate(5), 0, T0).unwrap();
    market.supply_at(LENDER, T0, 10_000_000).unwrap();
    let mut risk = RiskMarket::new(market, DebtAssetConfig::new(SOL, 1).unwrap());
    risk.configure_collateral(CollateralConfig::new(stable, pct(70), pct(80), 1, true).unwrap())
        .unwrap();
    risk.deposit_collateral(ALICE, stable, 1000).unwrap();

    let prices_before = price_book(&[price_quote(stable, 1, T0), price_quote(SOL, 200, T0)]);
    risk.borrow_at(ALICE, T0, 2, &prices_before).unwrap(); // 2 "SOL" of debt @ 200 = 400
    let hf_before = risk.health_factor_at(ALICE, T0, &prices_before).unwrap();

    let prices_after = price_book(&[price_quote(stable, 1, T0), price_quote(SOL, 400, T0)]);
    let hf_after = risk.health_factor_at(ALICE, T0, &prices_after).unwrap();

    assert!(hf_after.ratio().unwrap() < hf_before.ratio().unwrap());
}

#[test]
fn repayment_improves_health_factor() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(ALICE, T0, 1000, &prices).unwrap();
    let hf_before = risk.health_factor_at(ALICE, T0, &prices).unwrap();

    risk.repay_at(ALICE, T0, 200).unwrap();
    let hf_after = risk.health_factor_at(ALICE, T0, &prices).unwrap();

    assert!(hf_after.ratio().unwrap() > hf_before.ratio().unwrap());
}

#[test]
fn collateral_deposit_improves_health_factor() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(ALICE, T0, 1000, &prices).unwrap();
    let hf_before = risk.health_factor_at(ALICE, T0, &prices).unwrap();

    risk.deposit_collateral(ALICE, SOL, 4).unwrap();
    let hf_after = risk.health_factor_at(ALICE, T0, &prices).unwrap();

    assert!(hf_after.ratio().unwrap() > hf_before.ratio().unwrap());
}

// ---- preview purity and agreement -------------------------------------------

#[test]
fn preview_functions_never_mutate_state() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(ALICE, T0, 500, &prices).unwrap();

    let before = risk.clone();
    let _ = risk.preview_deposit_collateral(ALICE, SOL, 1);
    let _ = risk.preview_withdraw_collateral_at(ALICE, SOL, T0, 1, &prices);
    let _ = risk.preview_borrow_at(BOB, T0, 1, &prices);
    let _ = risk.preview_repay_at(ALICE, T0, 1);
    let _ = risk.preview_repay_all_at(ALICE, T0);
    let _ = risk.total_collateral_value_at(ALICE, T0, &prices);
    let _ = risk.health_factor_at(ALICE, T0, &prices);
    let _ = risk.liquidation_price_at(ALICE, SOL, T0, &prices);
    assert_eq!(risk, before);
}

#[test]
fn preview_and_execution_agree_for_every_risk_operation() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);

    let deposit_preview = risk.preview_deposit_collateral(ALICE, SOL, 8).unwrap();
    let deposit_actual = risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    assert_eq!(deposit_preview, deposit_actual);

    let borrow_preview = risk.preview_borrow_at(ALICE, T0, 500, &prices).unwrap();
    let borrow_actual = risk.borrow_at(ALICE, T0, 500, &prices).unwrap();
    assert_eq!(borrow_preview, borrow_actual);

    let withdraw_preview = risk
        .preview_withdraw_collateral_at(ALICE, SOL, T0, 1, &prices)
        .unwrap();
    let withdraw_actual = risk
        .withdraw_collateral_at(ALICE, SOL, T0, 1, &prices)
        .unwrap();
    assert_eq!(withdraw_preview, withdraw_actual);

    let repay_preview = risk.preview_repay_at(ALICE, T0, 100).unwrap();
    let repay_actual = risk.repay_at(ALICE, T0, 100).unwrap();
    assert_eq!(repay_preview, repay_actual);

    let repay_all_preview = risk.preview_repay_all_at(ALICE, T0).unwrap();
    let repay_all_actual = risk.repay_all_at(ALICE, T0).unwrap();
    assert_eq!(repay_all_preview, repay_all_actual);
}
