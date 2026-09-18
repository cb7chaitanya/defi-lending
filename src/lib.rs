//! # defi-lending
//!
//! **Educational accounting simulator — not a production lending
//! protocol.** This crate is Modules 3's Day 11 and Day 12 of a
//! DeFi-from-first-principles series.
//!
//! Day 11 ([`market`]) models the accounting core of a single-asset
//! lending market (cash, borrows, reserves, supply/debt shares,
//! utilization, a kinked borrow-rate curve, and timestamp-based interest
//! accrual) with no floating-point arithmetic and no unchecked integer
//! operations.
//!
//! Day 12 ([`risk`], [`collateral`], [`oracle`]) wraps that market with
//! multi-asset collateral valuation, maximum-LTV borrowing capacity,
//! liquidation-threshold-adjusted collateral, and health factors, built
//! entirely on top of Day 11's existing accrual, borrow-index, and
//! debt-share machinery — it does not duplicate or replace any of it.
//!
//! Both days intentionally do **not** implement liquidation execution,
//! liquidator bonuses, bad debt, cross-market settlement, leverage, flash
//! loans, governance, e-modes, isolation mode, supply/borrow caps, or any
//! Solana account plumbing — those are out of scope and belong to later
//! days. See `README.md` for the full accounting model, rounding policy,
//! and worked examples.

pub mod collateral;
pub mod error;
pub mod market;
pub mod math;
pub mod oracle;
pub mod rate_model;
pub mod risk;

pub use collateral::{CollateralConfig, DebtAssetConfig};
pub use error::LendingError;
pub use market::{
    AccountId, BorrowPreview, LendingMarket, RepayPreview, SupplyPreview, WithdrawPreview,
};
pub use math::{INDEX_SCALE, RATE_SCALE, SECONDS_PER_YEAR};
pub use oracle::{AssetId, PRICE_SCALE, PriceBook, PriceQuote};
pub use rate_model::InterestRateModel;
pub use risk::{
    CollateralDepositPreview, CollateralWithdrawPreview, DebtPosition, HealthFactor,
    RiskBorrowPreview, RiskMarket, health_factor_from, total_debt_value_multi,
};
