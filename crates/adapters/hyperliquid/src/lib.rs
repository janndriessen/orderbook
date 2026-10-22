//! Adapter for Hyperliquid market data.
//!
//! Hyperliquid writes prices and sizes as decimal strings; the protocol uses
//! integer ticks and lots. This crate converts between the two.

mod decimal;

pub use decimal::{DecimalError, parse_price, parse_qty};
