//! Order book and matching for one market.
//!
//! The crate depends only on `protocol` and `thiserror` (compile-time only),
//! so no async runtime or network crate can reach the matching path. Keeping
//! out other I/O, such as std `fs` and clocks, is up to review. Today it holds
//! the book alone; matching builds on it.

mod book;

pub use book::{BookError, Fill, OrderBook, PriceLevel, RestingOrder};
