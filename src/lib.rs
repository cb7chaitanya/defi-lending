//! # defi-lending
//!
//! **Educational accounting simulator — not a production lending
//! protocol.** This crate is Day 11 of Module 3 of a DeFi-from-first-
//! principles series: it models the accounting core of a single-asset
//! lending market (cash, borrows, reserves, supply/debt shares,
//! utilization, a kinked borrow-rate curve, and timestamp-based interest
//! accrual) with no floating-point arithmetic and no unchecked integer
//! operations.
//!
//! It intentionally does **not** implement collateral, prices, LTV,
//! health factors, liquidation, bad debt, leverage, flash loans,
//! governance, or any Solana account plumbing — those are out of scope
//! for this module and belong to later days. See `README.md` for the full
//! accounting model, rounding policy, and worked examples.

pub mod error;
pub mod market;
pub mod math;
pub mod rate_model;

pub use error::LendingError;
pub use market::{
    AccountId, BorrowPreview, LendingMarket, RepayPreview, SupplyPreview, WithdrawPreview,
};
pub use math::{INDEX_SCALE, RATE_SCALE, SECONDS_PER_YEAR};
pub use rate_model::InterestRateModel;
