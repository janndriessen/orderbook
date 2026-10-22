//! Order book service: order flow, matcher, publisher and WebSocket market-data
//! feed.
//!
//! Commands flow from the source (whatever order flow `run` is given, on its
//! own thread; the `orderbook` binary uses a seeded random walk) through the
//! gateway, which checks and numbers them, and a bounded queue into the
//! matcher, which runs alone on its own thread. Its events pass through a
//! second bounded queue to the publisher, which aggregates them into sequenced
//! feed messages and broadcasts them to WebSocket clients.

mod app;
mod gateway;
mod matcher;
mod publisher;
mod source;
mod ws;

pub use app::{cancel_on_ctrl_c, market, run};
pub use source::{RandomWalk, Step};
pub use tokio_util::sync::CancellationToken;
pub use ws::PING_INTERVAL;
