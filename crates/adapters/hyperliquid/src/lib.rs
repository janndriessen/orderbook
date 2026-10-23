//! Adapter for Hyperliquid market data.
//!
//! Hyperliquid writes prices and sizes as decimal strings; the protocol uses
//! integer ticks and lots. This crate parses the WebSocket messages and converts
//! their numbers to integers, and translates snapshots and trades into protocol
//! commands.

mod decimal;
mod message;
mod translate;

pub use decimal::DecimalError;
pub use message::{
    BookLevel, BookSnapshot, ConvertError, Message, Trade, TradeSide, WireBbo, WireBook, WireLevel,
    WireTrade,
};
pub use translate::{Divergence, Translator};
