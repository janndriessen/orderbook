//! Adapter for Hyperliquid market data.
//!
//! Hyperliquid writes prices and sizes as decimal strings; the protocol uses
//! integer ticks and lots. This crate parses the WebSocket messages and converts
//! their numbers to integers; it does not build protocol commands.

mod decimal;
mod message;

pub use decimal::{DecimalError, parse_price, parse_qty};
pub use message::{
    BookLevel, BookSnapshot, ConvertError, Message, Trade, TradeSide, WireBook, WireLevel,
    WireTrade,
};
