# defi-lending

**Educational accounting simulator — not a production lending protocol.**

This crate is Day 11 of Module 3 of a DeFi-from-first-principles series. It
models the accounting core of a **single-asset lending market**: cash,
outstanding borrows, protocol reserves, supplier claims, supply shares,
scaled debt, utilization, a two-slope ("kinked") borrow-rate model, and
timestamp-based interest accrual with a reserve-factor split. Every
computation uses fixed-point integer arithmetic with checked `u128`
intermediates — there is no floating-point arithmetic anywhere in the
accounting path, and no unchecked/wrapping arithmetic.

It intentionally does **not** implement collateral, prices, LTV, health
factors, liquidation, bad debt, leverage, flash loans, governance, or any
Solana account plumbing. Those are out of scope for this module and belong
to later days in the series.

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

## Public API

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

## Errors

`LendingError` is a hand-written enum (see `src/error.rs`) covering: zero
amount, invalid rate configuration, invalid reserve factor, insufficient
cash, insufficient supply shares, insufficient debt, a result that rounds
to zero, a backwards timestamp, arithmetic overflow, and an
undefined-on-empty-market condition. It implements `Display` by hand and
`std::error::Error`; no external error-formatting crate is used.

## A bug found during testing

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

## Explicit simplifications (out of scope for Day 11)

- No collateral, prices, loan-to-value ratios, health factors, or
  liquidation — a borrower's ability to borrow here is limited only by
  the market's available cash, not by any collateral they have posted.
- No bad debt handling or socialized-loss mechanism.
- No leverage, flash loans, or multi-asset markets (this is a
  single-asset accounting core).
- No governance (the rate model and reserve factor are fixed at market
  construction).
- No Solana (or any chain's) account plumbing, serialization, or
  transaction-fee accounting — `AccountId` is a bare identifier, and
  atomicity here is a Rust-level guarantee (complete-state-before-mutate),
  analogous in spirit to (but not implemented via) a Solana transaction's
  all-or-nothing execution
  (<https://docs.solanalabs.com/runtime/transactions>).

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
PROPTEST_CASES=2000 cargo test
PROPTEST_CASES=20000 cargo test
```

`proptest` is a `dev-dependency` only — it is never used by the library's
accounting path itself. The full suite has been run repeatedly (including
at 2,000 and 20,000 property-test cases per property) with no flaky
failures observed.

## References

- Compound cToken documentation — cash, borrows, reserves, and exchange
  rate accounting: <https://docs.compound.finance/v2/ctokens/>
- Aave documentation — utilization, interest-rate strategy curves, and
  scaled/normalized debt indexes: <https://docs.aave.com/>
- Solana core documentation — atomic transaction execution:
  <https://docs.solanalabs.com/runtime/transactions>
- Rust standard library — checked integer arithmetic:
  <https://doc.rust-lang.org/std/primitive.u128.html#method.checked_mul>
