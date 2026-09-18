//! Deterministic Day 12 worked scenarios from the spec: collateral
//! valuation, borrowing capacity, liquidation thresholds, and health
//! factors, wrapped around the Day 11 [`LendingMarket`].

use defi_lending::{
    AccountId, AssetId, CollateralConfig, DebtAssetConfig, DebtPosition, HealthFactor,
    InterestRateModel, LendingError, LendingMarket, PRICE_SCALE, PriceBook, PriceQuote, RATE_SCALE,
    RiskMarket, SECONDS_PER_YEAR, health_factor_from, total_debt_value_multi,
};

const SOL: AssetId = AssetId(1);
const USDC: AssetId = AssetId(0);

const LENDER: AccountId = AccountId(100);
const ALICE: AccountId = AccountId(1);
const BOB: AccountId = AccountId(2);
const CAROL: AccountId = AccountId(3);

const T0: u64 = 1_700_000_000;
const YEAR: u64 = SECONDS_PER_YEAR;
const MAX_AGE: u64 = 3600;

fn pct(p: u128) -> u128 {
    RATE_SCALE * p / 100
}

/// Whole-dollar amount, scaled by `PRICE_SCALE` — the common quote unit
/// every collateral/debt value in this crate is expressed in.
fn usd(whole: u128) -> u128 {
    whole * PRICE_SCALE
}

/// A rate model with a fixed APR regardless of utilization, so a chosen
/// elapsed time produces an exact, easily-checked amount of interest.
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

fn usdc_collateral_config(max_ltv_pct: u128, liquidation_threshold_pct: u128) -> CollateralConfig {
    CollateralConfig::new(
        USDC,
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

/// A risk market with plenty of lender liquidity and a flat borrow APR,
/// so borrowing never fails on liquidity and interest is trivial to
/// compute by hand.
fn new_risk_market(apr_pct: u128) -> RiskMarket {
    let mut market = LendingMarket::new(flat_rate(apr_pct), 0, T0).unwrap();
    market.supply_at(LENDER, T0, 10_000_000).unwrap();
    RiskMarket::new(market, DebtAssetConfig::new(USDC, 1).unwrap())
}

/// Scenario: single-collateral healthy account.
/// 8 SOL @ 200 USDC, max LTV 70%, liquidation threshold 80%, debt 800 USDC.
#[test]
fn single_collateral_healthy_account() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();

    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(ALICE, T0, 800, &prices).unwrap();

    assert_eq!(
        risk.total_collateral_value_at(ALICE, T0, &prices).unwrap(),
        usd(1600)
    );
    assert_eq!(
        risk.max_borrowing_capacity_at(ALICE, T0, &prices).unwrap(),
        usd(1120)
    );
    assert_eq!(
        risk.liquidation_adjusted_collateral_at(ALICE, T0, &prices)
            .unwrap(),
        usd(1280)
    );
    assert_eq!(risk.current_ltv_at(ALICE, T0, &prices).unwrap(), pct(50));
    assert_eq!(
        risk.health_factor_at(ALICE, T0, &prices).unwrap(),
        HealthFactor::Ratio(RATE_SCALE * 8 / 5) // 1.6
    );
    assert!(!risk.is_liquidatable_at(ALICE, T0, &prices).unwrap());
}

/// Scenario: price decline on the same position (SOL 200 -> 120).
#[test]
fn price_decline_causes_liquidation() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices_before = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(ALICE, T0, 800, &prices_before).unwrap();

    let prices = price_book(&[price_quote(SOL, 120, T0), price_quote(USDC, 1, T0)]);

    assert_eq!(
        risk.total_collateral_value_at(ALICE, T0, &prices).unwrap(),
        usd(960)
    );
    assert_eq!(
        risk.max_borrowing_capacity_at(ALICE, T0, &prices).unwrap(),
        usd(672)
    );
    assert_eq!(
        risk.liquidation_adjusted_collateral_at(ALICE, T0, &prices)
            .unwrap(),
        usd(768)
    );
    // 800 / 960 = 0.8333... ; documented fixed-point rounding is ceil.
    assert_eq!(
        risk.current_ltv_at(ALICE, T0, &prices).unwrap(),
        833_333_334
    );
    assert_eq!(
        risk.health_factor_at(ALICE, T0, &prices).unwrap(),
        HealthFactor::Ratio(RATE_SCALE * 96 / 100) // 0.96
    );
    assert!(risk.is_liquidatable_at(ALICE, T0, &prices).unwrap());
    assert_eq!(
        risk.additional_borrowing_capacity_at(ALICE, T0, &prices)
            .unwrap(),
        0
    );
}

/// Scenario: interest-driven liquidation at the same (declined) SOL
/// price. Debt grows from 800 to 840 through the market's real interest
/// accrual (a flat 5% APR over exactly one year), never by mutating any
/// private total directly.
#[test]
fn interest_driven_liquidation_and_exact_boundary_repayment() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(ALICE, SOL, 8).unwrap();
    let prices0 = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(ALICE, T0, 800, &prices0).unwrap();

    let t1 = T0 + YEAR;
    let prices = price_book(&[price_quote(SOL, 120, t1), price_quote(USDC, 1, t1)]);

    // Real interest accrual: 800 * 5% * 1yr = 40 -> 840. This is read
    // purely through `debt_of_at`/`total_debt_value_at`, not fabricated.
    assert_eq!(risk.market().debt_of_at(ALICE, t1).unwrap(), 840);
    assert_eq!(
        risk.total_debt_value_at(ALICE, t1, &prices).unwrap(),
        usd(840)
    );

    assert_eq!(
        risk.current_ltv_at(ALICE, t1, &prices).unwrap(),
        pct(875) / 10
    ); // 87.5%
    assert_eq!(
        risk.health_factor_at(ALICE, t1, &prices).unwrap(),
        HealthFactor::Ratio(914_285_714)
    );
    assert!(risk.is_liquidatable_at(ALICE, t1, &prices).unwrap());

    // In exact real-number terms, repaying 72 reaches the HF == 1
    // boundary: 840 - 72 = 768, matching liquidation-adjusted collateral
    // at this price (also 768). Under this crate's *actual*, preserved
    // partial-repayment rounding rule, though, that boundary is reached
    // one unit later than idealized real-number arithmetic would suggest
    // — see the comment below for why, and the README's Day 12 rounding
    // section for the general statement of this interaction.
    //
    // Alice's 800 debt shares were minted at index == INDEX_SCALE when
    // she borrowed at t0 (1 share == 1 asset unit), and the borrow index
    // at t1 is 840/800 (interest grew total_borrows by exactly that
    // ratio). Partial repayment burns shares via `floor(amount *
    // INDEX_SCALE / index)` (Day 11's documented "never erase more debt
    // than the assets paid" rule): `floor(72 * 800 / 840) == 68` shares,
    // leaving 732 shares, i.e. `ceil(732 * 840 / 800) == 769` — one unit
    // *above* the idealized 768, because the floor on shares-burned is
    // conservative in the borrower's disfavor (it always burns at most as
    // much share-value as was paid for, so residual debt never drops
    // below what repaying exactly covers, and can land up to one unit
    // above it). Repaying 73 instead burns 69 shares exactly, landing at
    // 731 shares and `ceil(731 * 840 / 800) == 768` — the true, exact
    // boundary. Both behaviors are asserted below, so the interaction is
    // demonstrated rather than hidden.
    let mut still_short = risk.clone();
    still_short.repay_at(ALICE, t1, 72).unwrap();
    assert_eq!(still_short.market().debt_of_at(ALICE, t1).unwrap(), 769);
    assert!(
        still_short.is_liquidatable_at(ALICE, t1, &prices).unwrap(),
        "72 alone is one unit short of the real boundary under floor-rounded partial repayment"
    );

    risk.repay_at(ALICE, t1, 73).unwrap();
    assert_eq!(risk.market().debt_of_at(ALICE, t1).unwrap(), 768);
    assert_eq!(
        risk.health_factor_at(ALICE, t1, &prices).unwrap(),
        HealthFactor::Ratio(RATE_SCALE)
    );
    assert!(!risk.is_liquidatable_at(ALICE, t1, &prices).unwrap());
}

/// Scenario: multiple collateral assets (SOL + USDC-as-collateral).
#[test]
fn multiple_collateral_assets() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.configure_collateral(usdc_collateral_config(90, 95))
        .unwrap();
    risk.deposit_collateral(BOB, SOL, 5).unwrap();
    risk.deposit_collateral(BOB, USDC, 1000).unwrap();

    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(BOB, T0, 1200, &prices).unwrap();

    assert_eq!(
        risk.total_collateral_value_at(BOB, T0, &prices).unwrap(),
        usd(2000)
    );
    assert_eq!(
        risk.max_borrowing_capacity_at(BOB, T0, &prices).unwrap(),
        usd(1600)
    );
    assert_eq!(
        risk.liquidation_adjusted_collateral_at(BOB, T0, &prices)
            .unwrap(),
        usd(1750)
    );
    assert_eq!(
        risk.health_factor_at(BOB, T0, &prices).unwrap(),
        HealthFactor::Ratio(1_458_333_333)
    );
    assert_eq!(
        risk.additional_borrowing_capacity_at(BOB, T0, &prices)
            .unwrap(),
        usd(400)
    );
    assert!(!risk.is_liquidatable_at(BOB, T0, &prices).unwrap());
}

/// Scenario: multiple debt denominations, valued through the standalone
/// multi-debt helper (the wrapped market itself remains single-borrow-
/// asset — see the README's "Multiple debt valuation" section). Uses the
/// same 1,750-USDC liquidation-adjusted collateral figure the multiple-
/// collateral-assets scenario derived, passed in directly since this
/// helper is intentionally market-independent.
#[test]
fn multiple_debt_denominations_standalone_engine() {
    let liquidation_adjusted_collateral = usd(1750);
    // 0.3 SOL, represented in milli-SOL (quantity_scale = 1000) so the
    // fractional amount is an exact integer.
    let positions = [
        DebtPosition {
            asset: USDC,
            amount: 600,
            quantity_scale: 1,
        },
        DebtPosition {
            asset: SOL,
            amount: 300,
            quantity_scale: 1000,
        },
    ];

    let prices = price_book(&[price_quote(SOL, 200, T0), price_quote(USDC, 1, T0)]);
    let total_debt = total_debt_value_multi(&positions, T0, &prices).unwrap();
    assert_eq!(total_debt, usd(660));

    let hf = health_factor_from(liquidation_adjusted_collateral, total_debt).unwrap();
    assert_eq!(hf, HealthFactor::Ratio(2_651_515_151));

    // SOL debt price rises to 400: 0.3 SOL is now worth 120, total debt 720.
    let prices_after = price_book(&[price_quote(SOL, 400, T0), price_quote(USDC, 1, T0)]);
    let total_debt_after = total_debt_value_multi(&positions, T0, &prices_after).unwrap();
    assert_eq!(total_debt_after, usd(720));
    let hf_after = health_factor_from(liquidation_adjusted_collateral, total_debt_after).unwrap();
    assert_eq!(hf_after, HealthFactor::Ratio(2_430_555_555));

    // Appreciation of the debt asset must never increase the health factor.
    assert!(matches!((hf, hf_after), (HealthFactor::Ratio(a), HealthFactor::Ratio(b)) if b < a));
}

/// Combined boundary scenario: proves the distinction between maximum
/// LTV and the liquidation threshold. Debt reaches exactly the maximum
/// borrowing capacity (via real interest accrual) while the health
/// factor is still comfortably above 1 — so a further borrow is rejected
/// purely by the LTV cap, not by any liquidation-boundary check.
#[test]
fn combined_boundary_scenario_ltv_vs_liquidation_threshold() {
    let mut risk = new_risk_market(5);
    risk.configure_collateral(sol_config(70, 80)).unwrap();
    risk.deposit_collateral(CAROL, SOL, 10).unwrap();
    let prices0 = price_book(&[price_quote(SOL, 150, T0), price_quote(USDC, 1, T0)]);
    risk.borrow_at(CAROL, T0, 1000, &prices0).unwrap();

    let t1 = T0 + YEAR;
    let prices = price_book(&[price_quote(SOL, 150, t1), price_quote(USDC, 1, t1)]);

    // 1000 * 5% * 1yr = 50 -> 1050, read purely through the market.
    assert_eq!(risk.market().debt_of_at(CAROL, t1).unwrap(), 1050);

    assert_eq!(
        risk.total_collateral_value_at(CAROL, t1, &prices).unwrap(),
        usd(1500)
    );
    assert_eq!(
        risk.max_borrowing_capacity_at(CAROL, t1, &prices).unwrap(),
        usd(1050)
    );
    assert_eq!(
        risk.liquidation_adjusted_collateral_at(CAROL, t1, &prices)
            .unwrap(),
        usd(1200)
    );
    assert_eq!(risk.current_ltv_at(CAROL, t1, &prices).unwrap(), pct(70));
    let hf = risk.health_factor_at(CAROL, t1, &prices).unwrap();
    assert_eq!(hf, HealthFactor::Ratio(1_142_857_142));
    assert!(!risk.is_liquidatable_at(CAROL, t1, &prices).unwrap());

    // Another 100 USDC borrow is rejected even though HF is above 1: the
    // max-LTV cap (1050) is already exactly met.
    assert_eq!(
        risk.additional_borrowing_capacity_at(CAROL, t1, &prices)
            .unwrap(),
        0
    );
    assert_eq!(
        risk.borrow_at(CAROL, t1, 100, &prices),
        Err(LendingError::ExceedsMaxLtv)
    );

    // The liquidation price (where HF would hit exactly 1.0) is 131.25.
    let liquidation_price = risk.liquidation_price_at(CAROL, SOL, t1, &prices).unwrap();
    assert_eq!(liquidation_price, usd(1) * 13125 / 100); // 131.25 USDC
}
