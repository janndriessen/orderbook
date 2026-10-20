//! Order book service: order flow, matcher, publisher and WebSocket market-data
//! feed.
//!
//! Commands flow from the source (a seeded random walk of order flow, on its
//! own thread) into the matcher, whose events pass through a bounded queue to
//! the publisher, which aggregates them into sequenced feed messages and
//! broadcasts them to WebSocket clients.

mod app;
mod matcher;
mod publisher;
mod source;
mod ws;

pub use app::run;
