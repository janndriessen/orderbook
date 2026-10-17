//! Order book service: event source, publisher and WebSocket market-data feed.
//!
//! Events flow from the source (a stand-in for the matching engine, on its own
//! thread) through a bounded queue to the publisher, which aggregates them
//! into sequenced feed messages and broadcasts them to WebSocket clients.

mod app;
mod publisher;
mod source;
mod ws;

pub use app::run;
