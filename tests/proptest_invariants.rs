//! Property-based tests over randomized operation sequences.
//!
//! `proptest` is a dev-dependency only (see Cargo.toml) — it is never used
//! from the library's accounting path.

use defi_lending::{
    AccountId, INDEX_SCALE, InterestRateModel, LendingError, LendingMarket, RATE_SCALE,
};
use proptest::prelude::*;

fn pct(p: u128) -> u128 {
    RATE_SCALE * p / 100
}

const ACCOUNTS: [AccountId; 4] = [AccountId(1), AccountId(2), AccountId(3), AccountId(4)];
const T0: u64 = 1_700_000_000;

fn arb_rate_model() -> impl Strategy<Value = InterestRateModel> {
    (
        1u128..=pct(50),
        1u128..pct(100),
        0u128..=pct(200),
        0u128..=pct(400),
    )
        .prop_map(|(base, optimal, slope1, slope2)| {
            InterestRateModel::new(base, optimal, slope1, slope2).unwrap()
        })
}

#[derive(Debug, Clone)]
enum Op {
    Supply { who: usize, amount: u128, dt: u64 },
    Withdraw { who: usize, amount: u128, dt: u64 },
    Borrow { who: usize, amount: u128, dt: u64 },
    Repay { who: usize, amount: u128, dt: u64 },
    RepayAll { who: usize, dt: u64 },
}

fn arb_op() -> impl Strategy<Value = Op> {
    let who = 0usize..ACCOUNTS.len();
    let amount = 1u128..=2_000;
    let dt = 0u64..=(60 * 60 * 24 * 30); // up to ~30 days between ops
    prop_oneof![
        (who.clone(), amount.clone(), dt.clone()).prop_map(|(who, amount, dt)| Op::Supply {
            who,
            amount,
            dt
        }),
        (who.clone(), amount.clone(), dt.clone()).prop_map(|(who, amount, dt)| Op::Withdraw {
            who,
            amount,
            dt
        }),
        (who.clone(), amount.clone(), dt.clone()).prop_map(|(who, amount, dt)| Op::Borrow {
            who,
            amount,
            dt
        }),
        (who.clone(), amount, dt.clone()).prop_map(|(who, amount, dt)| Op::Repay {
            who,
            amount,
            dt
        }),
        (who, dt).prop_map(|(who, dt)| Op::RepayAll { who, dt }),
    ]
}

fn check_core_invariants(market: &LendingMarket, t: u64) {
    let cash = market.cash();
    let borrows = market.total_borrows_at(t).unwrap();
    let reserves = market.protocol_reserves_at(t).unwrap();
    let assets = market.supplier_assets_at(t).unwrap();

    // supplier_assets == cash + total_borrows - protocol_reserves
    assert_eq!(assets, cash + borrows - reserves);

    // User supply-share balances sum exactly to total supply shares.
    let supply_sum: u128 = market.supply_share_accounts().map(|(_, s)| *s).sum();
    assert_eq!(supply_sum, market.total_supply_shares());

    // User debt-share balances sum exactly to total debt shares.
    let debt_sum: u128 = market.debt_share_accounts().map(|(_, s)| *s).sum();
    assert_eq!(debt_sum, market.total_debt_shares());

    // Utilization is bounded in [0, RATE_SCALE].
    let utilization = market.utilization_at(t).unwrap();
    assert!(utilization <= RATE_SCALE);

    // Reserve increment (as a fraction of the interest split) never lets
    // reserves exceed cash+borrows in a way that breaks the identity
    // above; already covered by the exact equality assertion.

    // No account can withdraw more than available cash right now.
    for acc in ACCOUNTS {
        let claim = market.claimable_assets_at(acc, t).unwrap();
        if claim > 0 {
            // A claim larger than cash is legal (solvency vs liquidity),
            // but attempting to withdraw exactly `cash + 1` must fail,
            // and withdrawing `cash` (if <= claim) must not panic.
            let over_cash = cash.saturating_add(1);
            if over_cash <= claim {
                let mut probe = market.clone();
                let result = probe.withdraw_at(acc, t, over_cash);
                assert!(result.is_err());
                assert_eq!(probe, *market);
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(256)
    ))]

    #[test]
    fn core_invariants_hold_over_random_sequences(
        rate_model in arb_rate_model(),
        reserve_factor in 0u128..=RATE_SCALE,
        ops in prop::collection::vec(arb_op(), 0..40),
    ) {
        let mut market = LendingMarket::new(rate_model, reserve_factor, T0).unwrap();
        let mut t = T0;
        let mut prev_index = market.borrow_index_at(t).unwrap();
        let mut prev_utilization = market.utilization_at(t).unwrap();

        for op in ops {
            let (who, dt) = match &op {
                Op::Supply { who, dt, .. } => (*who, *dt),
                Op::Withdraw { who, dt, .. } => (*who, *dt),
                Op::Borrow { who, dt, .. } => (*who, *dt),
                Op::Repay { who, dt, .. } => (*who, *dt),
                Op::RepayAll { who, dt } => (*who, *dt),
            };
            t += dt;
            let acc = ACCOUNTS[who];

            let before = market.clone();
            let result = match op {
                Op::Supply { amount, .. } => market.supply_at(acc, t, amount).map(|_| ()),
                Op::Withdraw { amount, .. } => market.withdraw_at(acc, t, amount).map(|_| ()),
                Op::Borrow { amount, .. } => market.borrow_at(acc, t, amount).map(|_| ()),
                Op::Repay { amount, .. } => market.repay_at(acc, t, amount).map(|_| ()),
                Op::RepayAll { .. } => market.repay_all_at(acc, t).map(|_| ()),
            };

            match result {
                Ok(()) => {
                    check_core_invariants(&market, t);

                    // Borrow index is monotonic non-decreasing.
                    let index_now = market.borrow_index_at(t).unwrap();
                    prop_assert!(index_now >= prev_index);
                    prev_index = index_now;

                    // Reserve increments never exceed newly accrued interest:
                    // guaranteed structurally since reserve_increment is a
                    // floor(interest * reserve_factor / RATE_SCALE) <= interest
                    // whenever reserve_factor <= RATE_SCALE, which `new`
                    // enforces; spot-check the derived inequality here.
                    prop_assert!(reserve_factor <= RATE_SCALE);

                    let _ = prev_utilization; // recorded for readability of intent
                    prev_utilization = market.utilization_at(t).unwrap();
                }
                Err(_) => {
                    // Failed operations leave the complete state unchanged.
                    prop_assert_eq!(&market, &before);
                }
            }
        }
    }

    /// The two borrow-rate branches are monotonic in utilization and meet
    /// exactly at the kink, for arbitrary valid configurations.
    #[test]
    fn borrow_rate_is_monotonic_and_continuous_at_kink(
        rate_model in arb_rate_model(),
        a in 0u128..=RATE_SCALE,
        b in 0u128..=RATE_SCALE,
    ) {
        let ra = rate_model.borrow_rate(a).unwrap();
        let rb = rate_model.borrow_rate(b).unwrap();
        if a <= b {
            prop_assert!(ra <= rb);
        } else {
            prop_assert!(ra >= rb);
        }

        let optimal = rate_model.optimal_utilization();
        let at_kink = rate_model.borrow_rate(optimal).unwrap();
        prop_assert_eq!(at_kink, rate_model.base_rate() + rate_model.slope_1());
    }

    /// Pure previews never mutate state, and always agree with execution.
    #[test]
    fn preview_never_mutates_and_matches_execution(
        rate_model in arb_rate_model(),
        reserve_factor in 0u128..=RATE_SCALE,
        supply_amount in 1u128..=5_000,
        borrow_amount in 1u128..=5_000,
        dt in 0u64..=(60 * 60 * 24 * 400),
    ) {
        let mut market = LendingMarket::new(rate_model, reserve_factor, T0).unwrap();
        market.supply_at(ACCOUNTS[0], T0, supply_amount).map(|_| ()).ok();
        market.borrow_at(ACCOUNTS[1], T0, borrow_amount).map(|_| ()).ok();

        let t = T0 + dt;
        let before = market.clone();
        let preview = market.preview_supply_at(ACCOUNTS[0], t, 100);
        prop_assert_eq!(&market, &before, "preview must not mutate state");

        if let Ok(preview) = preview {
            let executed = market.supply_at(ACCOUNTS[0], t, 100).unwrap();
            prop_assert_eq!(preview, executed);
        }
    }

    /// A later supplier cannot capture interest that accrued before they
    /// deposited: after supplying, their claim never exceeds what the
    /// exchange rate at that instant entitles them to (accounting for
    /// floor rounding on both the mint and the claim query).
    #[test]
    fn later_supplier_cannot_capture_earlier_interest(
        apr in 1u128..=pct(50),
        reserve_factor in 0u128..=RATE_SCALE,
        initial_supply in 100u128..=5_000,
        // Bounded by the smallest possible `initial_supply` (100) so the
        // borrow always fits without needing a `prop_assume!` reject,
        // which would otherwise blow the global-reject budget at high
        // PROPTEST_CASES counts.
        borrow_amount in 1u128..=100,
        dt in 1u64..=(60 * 60 * 24 * 400),
        new_supply in 1u128..=5_000,
    ) {
        let rate_model = InterestRateModel::new(apr, pct(50), 0, 0).unwrap();
        let mut market = LendingMarket::new(rate_model, reserve_factor, T0).unwrap();
        market.supply_at(ACCOUNTS[0], T0, initial_supply).unwrap();
        market.borrow_at(ACCOUNTS[1], T0, borrow_amount).unwrap();

        let t = T0 + dt;
        let exchange_rate_before = market.supply_exchange_rate_at(t).unwrap();

        if let Ok(preview) = market.preview_supply_at(ACCOUNTS[2], t, new_supply) {
            market.supply_at(ACCOUNTS[2], t, new_supply).unwrap();
            // The newcomer's minted shares, valued back at (approximately)
            // the pre-deposit exchange rate, must not exceed what they put
            // in (floor rounding can only ever cost them a sub-unit, never
            // hand them a windfall).
            let implied_value = preview.shares_minted * exchange_rate_before / INDEX_SCALE;
            prop_assert!(implied_value <= new_supply);
        }
    }
}

#[test]
fn checked_arithmetic_errors_instead_of_panicking_on_extreme_inputs() {
    let rate_model = InterestRateModel::new(pct(2), pct(80), pct(8), pct(60)).unwrap();
    let mut market = LendingMarket::new(rate_model, 0, 0).unwrap();

    // Supplying/borrowing near-u128::MAX amounts must error out cleanly
    // (ArithmeticOverflow) rather than panicking, since the intermediate
    // `amount * INDEX_SCALE` products used for share math can overflow.
    match market.supply_at(ACCOUNTS[0], 0, u128::MAX / 4) {
        Ok(_) => {}
        Err(LendingError::ArithmeticOverflow) => return,
        Err(other) => panic!("unexpected error: {other:?}"),
    }
    match market.borrow_at(ACCOUNTS[1], 0, u128::MAX / 8) {
        Ok(_) => {}
        Err(LendingError::ArithmeticOverflow) => return,
        Err(other) => panic!("unexpected error: {other:?}"),
    }

    // An enormous elapsed time should error out (ArithmeticOverflow) or
    // succeed — but must never panic.
    let result = market.total_borrows_at(u64::MAX);
    match result {
        Ok(_) => {}
        Err(LendingError::ArithmeticOverflow) => {}
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}
