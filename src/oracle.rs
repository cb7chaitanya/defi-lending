//! A minimal, deterministic price-observation model for Day 12's risk
//! calculations.
//!
//! **This is not a production oracle.** It deliberately has no notion of
//! price authenticity (signatures, provider identity), confidence
//! intervals, manipulation resistance, or multi-provider aggregation —
//! every [`PriceQuote`] is trusted verbatim by the caller that supplies
//! it. Those concerns are explicitly deferred to a later module; see the
//! README's "Oracle model" section.

use std::collections::BTreeMap;

use crate::error::LendingError;

/// A fixed-point scale for asset prices, denominated in one common quote
/// unit throughout this crate (documented as "USDC-scaled": 6 decimal
/// places, matching real USDC's on-chain precision). `PRICE_SCALE` means
/// "1.0 unit of the quote currency". For example, a price of 200 USDC per
/// whole token is stored as `200 * PRICE_SCALE`.
pub const PRICE_SCALE: u128 = 1_000_000;

/// An opaque asset identifier, shared between collateral assets and the
/// market's single borrow-able ("debt") asset.
///
/// Deliberately minimal for an educational library: a bare `u32` newtype
/// with no relationship to a real token mint or on-chain metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetId(pub u32);

/// A single deterministic price observation for one asset.
///
/// `price` is a fixed-point integer scaled by [`PRICE_SCALE`]. `observed_at`
/// is the timestamp the price was captured, and `max_age_seconds` is the
/// staleness bound the quote itself carries: at an evaluation timestamp
/// `t`, the quote is usable only if `observed_at <= t` (no future-dated
/// observations) and `t - observed_at <= max_age_seconds`.
///
/// Fields are private and only constructible through [`PriceQuote::new`],
/// which rejects a zero price outright — every quote that exists is at
/// least structurally valid; staleness and future-dating are checked
/// separately at the evaluation timestamp, since that isn't known until
/// the quote is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceQuote {
    asset: AssetId,
    price: u128,
    observed_at: u64,
    max_age_seconds: u64,
}

impl PriceQuote {
    /// Builds a price quote. Rejects `price == 0` with
    /// [`LendingError::ZeroPrice`] — a zero price is never a legitimate
    /// observation in this model (an asset with no market value should
    /// simply not be priced, not priced at zero, since a zero price
    /// would make every position in it valueless without a corresponding
    /// explicit "this asset is worthless" configuration decision).
    pub fn new(
        asset: AssetId,
        price: u128,
        observed_at: u64,
        max_age_seconds: u64,
    ) -> Result<Self, LendingError> {
        if price == 0 {
            return Err(LendingError::ZeroPrice);
        }
        Ok(Self {
            asset,
            price,
            observed_at,
            max_age_seconds,
        })
    }

    /// The asset this quote prices.
    pub fn asset(&self) -> AssetId {
        self.asset
    }

    /// The quoted price, scaled by [`PRICE_SCALE`].
    pub fn price(&self) -> u128 {
        self.price
    }

    /// The timestamp this price was observed at.
    pub fn observed_at(&self) -> u64 {
        self.observed_at
    }

    /// The maximum age (in seconds) this quote remains usable for.
    pub fn max_age_seconds(&self) -> u64 {
        self.max_age_seconds
    }

    /// Validates this quote for use at `timestamp`, returning the price
    /// if valid.
    ///
    /// Rejects a future-dated observation
    /// (`observed_at > timestamp`, [`LendingError::FuturePriceObservation`])
    /// and a stale one
    /// (`timestamp - observed_at > max_age_seconds`, [`LendingError::StalePrice`]).
    /// This crate always prohibits future-dated observations: an
    /// evaluation cannot trust a price that, from its own point of view,
    /// has not happened yet.
    fn validated_price_at(&self, timestamp: u64) -> Result<u128, LendingError> {
        if self.observed_at > timestamp {
            return Err(LendingError::FuturePriceObservation);
        }
        let age = timestamp - self.observed_at;
        if age > self.max_age_seconds {
            return Err(LendingError::StalePrice);
        }
        Ok(self.price)
    }
}

/// A deterministic snapshot of the freshest known [`PriceQuote`] per
/// asset, supplied by the caller to every risk-sensitive [`crate::risk`]
/// operation. Not a live feed: it is just a map the caller populates
/// before each call, kept as a distinct type mainly so "the set of prices
/// this operation was evaluated against" is an explicit, inspectable
/// value rather than an implicit global.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriceBook {
    quotes: BTreeMap<AssetId, PriceQuote>,
}

impl PriceBook {
    /// An empty price book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records (or overwrites) the quote for `quote.asset()`.
    pub fn set(&mut self, quote: PriceQuote) {
        self.quotes.insert(quote.asset(), quote);
    }

    /// The raw stored quote for `asset`, if any, regardless of staleness.
    pub fn get(&self, asset: AssetId) -> Option<&PriceQuote> {
        self.quotes.get(&asset)
    }

    /// The validated price for `asset` at `timestamp`.
    ///
    /// Returns [`LendingError::MissingPrice`] if no quote is on file,
    /// [`LendingError::FuturePriceObservation`] or
    /// [`LendingError::StalePrice`] if the quote on file is not currently
    /// usable at `timestamp`.
    pub fn price_at(&self, asset: AssetId, timestamp: u64) -> Result<u128, LendingError> {
        let quote = self.quotes.get(&asset).ok_or(LendingError::MissingPrice)?;
        quote.validated_price_at(timestamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: AssetId = AssetId(1);

    #[test]
    fn rejects_zero_price() {
        assert_eq!(PriceQuote::new(A, 0, 0, 60), Err(LendingError::ZeroPrice));
    }

    #[test]
    fn missing_price_is_an_error() {
        let book = PriceBook::new();
        assert_eq!(book.price_at(A, 100), Err(LendingError::MissingPrice));
    }

    #[test]
    fn stale_price_is_rejected() {
        let mut book = PriceBook::new();
        book.set(PriceQuote::new(A, PRICE_SCALE, 0, 60).unwrap());
        assert_eq!(book.price_at(A, 61), Err(LendingError::StalePrice));
        assert!(book.price_at(A, 60).is_ok());
    }

    #[test]
    fn future_dated_price_is_rejected() {
        let mut book = PriceBook::new();
        book.set(PriceQuote::new(A, PRICE_SCALE, 100, 60).unwrap());
        assert_eq!(
            book.price_at(A, 99),
            Err(LendingError::FuturePriceObservation)
        );
    }
}
