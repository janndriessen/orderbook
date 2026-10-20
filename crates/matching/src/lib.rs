//! Order book and matching for one market.
//!
//! The crate depends only on `protocol` and `thiserror` (compile-time only),
//! so no async runtime or network crate can reach the matching path. Keeping
//! out other I/O, such as std `fs` and clocks, is up to review.
//!
//! It holds the order book, the matching of limit and market orders, and the
//! cancel and replace of resting orders. `Matcher::apply` runs a `Command`.
//!
//! The hand-written scenarios in the top-level `scenarios/` directory run
//! against it as part of this crate's tests.

mod book;
mod matching;

pub use book::{BookError, Fill, OrderBook, PriceLevel, RestingOrder};
pub use matching::Matcher;
