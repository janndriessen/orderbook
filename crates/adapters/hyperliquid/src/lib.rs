//! Adapter for Hyperliquid market data.
//!
//! Hyperliquid writes prices and sizes as decimal strings; the protocol uses
//! integer ticks and lots. This crate parses the WebSocket messages and converts
//! their numbers to integers, keeps the book of the `l2` feed up to date from
//! its compressed changes, and translates snapshots and trades into protocol
//! commands.

mod decimal;
mod l2;
mod message;
mod translate;

pub use decimal::DecimalError;
pub use l2::{L2Decoder, L2Error, WireL2};
pub use message::{
    BookLevel, BookSnapshot, ConvertError, Message, Trade, TradeSide, WireBbo, WireBook, WireLevel,
    WireTrade,
};
pub use translate::{Divergence, Translator};
