# defi-lending

**Educational accounting simulator — not a production lending protocol.**

This crate is Days 11 and 12 of Module 3 of a DeFi-from-first-principles
series.

**Day 11** models the accounting core of a **single-asset lending
market**: cash, outstanding borrows, protocol reserves, supplier claims,
supply shares, scaled debt, utilization, a two-slope ("kinked") borrow-rate
model, and timestamp-based interest accrual with a reserve-factor split.

**Day 12** wraps that market with **collateral valuation and risk
accounting**: multi-asset collateral, oracle-priced valuation,
maximum-LTV borrowing capacity, liquidation-threshold-adjusted collateral,
health factors, and the deposit/withdraw/borrow checks that keep an
account solvent. It is built entirely on top of Day 11's own accrual,
borrow-index, and debt-share machinery — Day 12 never duplicates or
replaces any of it (see "How Day 12 reuses Day 11" below).

Every computation in both days uses fixed-point integer arithmetic with
checked `u128` intermediates — there is no floating-point arithmetic
anywhere in the accounting path, and no unchecked/wrapping arithmetic.

Both days intentionally do **not** implement liquidation *execution*,
liquidator bonuses, bad debt, cross-market settlement, leverage, flash
loans, governance, e-modes, isolation mode, supply/borrow caps, or any
Solana account plumbing. Those are out of scope for this module and belong
to later days in the series — see each day's "Explicit simplifications"
section below for the precise boundary.

## Accounting model

```text
cash              = tokens currently held by the market
total_borrows     = principal plus accrued borrower interest
protocol_reserves = the protocol-owned part of accrued interest
supplier_assets   = cash + total_borrows - protocol_reserves
```

This identity — `supplier_assets == cash + total_borrows -
protocol_reserves` — is enforced structurally: `supplier_assets` is never
stored, it is always *derived* from the other three fields, so it cannot
drift out of sync. This mirrors how Compound's cToken accounting composes
`getCash()`, `totalBorrows`, and `totalReserves`
(<https://docs.compound.finance/v2/ctokens/>).

### Utilization (and why this denominator)

```text
utilization = total_borrows / (cash + total_borrows)
```

This crate deliberately uses `cash + total_borrows` as the denominator,
**not** `cash + total_borrows - protocol_reserves` (supplier_assets). Both
choices appear in real protocols; Aave's utilization ratio, for instance,
is defined against `totalDebt / (availableLiquidity + totalDebt)`, which
is exactly this crate's convention. Compound's cToken exchange-rate math
instead treats reserves as a haircut on what's distributable to suppliers,
which is closer to computing utilization against `supplier_assets`. The
practical difference is small (reserves are usually a modest fraction of
total value) but not zero, and it changes where utilization saturates. We
picked `cash + total_borrows` because it keeps utilization bounded in
`[0, 1]` by construction with no separate clamp, and because it answers
the question "what fraction of the assets currently deployable by the
market (cash-that-could-be-lent plus what's already lent) is on loan?" —
a purely liquidity-facing question that doesn't need to know how the
interest split shook out. **This is an explicit policy choice, not a
universal standard — read your protocol's docs before assuming a
denominator.**

On an empty market (`cash == 0 && total_borrows == 0`), utilization is
defined as `0` rather than dividing by zero.

### Fixed-point representation

- `RATE_SCALE = 1_000_000_000` (1e9): the scale for rates, utilization,
  and the reserve factor. `RATE_SCALE` means 100%.
- `INDEX_SCALE = 1_000_000_000_000_000_000` (1e18): the scale for the
  cumulative borrow index. `INDEX_SCALE` means an index value of `1.0`.
- `SECONDS_PER_YEAR = 365 * 24 * 60 * 60`: the APR→per-second conversion
  convention. "Three months" / "a quarter" anywhere in this crate's tests
  means exactly `SECONDS_PER_YEAR / 4` seconds, chosen so ratio arithmetic
  stays exact instead of picking up calendar-month rounding noise.

All ratio math goes through two checked helpers in `src/math.rs`:
`mul_div_floor(a, b, denom)` and `mul_div_ceil(a, b, denom)`, both of which
compute the `a * b` product in a checked `u128` and return
`LendingError::ArithmeticOverflow` instead of panicking or wrapping (per
Rust's checked-arithmetic conventions, see
[`u128::checked_mul`](https://doc.rust-lang.org/std/primitive.u128.html#method.checked_mul)).
Every ratio comparison that can be phrased as a cross-multiplication
(e.g. "does this claim exceed this many shares' worth of assets") is
written that way in the property tests to avoid introducing rounding error
that isn't already accounted for by the mint/burn rounding policy below.

## Interest-rate model

A two-slope ("kinked") curve, in the style described in Aave's technical
documentation for its interest-rate strategies:

```text
utilization <= optimal_utilization:
    borrow_rate = base_rate + utilization / optimal_utilization * slope_1

utilization > optimal_utilization:
    borrow_rate = base_rate + slope_1
                + (utilization - optimal_utilization)
                  / (1 - optimal_utilization) * slope_2
```

Both branches evaluate to `base_rate + slope_1` exactly at
`utilization == optimal_utilization`, so the curve is continuous at the
kink (see `rate_model::tests::branches_meet_exactly_at_the_kink` and the
property test `borrow_rate_is_monotonic_and_continuous_at_kink`).
`optimal_utilization` must be strictly between 0% and 100%; at either
boundary one of the two branches divides by zero, so `InterestRateModel::new`
rejects it outright rather than special-casing it.

### Worked example

`base = 2%`, `optimal = 80%`, `slope_1 = 8%`, `slope_2 = 60%`:

| utilization | borrow rate |
|---|---|
| 0%   | 2%  |
| 40%  | 6%  |
| 80%  | 10% |
| 90%  | 40% |
| 100% | 70% |

(`rate_model::tests::worked_example_matches_spec` pins these exact
values.)

## Accrual and the borrow index

Interest for an interval with an unchanged rate is simple (non-compounding
within the interval) interest on the *opening* balance:

```text
interest = opening_borrows * borrow_apr * elapsed_seconds / SECONDS_PER_YEAR
```

The reserve factor applies only to this newly accrued interest:

```text
reserve_increment  = floor(interest * reserve_factor / RATE_SCALE)
supplier_interest  = interest - reserve_increment
```

Flooring `reserve_increment` means the protocol's cut is always rounded
down; any sub-unit remainder from that split is never taken by the
protocol, and since `total_borrows` still grows by the *full* `interest`
while `protocol_reserves` grows only by the floored share,
`supplier_assets` picks up the difference automatically. **The remainder's
documented owner is the suppliers.**

### Scaled debt: why it avoids iterating borrowers

Individual borrower balances are never stored as raw asset amounts.
Instead, each borrower holds *debt shares* (a scaled balance), and the
market tracks one cumulative `borrow_index` (scaled by `INDEX_SCALE`,
starting at `1.0`) for the whole market:

```text
current_debt = debt_shares * borrow_index / INDEX_SCALE
```

Whenever interest accrues, `borrow_index` grows by exactly the ratio
`total_borrows` grew by:

```text
borrow_index_new = borrow_index_old * (opening_borrows + interest) / opening_borrows
```

so **every** borrower's `debt_shares * borrow_index / INDEX_SCALE` grows
by the same ratio simultaneously, in one O(1) update, without the
protocol ever walking a list of accounts. This is the technique Aave
describes for its reserve normalized-variable-debt index (a single
per-reserve multiplier applied to each user's stored scaled balance) and
is the standard way to avoid an O(n)-in-borrowers accrual step.

### Accrual ordering and re-anchoring

Every timestamped, *mutating* capital operation (`supply_at`,
`withdraw_at`, `borrow_at`, `repay_at`, `repay_all_at`) first accrues the
interval since the market's `last_accrual_timestamp` up to the call's
`timestamp`, and only then prices and applies the requested operation.
`last_accrual_timestamp` is the **single canonical checkpoint** for
aggregate debt, and it is only ever advanced by one of those five mutating
calls.

Crucially, **read-only queries and previews never advance the
checkpoint.** `total_borrows_at`, `supplier_assets_at`,
`claimable_assets_at`, `debt_of_at`, every `preview_*_at` method, and so
on all *simulate* accrual up to the timestamp you pass them, purely, and
return what the state would be — without mutating `last_accrual_timestamp`
or anything else. This is what keeps arbitrary, no-op checkpoint reads
from ever changing the eventual economic outcome: you can call
`total_borrows_at` a thousand times at a thousand different timestamps
between two real operations, and the market's actual state — and the
result of the *next* mutating call — is identical to what it would have
been with zero such reads. See `tests/regression.rs`,
`read_only_checkpoints_do_not_change_final_state` and
`repeated_observation_at_same_timestamp_is_idempotent`.

**What this model does *not* give you** is frequency-independence across
*real* mutating operations. If you split one elapsed interval into two
mutating calls (e.g. two small repayments a day apart instead of one),
the second call's interest is computed on a `total_borrows` that already
includes the first call's interest, and the utilization the rate is
evaluated against can itself have shifted — so the two-call path can
accrue slightly *more* total interest than a single call spanning the
same wall-clock time would have. This is genuine discrete compounding,
not a bug: it's the same behavior real per-transaction-accrual protocols
exhibit (Compound recomputes its exchange rate on every market-touching
transaction). Exact frequency independence would require continuously
compounded interest (or accrual state that ignores intermediate
principal changes), which conflicts with "accrue simple interest on the
opening balance of each interval" as specified for this module — we
implement the latter and document the trade-off rather than hiding it
behind a tolerance. `tests/regression.rs`,
`splitting_a_real_interval_compounds_more_not_a_frequency_bug`, pins the
direction (split-path interest ≥ single-shot interest) as a regression
test.

Backwards timestamps (`timestamp < last_accrual_timestamp`) are rejected
with `LendingError::BackwardsTimestamp` on every entry point, touching
nothing.

## Supply-share accounting

```text
exchange_rate  = supplier_assets / total_supply_shares
supplier_claim = user_shares * supplier_assets / total_supply_shares
```

The very first supply mints shares one-for-one. After that, interest
accrual increases `supplier_assets` while `total_supply_shares` stays
fixed — so the exchange rate rises and every existing supplier's claim
rises with it, without any shares being minted to them. A later supplier
enters at the *current* exchange rate, so they mint fewer shares per unit
deposited than an earlier supplier did; they get back only what their
deposit is worth going forward, and cannot retroactively capture interest
that accrued before they arrived (`scenario_5_supplier_share_fairness`,
and the property test `later_supplier_cannot_capture_earlier_interest`).

## Solvency vs. liquidity

An account's supply-share claim (`claimable_assets_at`) is a **solvency**
statement — it says how much of the market's total value that account is
accounting-entitled to. It is computed from `supplier_assets`, which
includes `total_borrows` — money the market has lent out and does not
currently hold. `cash()` is a **liquidity** statement — how much is
actually sitting in the market right now, available to hand out.

A supplier can have a perfectly valid, fully-solvent claim that is larger
than `cash()`, if enough of the market's assets are on loan. `withdraw_at`
and `borrow_at` both check *both* constraints independently: a withdrawal
needs enough supply shares (solvency) *and* enough available cash
(liquidity); a borrow needs enough available cash. Either constraint can
fail on its own — `scenario_6_claim_exceeds_cash` demonstrates a claim
that is fully solvent but cannot be fully withdrawn for lack of cash,
while a partial withdrawal within `cash()` succeeds.

## Rounding policy

Every direction is documented and has a dedicated deterministic test:

| Operation | Rounds | Rationale |
|---|---|---|
| Exact-asset supply → shares minted | **down** (floor) | never mint a depositor more claim than their deposit is worth |
| Exact-asset withdrawal → shares burned | **up** (ceil) | never let a withdrawer take more value than the shares they surrender |
| Exact-asset borrow → debt shares recorded | **up** (ceil) | the borrower's scaled balance always covers at least what was disbursed |
| Partial repayment → debt shares burned | **down** (floor) | never erase more debt (in share terms) than the assets paid cover |
| Full repayment (`repay_all_at`) | amount owed **up** (ceil), shares burned = **all** | closes the position exactly, with no debt-share dust left behind |
| Reserve increment (interest split) | **down** (floor) | the protocol's cut is conservative; remainder flows to suppliers |

Any operation that is nonzero in asset terms but would round to zero
shares (or zero assets) is rejected with `LendingError::RoundsToZero`
rather than silently becoming a no-op.

Every remainder created by a rounding rule has a named owner:

- The supply/withdraw floor-vs-ceil pairing means existing suppliers are
  never diluted by a rounding-favorable withdrawal.
- The reserve-split floor means the remainder is owned by suppliers.
- `repay_all_at`'s ceil can, in a market with multiple borrowers whose
  balances each carry independent prior rounding, push the amount a given
  borrower must pay up to at most one unit past their exact share of
  `total_borrows`; since `total_borrows` cannot go negative, that
  one-unit remainder is floored away (`saturating_sub` in
  `LendingMarket::plan_repay_all`) and is owned by the remaining
  suppliers, since it stays in `cash` without a matching `total_borrows`
  entry.

Ratio comparisons that decide pass/fail (e.g. "is this claim larger than
this many shares are worth") are written as cross-multiplications in the
property tests, to compare exactly rather than introducing a second,
independent rounding step just to perform the comparison.

## Public API (Day 11)

```rust
InterestRateModel::new(base_rate, optimal_utilization, slope_1, slope_2) -> Result<Self, LendingError>
InterestRateModel::borrow_rate(utilization) -> Result<u128, LendingError>

LendingMarket::new(rate_model, reserve_factor, initial_timestamp) -> Result<Self, LendingError>
cash() -> u128
total_borrows_at(timestamp) -> Result<u128, LendingError>
protocol_reserves_at(timestamp) -> Result<u128, LendingError>
supplier_assets_at(timestamp) -> Result<u128, LendingError>
utilization_at(timestamp) -> Result<u128, LendingError>
borrow_rate_at(timestamp) -> Result<u128, LendingError>
borrow_index_at(timestamp) -> Result<u128, LendingError>
supply_exchange_rate_at(timestamp) -> Result<u128, LendingError>

supply_shares_of(user) -> u128
debt_shares_of(user) -> u128
claimable_assets_at(user, timestamp) -> Result<u128, LendingError>
debt_of_at(user, timestamp) -> Result<u128, LendingError>

preview_supply_at(user, timestamp, amount) -> Result<SupplyPreview, LendingError>
preview_withdraw_at(user, timestamp, amount) -> Result<WithdrawPreview, LendingError>
preview_borrow_at(user, timestamp, amount) -> Result<BorrowPreview, LendingError>
preview_repay_at(user, timestamp, amount) -> Result<RepayPreview, LendingError>
preview_repay_all_at(user, timestamp) -> Result<RepayPreview, LendingError>

supply_at(user, timestamp, amount) -> Result<SupplyPreview, LendingError>
withdraw_at(user, timestamp, amount) -> Result<WithdrawPreview, LendingError>
borrow_at(user, timestamp, amount) -> Result<BorrowPreview, LendingError>
repay_at(user, timestamp, amount) -> Result<RepayPreview, LendingError>
repay_all_at(user, timestamp) -> Result<RepayPreview, LendingError>
```

There are no plain (non-timestamped) wrappers: every state-changing and
every time-sensitive read requires an explicit `timestamp`, since a
"current time" default would be ambiguous for a library with no I/O of
its own.

Every mutating method and its `preview_*` counterpart are backed by the
same private, `&self`-only "planning" function, which computes the
*complete* next state (or an `Err`) before anything is mutated. This is
what guarantees, by construction rather than by convention:

- **Preview/execution agreement**: `preview_X_at(args)` and
  `X_at(args)` (called back-to-back, no intervening state change) always
  return the same value.
- **Atomic failure**: a mutating call either succeeds and applies a fully
  computed next state, or fails and leaves every field byte-for-byte
  unchanged — there is no code path that mutates part of the state and
  then returns an `Err`.

`AccountId` is a bare `u64` newtype with no relationship to any real
wallet or on-chain account layout — deliberately minimal for an
educational library; Solana account plumbing is out of scope for Day 11.

## Public API (Day 12)

```rust
CollateralConfig::new(asset, max_ltv, liquidation_threshold, quantity_scale, enabled) -> Result<Self, LendingError>
DebtAssetConfig::new(asset, quantity_scale) -> Result<Self, LendingError>

PriceQuote::new(asset, price, observed_at, max_age_seconds) -> Result<Self, LendingError>
PriceBook::new() -> Self
PriceBook::set(quote)
PriceBook::price_at(asset, timestamp) -> Result<u128, LendingError>

RiskMarket::new(market: LendingMarket, debt_asset: DebtAssetConfig) -> Self
market() -> &LendingMarket
configure_collateral(config) -> Result<(), LendingError>
collateral_balance_of(user, asset) -> u128

collateral_value_at(user, asset, timestamp, prices) -> Result<u128, LendingError>
total_collateral_value_at(user, timestamp, prices) -> Result<u128, LendingError>
max_borrowing_capacity_at(user, timestamp, prices) -> Result<u128, LendingError>
liquidation_adjusted_collateral_at(user, timestamp, prices) -> Result<u128, LendingError>
total_debt_value_at(user, timestamp, prices) -> Result<u128, LendingError>
current_ltv_at(user, timestamp, prices) -> Result<u128, LendingError>
health_factor_at(user, timestamp, prices) -> Result<HealthFactor, LendingError>
additional_borrowing_capacity_at(user, timestamp, prices) -> Result<u128, LendingError>
is_liquidatable_at(user, timestamp, prices) -> Result<bool, LendingError>
liquidation_price_at(user, target_asset, timestamp, prices) -> Result<u128, LendingError>

preview_deposit_collateral(user, asset, amount) -> Result<CollateralDepositPreview, LendingError>
deposit_collateral(user, asset, amount) -> Result<CollateralDepositPreview, LendingError>
preview_withdraw_collateral_at(user, asset, timestamp, amount, prices) -> Result<CollateralWithdrawPreview, LendingError>
withdraw_collateral_at(user, asset, timestamp, amount, prices) -> Result<CollateralWithdrawPreview, LendingError>
preview_borrow_at(user, timestamp, amount, prices) -> Result<RiskBorrowPreview, LendingError>
borrow_at(user, timestamp, amount, prices) -> Result<RiskBorrowPreview, LendingError>

repay_at(user, timestamp, amount) -> Result<RepayPreview, LendingError>       // delegates to LendingMarket, no price
repay_all_at(user, timestamp) -> Result<RepayPreview, LendingError>          // delegates to LendingMarket, no price
supply_liquidity_at(user, timestamp, amount) -> Result<SupplyPreview, LendingError>   // Day 11 supplier role, unrelated to collateral
withdraw_liquidity_at(user, timestamp, amount) -> Result<WithdrawPreview, LendingError>

// Standalone, market-independent (see "Multiple debt valuation" above):
health_factor_from(liquidation_adjusted_collateral, total_debt_value) -> Result<HealthFactor, LendingError>
total_debt_value_multi(positions: &[DebtPosition], timestamp, prices) -> Result<u128, LendingError>
```

`RiskMarket` deliberately does **not** expose a `&mut LendingMarket`
accessor: every mutation that needs to be risk-checked goes through
`RiskMarket`'s own methods, so there is no way to reach in and call the
wrapped market's `borrow_at` directly and bypass the collateral checks.
State (`collateral_configs`, `collateral_balances`) stays private, with
read-only accessors only, matching Day 11's own convention.

## Errors

`LendingError` is a single hand-written enum (see `src/error.rs`),
shared across both days, covering: zero amount, invalid rate
configuration, invalid reserve factor, insufficient cash, insufficient
supply shares, insufficient debt, a result that rounds to zero, a
backwards timestamp, arithmetic overflow, an undefined-on-empty-market
condition (Day 11); and, added for Day 12: an unknown or disabled
collateral asset, a duplicate collateral registration, an invalid
collateral configuration, insufficient collateral, exceeding maximum
LTV, a health factor that would drop below 1.0, and a missing, zero,
stale, or future-dated price. It implements `Display` by hand and
`std::error::Error`; no external error-formatting crate is used.

## A bug found during testing (Day 11)

The deterministic scenario tests (`tests/scenarios.rs`) caught a real
accounting bug during development: `borrow_at`, `repay_at`, and
`repay_all_at` correctly computed the principal-adjusted `total_borrows`
in their `*Preview` return value (via the shared `plan_*` function), and
correctly applied the *interest-accrual* part of the state update via
`apply_accrual`, but the mutating methods never actually copied
`preview.total_borrows_after` back into `self.total_borrows`. The
accrual snapshot updated `total_borrows` for the interest that had accrued
up to the call's timestamp, but the borrow/repay principal delta on top
of that snapshot was silently dropped.

The symptom was exact and immediate: `scenario_1_basic_snapshot` (`cash =
600, borrows = 400`) failed with `total_borrows_at` returning `0`
immediately after a `borrow_at(BOB, T0, 400)` call, because `self.cash`
had been debited correctly but `self.total_borrows` still held its
pre-borrow value. All seven deterministic scenarios failed with the same
shape of error before the fix (`supply_at`/`withdraw_at`, which don't
touch `total_borrows`, were unaffected). The fix was one line added to
each of the three affected methods:

```rust
self.total_borrows = preview.total_borrows_after;
```

`tests/regression.rs` does not have a test named specifically after this
bug, since it is fully covered by the ordinary deterministic scenarios
(every scenario that borrows or repays anything exercises this code
path) and by `preview_and_execution_agree`, which would fail immediately
if a mutating method's final state ever diverged from its own preview's
projected `total_borrows_after` again.

## Explicit simplifications (Day 11, out of scope on its own)

Taken entirely on its own — without the Day 12 layer described below —
the accounting in this section has no notion of:

- Collateral, prices, loan-to-value ratios, health factors, or
  liquidation — within *this* module, a borrower's ability to borrow is
  limited only by the market's available cash, not by any collateral
  they have posted. (Day 12, below, adds exactly this on top, without
  changing anything described above.)
- Bad debt handling or a socialized-loss mechanism.
- Leverage, flash loans, or multiple simultaneously borrow-able assets
  (this module's `LendingMarket` is a single-asset accounting core; see
  the Day 12 section for how multi-*collateral*, single-*debt-asset*
  differs from true multi-asset borrowing).
- Governance (the rate model and reserve factor are fixed at market
  construction).
- Solana (or any chain's) account plumbing, serialization, or
  transaction-fee accounting — `AccountId` is a bare identifier, and
  atomicity here is a Rust-level guarantee (complete-state-before-mutate),
  analogous in spirit to (but not implemented via) a Solana transaction's
  all-or-nothing execution
  (<https://docs.solanalabs.com/runtime/transactions>).

---

# Day 12: collateral, risk, and health factors

Day 12 answers the question Day 11 deliberately left open: *given a
market that will lend out its single asset to anyone with enough cash
available, what should actually gate who's allowed to borrow, and how
much?* It adds collateral valuation, borrowing-capacity limits, and
liquidation thresholds — the risk layer — without touching a single line
of Day 11's own accounting.

## How Day 12 reuses Day 11

[`risk::RiskMarket`](src/risk.rs) *owns* a Day 11 [`LendingMarket`] and
adds, beside it, only:

- per-asset collateral configuration (`max_ltv`, `liquidation_threshold`,
  a quantity-scaling convention, an enabled flag);
- per-user, per-asset collateral balances.

It does **not** re-implement, shadow, or duplicate interest accrual, the
borrow index, debt-share accounting, supply-share accounting, cash/total-
borrow bookkeeping, protocol reserves, or the kinked rate model — every
figure Day 12 needs from the market (a user's current debt, the market's
available cash, the accrued borrow index) is obtained by calling straight
into the existing Day 11 `LendingMarket` API (`debt_of_at`,
`preview_borrow_at`, `borrow_at`, ...), never by reading or writing a
second, parallel copy of that state.

Concretely, a risk-checked borrow:

1. Calls `LendingMarket::preview_borrow_at` — Day 11's own *pure*
   preview — to get the complete proposed post-borrow market state
   (cash, total borrows, debt shares, borrow index) without mutating
   anything.
2. Prices that proposed debt and the account's current collateral, and
   checks the maximum-LTV constraint described below.
3. Only if that check passes, calls `LendingMarket::borrow_at` with the
   *same arguments* used for step 1 — the identical, deterministic
   computation, now committed.

So a successful collateral-aware borrow produces **exactly** the
market-accounting result the plain Day 11 borrow path would have
produced on its own, plus the (separate) collateral bookkeeping Day 12
owns. If the risk check in step 2 fails, step 3 never runs, and the
market is untouched — see "Atomic plan-then-commit execution" below.

## Asset and price representation

- [`oracle::AssetId`](src/oracle.rs) is a bare `u32` newtype, shared
  between collateral assets and the market's single debt asset —
  deliberately minimal, with no relationship to a real token mint.
- Prices ([`oracle::PriceQuote`]) are fixed-point integers scaled by
  [`oracle::PRICE_SCALE`] (`1_000_000`, i.e. 6 decimal places),
  denominated in **one common quote unit** documented throughout as
  "USDC-scaled" — matching real USDC's on-chain precision, though this
  crate doesn't model USDC itself. `PRICE_SCALE` means "1.0 unit of the
  quote currency": a price of 200 USDC per token is stored as
  `200 * PRICE_SCALE`.
- [`collateral::CollateralConfig`] and
  [`collateral::DebtAssetConfig`] both carry a `quantity_scale`: the
  number of smallest on-chain units equal to one whole unit of that
  asset (e.g. `1_000_000_000` for a 9-decimal token) — an explicit
  quantity-scaling convention that plays the same role a token decimals
  count would, without needing a separate checked power-of-ten helper.
  All of this crate's worked examples use `quantity_scale = 1` (whole
  units) for clarity; `tests/risk_scenarios.rs`'s
  `multiple_debt_denominations_standalone_engine` test exercises a
  non-trivial scale (representing 0.3 SOL in milli-SOL units) to prove
  fractional quantities value correctly.

**Every collateral parameter is a per-asset policy choice, not a
universal protocol constant.** Real lending protocols set `max_ltv` and
`liquidation_threshold` independently for each asset based on that
asset's liquidity and volatility (see Aave's per-reserve risk parameters
in its official documentation); this crate models that directly —
there is no single, market-wide risk parameter anywhere in `risk.rs`.

## Collateral value, maximum LTV, and liquidation threshold

```text
collateral_value_i              = collateral_balance_i * price_i / quantity_scale_i
max_borrowing_capacity           = sum(collateral_value_i * max_ltv_i)
liquidation_adjusted_collateral  = sum(collateral_value_i * liquidation_threshold_i)
total_debt_value                 = debt_amount * debt_price / debt_quantity_scale
current_ltv                      = total_debt_value / total_collateral_value
health_factor                    = liquidation_adjusted_collateral / total_debt_value
```

**Why maximum LTV and the liquidation threshold are different
numbers.** `max_ltv` gates how much *new* debt an account may take on
right now; `liquidation_threshold` gates how much debt an *existing*
position may carry before it becomes eligible for liquidation. Every
configured collateral asset enforces `max_ltv <= liquidation_threshold`
(validated in `CollateralConfig::new`), which leaves deliberate headroom
between "the most you're allowed to borrow today" and "the point at which
you'd actually be liquidated" — the gap is the cushion that protects a
position from being pushed straight to the liquidation boundary by
ordinary price movement or accruing interest the moment it's opened. The
combined-boundary worked example below demonstrates this concretely: an
account can be turned away for a further borrow by the `max_ltv` cap
while its health factor is still comfortably above 1.

This mirrors the distinction Aave's official documentation draws between
an asset's `LTV` and `liquidationThreshold` reserve parameters.

## Health factor

```text
health_factor = liquidation_adjusted_collateral / total_debt_value
```

Expressed as [`risk::HealthFactor`] rather than a bare, possibly-infinite
float:

- `HealthFactor::NoDebt` — no outstanding debt; never liquidatable,
  regardless of collateral. This avoids ever representing "infinite
  health" as a floating-point value or a magic sentinel integer.
- `HealthFactor::Ratio(hf)` — a finite ratio scaled by `RATE_SCALE`
  (`RATE_SCALE` means `HF == 1.0`).

**Boundary semantics, stated once and applied everywhere:** `HF > 1` is
safe. `HF < 1` is liquidatable. **`HF == 1` is the exact liquidation
boundary and is treated as safe, not liquidatable** —
`HealthFactor::is_liquidatable` returns `false` for `hf == RATE_SCALE`,
every test that reaches the exact boundary asserts *not* liquidatable
there (see `interest_driven_liquidation_and_exact_boundary_repayment`),
and this doc is the single place that statement is made so code, tests,
and prose can never quietly disagree with each other. This matches
Aave's own definition, where liquidation is triggered once the health
factor drops *below* 1.

## Price-driven and interest-driven liquidation

An account's health factor can fall below 1 two structurally different
ways, both demonstrated by dedicated tests:

- **Collateral price decline**: `liquidation_adjusted_collateral` falls
  while `total_debt_value` stays fixed (`price_decline_causes_liquidation`,
  `collateral_price_decline_can_cause_liquidation`).
- **Interest accrual**: `total_debt_value` grows while collateral value
  stays fixed, purely through Day 11's own existing interest/debt-share
  machinery — `debt_of_at` simulating accrual up to the query timestamp,
  exactly as it does with no collateral involved at all
  (`interest_driven_liquidation_and_exact_boundary_repayment`,
  `interest_can_turn_a_safe_account_liquidatable`). No test fabricates
  accrued debt by mutating a private total directly; every accrued-debt
  figure in every Day 12 test is read through the market's own pure
  accrual simulation.
- A third, related case — **debt-asset appreciation** — lowers the
  health factor by raising `total_debt_value` through the *price* of the
  debt asset rather than through interest
  (`debt_asset_appreciation_lowers_health_factor`).

## Multiple collateral assets

`total_collateral_value_at`, `max_borrowing_capacity_at`, and
`liquidation_adjusted_collateral_at` all sum a per-asset term over every
asset an account holds a nonzero balance of, each term rounded
independently (see "Conservative rounding" below) before the checked
sum. An asset the account has never deposited contributes nothing and
requires no price lookup at all — see "Stale, missing, and future-dated
prices" below for why that matters.

## Multiple debt valuation, and this crate's single-market limitation

The wrapped `LendingMarket` remains **single-borrow-asset**, exactly as
in Day 11 — `RiskMarket` does not bolt a second, independently
interest-accruing debt market on top of it, since doing so would mean
maintaining a second copy of Day 11's own borrow-index/debt-share
machinery for no benefit to this module's own economic model, and would
directly contradict "reuse Day 11, never duplicate it."

Where a worked example nonetheless calls for multiple, differently
denominated debts (e.g. some USDC debt and some SOL debt on the same
account), this crate provides a **standalone, market-independent**
valuation helper instead of pretending the wrapped market supports it:
[`risk::DebtPosition`] and [`risk::total_debt_value_multi`] sum the
quote-unit value of an arbitrary slice of named debt positions, each
rounded up individually before a checked sum — exactly the same
per-term-then-sum discipline the collateral totals use, just without a
live market behind it. [`risk::health_factor_from`] is the same pure
function `RiskMarket::health_factor_at` itself is built on, so the wired
and standalone paths share one implementation rather than two
independently-written ones that could drift apart.
`multiple_debt_denominations_standalone_engine` tests this helper
directly. **This is a documented design boundary, not a workaround**: if
your application needs a single account to hold live, accruing debt in
more than one asset simultaneously, that requires a genuinely different
market architecture than the single-asset `LendingMarket` this series
has built up through Day 11, and is out of scope here.

## Conservative rounding

The rule, applied consistently: **never overstate collateral, never
understate debt.**

| Quantity | Rounds | Rationale |
|---|---|---|
| Collateral value (per asset) | **down** (floor) | never overstate what backs a loan |
| Maximum borrowing capacity (per-asset term) | **down** (floor) | never overstate how much an account can borrow |
| Liquidation-adjusted collateral (per-asset term) | **down** (floor) | never overstate the safety cushion behind an account's debt |
| Total debt value | **up** (ceil) | never understate debt — and reuses Day 11's own already-ceiled `debt_of_at` amount, so both the debt-share-to-asset and the asset-to-quote-unit conversions round the same direction |
| Current LTV (display only) | **up** (ceil) | a risk-exposure figure follows the debt side's rounding convention, not the collateral side's |
| Health factor | **down** (floor) | both its inputs are already conservative, and the final division rounds down again, so the reported HF never overstates safety |
| Additional borrowing capacity | **saturates at 0** via `saturating_sub`, never unchecked subtraction | `max_borrowing_capacity` can be below `total_debt_value` (e.g. after a price decline); reporting a negative number isn't meaningful, and unchecked subtraction would panic or wrap |
| Liquidation price (informational) | **up** (ceil) | the returned price is a conservative "at or above this, the account is still safe on this asset alone" bound — rounding it down could report a price that looks safe but has already crossed the boundary once truncated |

Every per-asset term (collateral value, its max-LTV contribution, its
liquidation-threshold contribution) is rounded *individually*, before
the checked sum across assets — bounding the rounding loss per asset
rather than letting it compound across a multi-asset sum. Property test
10 (`rounding_never_overstates_collateral_or_understates_debt`) proves
both directions directly against the unrounded cross-multiplied
inequality (`value * quantity_scale <= balance * price` and
`debt_value * quantity_scale >= debt_amount * price`), not an empirically
chosen tolerance.

## Stale, missing, and future-dated prices

[`oracle::PriceBook`] is a deterministic snapshot the caller supplies to
every risk-sensitive call — not a live feed. This is explicitly **not a
production oracle**: it has no notion of price authenticity (signing,
provider identity), confidence intervals, manipulation resistance, or
multi-provider aggregation. Those are deferred to a later, dedicated
oracle module; see `src/oracle.rs`'s module docs.

A price is rejected if:

- **missing** — no quote on file for that asset (`LendingError::MissingPrice`);
- **zero** — rejected at construction (`PriceQuote::new`,
  `LendingError::ZeroPrice`) — a zero price is never a legitimate
  observation in this model, only ever a sign of a missing feed;
- **future-dated** — `observed_at > evaluation_timestamp`
  (`LendingError::FuturePriceObservation`) — this crate **always**
  prohibits evaluating a price from the future, since an evaluation can't
  trust a price that, from its own point of view, hasn't happened yet;
- **stale** — `evaluation_timestamp - observed_at > max_age_seconds`
  (`LendingError::StalePrice`), where `max_age_seconds` travels with the
  quote itself.

### Risk-increasing vs. risk-reducing operations

**Every operation that can increase an account's risk requires valid,
current prices for every asset the check depends on:**

- `borrow_at` — needs the debt asset's price and every held collateral
  asset's price.
- `withdraw_collateral_at` — needs every held collateral asset's price
  (the position's safety margin can only ever shrink from a withdrawal).

**Risk-reducing operations never require a price, and keep working even
with no oracle available at all:**

- `deposit_collateral` — takes **no** `PriceBook` parameter at all.
  Custodying more collateral can only ever help an account's safety
  margin, never hurt it, so there is nothing to validate against a
  price.
- `repay_at` / `repay_all_at` — delegate straight to Day 11's own
  `LendingMarket::repay_at` / `repay_all_at`, unchanged, with no price
  check. A borrower must always be able to pay down debt, including
  during an oracle outage.

`price_failure_blocks_borrow_and_withdrawal_but_not_repayment_or_deposit`
tests exactly this split with an empty `PriceBook`. The design principle
this all follows: **a missing price must never manufacture new borrowing
power**, but it must also never trap a user who is trying to make their
position safer.

## Atomic plan-then-commit execution

Every Day 12 mutating method (`deposit_collateral`,
`withdraw_collateral_at`, `borrow_at`) is backed by a private,
`&self`-only "plan" function that computes the *complete* next state —
or an `Err` — before anything is mutated, exactly mirroring Day 11's own
discipline:

- `withdraw_collateral_at` recomputes the account's total collateral
  value, maximum borrowing capacity, liquidation-adjusted collateral,
  and health factor **as they would be after** the withdrawal (via a
  hypothetical-balance override that never mutates `self`), and only
  commits the balance change once every check on that *proposed* state
  has passed.
- `borrow_at` computes the market's complete proposed post-borrow state
  via Day 11's own pure `preview_borrow_at`, prices the proposed debt,
  checks it against the account's (unaffected-by-borrowing) collateral,
  and only then commits — through Day 11's own `borrow_at`, not a
  re-implementation of it.

A failed operation therefore never touches collateral balances, debt
shares, cash, totals, indexes, the accrual checkpoint, reserves, or the
price book it was passed — `failed_operations_leave_state_unchanged`
(both a direct test and property 8) checks every mutating entry point
this way.

## Worked examples

All five scenarios below are deterministic tests in
`tests/risk_scenarios.rs`, run against a real `RiskMarket` (interest
accrual included, via Day 11's own machinery — nothing is fabricated).

**Single-collateral healthy account** — 8 SOL @ 200 USDC, max LTV 70%,
liquidation threshold 80%, debt 800 USDC: collateral value 1,600, max
borrowing capacity 1,120, liquidation-adjusted collateral 1,280, current
LTV 50%, health factor 1.6, not liquidatable.

**Price decline** — the same position, SOL now 120 USDC: collateral
value 960, max borrowing capacity 672, liquidation-adjusted collateral
768, current LTV 83.333...% (ceiled), health factor 0.96, liquidatable,
additional borrowing capacity 0.

**Interest-driven liquidation** — same declined price, debt grows from
800 to 840 through one real year of 5% APR interest (read purely via
`debt_of_at`): current LTV 87.5%, health factor ≈0.914285714,
still liquidatable. Repaying 73 lands exactly on the `HF == 1` boundary
— see the note below on why the exact figure is 73, not the 72 a purely
real-number calculation would suggest.

**Multiple collateral assets** — 5 SOL @ 200 (max LTV 70%, liquidation
threshold 80%) plus 1,000 USDC-as-collateral (max LTV 90%, liquidation
threshold 95%), debt 1,200: total collateral value 2,000, max borrowing
capacity 1,600, liquidation-adjusted collateral 1,750, health factor
1.458333... (floored), additional capacity 400, safe.

**Combined boundary scenario** (LTV vs. liquidation threshold) — 10 SOL
@ 150, max LTV 70%, liquidation threshold 80%, debt after one real year
of 5% APR interest is exactly 1,050: collateral value 1,500, max
borrowing capacity 1,050 (== debt, the exact LTV boundary),
liquidation-adjusted collateral 1,200, current LTV 70%, health factor
1.142857... — safely above 1. A further 100 USDC borrow is nonetheless
**rejected**, purely by the max-LTV cap, proving the LTV constraint binds
independently of (and earlier than) the liquidation-threshold-based
health factor. The liquidation price for SOL — the price at which HF
would hit exactly 1.0, everything else held fixed — is exactly 131.25
USDC.

### A rounding interaction found while building the interest-driven-liquidation test

The spec-style scenario above expects that repaying `840 - 768 = 72`
lands exactly on the `HF == 1` boundary (768 also being the exact
liquidation-adjusted collateral figure at that price). Under **real,
unrounded** arithmetic it does. Under this crate's actual, *preserved*
partial-repayment rounding rule (Day 11's `repay_at` burns
`floor(amount * INDEX_SCALE / borrow_index)` debt shares — "never erase
more debt than the assets paid," see the Day 11 rounding-policy table
above), it does not: Alice's 800 debt shares (minted 1:1 at borrow time)
and a borrow index of `840/800` mean repaying exactly 72 burns
`floor(72 * 800 / 840) = 68` shares, leaving `ceil(732 * 840 / 800) =
769` — one unit **above** the idealized 768, because the floor on
shares-burned is conservative in the borrower's favor: it never lets a
partial repayment erase more debt-share value than was actually paid
for, which means the residual debt can land up to one unit above what a
real-number calculation would predict. Repaying 73 instead burns 69
shares exactly, landing at 731 shares and `ceil(731 * 840 / 800) = 768`
— the true, exact boundary.

Both behaviors are asserted directly in
`interest_driven_liquidation_and_exact_boundary_repayment` (repaying 72
alone leaves the account still liquidatable by a hair; repaying 73
reaches the exact boundary), so this interaction is demonstrated and
regression-tested rather than quietly worked around. It is a direct,
provable consequence of Day 11's own documented partial-repayment
rounding rule — which this module is required to reuse unchanged, not
loosen to make a worked example land on a rounder number.

## Explicit simplifications (Day 12) and Day 13+ scope

Day 12 does **not** implement:

- **Liquidation execution** — this module tells you *whether* an account
  is liquidatable (`is_liquidatable_at`) and *how far* from the boundary
  it is (`health_factor_at`), but performs no seizure of collateral, no
  debt write-down, and defines no liquidator-facing entry point at all.
- **Liquidator bonuses / close factors** — no economic incentive
  structure for a third party to perform a liquidation exists yet.
- **Bad debt** — an account whose collateral value has collapsed below
  its debt value is reported as unhealthy (a very low or zero health
  factor), but nothing here writes off debt or socializes a loss.
- **Cross-market settlement** — this remains one market with one debt
  asset; there is no mechanism for collateral or debt to move between
  independent markets.
- **Leverage or flash loans.**
- **Governance** — collateral configuration is set once via
  `configure_collateral` and cannot be changed or voted on afterward.
- **E-mode / isolation mode** — no correlated-asset category that relaxes
  LTV limits, and no isolated-collateral mode that caps borrowable value
  independent of the normal LTV math.
- **Supply or borrow caps** — no market-wide or per-asset ceiling on
  total collateral or total debt.
- **Solana (or any chain's) account plumbing** — unchanged from Day 11:
  `AccountId` and `AssetId` remain bare identifiers with no relationship
  to a real wallet, token mint, or on-chain account layout.

These are exactly the boundaries later modules in the series are expected
to fill in.

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
PROPTEST_CASES=2000 cargo test
PROPTEST_CASES=20000 cargo test
```

`proptest` is a `dev-dependency` only — it is never used by the library's
accounting path itself. Day 12 added three test files
(`tests/risk_scenarios.rs`, `tests/risk_adversarial.rs`,
`tests/risk_proptest.rs`) alongside Day 11's existing ones; none of
Day 11's original tests were modified. The full suite has been run
repeatedly (including at 2,000 and 20,000 property-test cases per
property, across both days' proptest files) with no flaky failures
observed.

## References

- Compound cToken documentation — cash, borrows, reserves, and exchange
  rate accounting: <https://docs.compound.finance/v2/ctokens/>
- Aave documentation — utilization, interest-rate strategy curves,
  scaled/normalized debt indexes, and per-reserve risk parameters
  (loan-to-value, liquidation threshold) and the health factor:
  <https://docs.aave.com/>
- Solana core documentation — atomic transaction execution:
  <https://docs.solanalabs.com/runtime/transactions>
- Rust standard library — checked integer arithmetic:
  <https://doc.rust-lang.org/std/primitive.u128.html#method.checked_mul>
