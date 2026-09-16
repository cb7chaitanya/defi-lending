//! Deterministic scenario tests from the Day 11 spec.

use defi_lending::{AccountId, InterestRateModel, LendingMarket, RATE_SCALE, SECONDS_PER_YEAR};

const ALICE: AccountId = AccountId(1);
const BOB: AccountId = AccountId(2);
const CAROL: AccountId = AccountId(3);

fn pct(p: u128) -> u128 {
    RATE_SCALE * p / 100
}

/// A rate model that returns a fixed APR regardless of utilization,
/// useful for scenarios that specify an APR directly rather than a curve.
fn flat_rate(apr_pct: u128) -> InterestRateModel {
    InterestRateModel::new(pct(apr_pct), pct(50), 0, 0).unwrap()
}

const T0: u64 = 1_700_000_000;
const YEAR: u64 = SECONDS_PER_YEAR;
const QUARTER: u64 = SECONDS_PER_YEAR / 4;

/// Scenario 1: cash=600, borrows=400, no reserves.
#[test]
fn scenario_1_basic_snapshot() {
    let mut market = LendingMarket::new(flat_rate(5), 0, T0).unwrap();
    market.supply_at(ALICE, T0, 1000).unwrap();
    market.borrow_at(BOB, T0, 400).unwrap();

    assert_eq!(market.cash(), 600);
    assert_eq!(market.total_borrows_at(T0).unwrap(), 400);
    assert_eq!(market.protocol_reserves_at(T0).unwrap(), 0);
    assert_eq!(market.supplier_assets_at(T0).unwrap(), 1000);
    assert_eq!(market.utilization_at(T0).unwrap(), pct(40));
}

/// Scenario 2: withdraw / borrow / repay sequence with no elapsed time.
#[test]
fn scenario_2_capital_transitions() {
    let mut market = LendingMarket::new(flat_rate(5), 0, T0).unwrap();
    market.supply_at(ALICE, T0, 1000).unwrap();
    market.borrow_at(BOB, T0, 200).unwrap();
    assert_eq!(market.cash(), 800);
    assert_eq!(market.total_borrows_at(T0).unwrap(), 200);

    market.withdraw_at(ALICE, T0, 150).unwrap();
    market.borrow_at(BOB, T0, 250).unwrap();
    market.repay_at(BOB, T0, 100).unwrap();

    assert_eq!(market.cash(), 500);
    assert_eq!(market.total_borrows_at(T0).unwrap(), 350);
    assert_eq!(market.supplier_assets_at(T0).unwrap(), 850);
    // 350 / 850 = 0.411764705882... ; floor at RATE_SCALE (1e9) precision.
    assert_eq!(market.utilization_at(T0).unwrap(), 411_764_705);
}

/// Scenario 3: cash=800, borrows=200, 12% APR for three months.
#[test]
fn scenario_3_quarterly_accrual() {
    let mut market = LendingMarket::new(flat_rate(12), 0, T0).unwrap();
    market.supply_at(ALICE, T0, 1000).unwrap();
    market.borrow_at(BOB, T0, 200).unwrap();
    assert_eq!(market.cash(), 800);

    let t1 = T0 + QUARTER;
    assert_eq!(market.total_borrows_at(t1).unwrap(), 206);
    // Cash is untouched by interest accrual: only borrows/reserves move.
    assert_eq!(market.cash(), 800);

    let index_before = market.borrow_index_at(T0).unwrap();
    let index_after = market.borrow_index_at(t1).unwrap();
    assert!(
        index_after > index_before,
        "borrow index must grow monotonically"
    );
    assert_eq!(index_after, defi_lending::INDEX_SCALE * 206 / 200);
}

/// Scenario 3 (continued): a 50-unit proportional debt becomes 51.5 in
/// exact mathematical terms. Token amounts here are scaled 10x (so "50
/// units" is 500 raw units) purely so that 51.5 is representable as an
/// exact integer asset amount — the underlying ratio (206/200 = 1.03) is
/// identical to the unscaled market above.
#[test]
fn scenario_3_fractional_debt_scaled_by_ten() {
    let mut market = LendingMarket::new(flat_rate(12), 0, T0).unwrap();
    market.supply_at(ALICE, T0, 10_000).unwrap();
    market.borrow_at(BOB, T0, 1_500).unwrap();
    market.borrow_at(CAROL, T0, 500).unwrap(); // Carol's "50-unit" position
    assert_eq!(market.total_borrows_at(T0).unwrap(), 2_000);

    let t1 = T0 + QUARTER;
    assert_eq!(market.total_borrows_at(t1).unwrap(), 2_060); // 2000 * 1.03

    // At index growth 2060/2000 = 1.03 exactly, 500 * 1.03 = 515 exactly,
    // i.e. the "51.5" from the spec at 10x scale, with no rounding.
    assert_eq!(market.debt_of_at(CAROL, t1).unwrap(), 515);
    assert_eq!(
        market.preview_repay_all_at(CAROL, t1).unwrap().assets_in,
        515
    );
}

/// Scenario 4: cash=600, borrows=400, 20% APR, 15% reserve factor, 1 year.
#[test]
fn scenario_4_reserve_split() {
    let mut market = LendingMarket::new(flat_rate(20), pct(15), T0).unwrap();
    market.supply_at(ALICE, T0, 1000).unwrap();
    market.borrow_at(BOB, T0, 400).unwrap();
    assert_eq!(market.cash(), 600);

    let t1 = T0 + YEAR;
    let borrows_before = market.total_borrows_at(T0).unwrap();
    let borrows_after = market.total_borrows_at(t1).unwrap();
    assert_eq!(borrows_after - borrows_before, 80); // borrower interest

    assert_eq!(market.protocol_reserves_at(t1).unwrap(), 12);
    assert_eq!(borrows_after, 480);
    assert_eq!(market.supplier_assets_at(t1).unwrap(), 1068);

    let supplier_interest =
        market.supplier_assets_at(t1).unwrap() - market.supplier_assets_at(T0).unwrap();
    assert_eq!(supplier_interest, 68);
}

/// Scenario 5: supplier-share fairness across accrual and a later supplier.
#[test]
fn scenario_5_supplier_share_fairness() {
    let mut market = LendingMarket::new(flat_rate(20), 0, T0).unwrap();
    market.supply_at(ALICE, T0, 1000).unwrap();
    market.borrow_at(BOB, T0, 400).unwrap();
    assert_eq!(market.cash(), 600);

    let t1 = T0 + YEAR;
    // Interest increases Alice's claim without minting her any new shares.
    let shares_before = market.supply_shares_of(ALICE);
    let claim_before = market.claimable_assets_at(ALICE, t1).unwrap();
    assert_eq!(claim_before, 1080); // 600 + 480 - 0 reserves, all Alice's
    assert_eq!(market.supply_shares_of(ALICE), shares_before);

    // Carol supplies at the post-accrual exchange rate.
    let preview = market.preview_supply_at(CAROL, t1, 1000).unwrap();
    market.supply_at(CAROL, t1, 1000).unwrap();
    assert_eq!(market.supply_shares_of(CAROL), preview.shares_minted);
    assert!(
        preview.shares_minted < 1000,
        "a later supplier must mint fewer shares per asset than the initial 1:1 rate once value has accrued"
    );

    // Alice's claim is unaffected by Carol's later deposit: she does not
    // lose the interest she already earned, nor does Carol capture it.
    let claim_after = market.claimable_assets_at(ALICE, t1).unwrap();
    assert_eq!(claim_after, claim_before);
    assert!(market.claimable_assets_at(CAROL, t1).unwrap() <= 1000);
}

/// Scenario 6: a supplier's claim can exceed available cash.
#[test]
fn scenario_6_claim_exceeds_cash() {
    let mut market = LendingMarket::new(flat_rate(5), 0, T0).unwrap();
    market.supply_at(ALICE, T0, 1000).unwrap();
    market.borrow_at(BOB, T0, 700).unwrap();
    assert_eq!(market.cash(), 300);
    assert_eq!(market.claimable_assets_at(ALICE, T0).unwrap(), 1000);

    let before = market.clone();
    let result = market.withdraw_at(ALICE, T0, 1000);
    assert!(matches!(
        result,
        Err(defi_lending::LendingError::InsufficientCash)
    ));
    assert_eq!(
        market, before,
        "a failed withdrawal must leave state untouched"
    );

    // A withdrawal within available cash succeeds.
    market.withdraw_at(ALICE, T0, 300).unwrap();
    assert_eq!(market.cash(), 0);
}

/// Scenario 7: full repayment clears all debt shares with no dust.
#[test]
fn scenario_7_full_repayment_clears_debt() {
    let mut market = LendingMarket::new(flat_rate(7), pct(10), T0).unwrap();
    market.supply_at(ALICE, T0, 1000).unwrap();
    market.borrow_at(BOB, T0, 333).unwrap();

    let t1 = T0 + QUARTER + 12_345; // an "ugly" elapsed time to force rounding
    let owed = market.preview_repay_all_at(BOB, t1).unwrap().assets_in;
    assert!(owed >= 333);

    market.repay_all_at(BOB, t1).unwrap();
    assert_eq!(market.debt_shares_of(BOB), 0);
    assert_eq!(market.debt_of_at(BOB, t1).unwrap(), 0);
    assert!(market.debt_share_accounts().all(|(id, _)| *id != BOB));
}
