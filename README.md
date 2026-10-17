# orderbook

An exchange-style order book service and matching engine for a single market (BTC-USD), driven by order flow derived from Hyperliquid data. A full-screen web UI visualizes the live book and a log of the events that produced it. The project mirrors how real venues are built and documents where it deliberately does not.

> Early stage: only the shared protocol crate exists so far. See [Status](#status).

## Architecture

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/architecture-dark.svg">
  <img alt="Three input sources (Hyperliquid data via an adapter, a synthetic generator, hand-written scenarios) feed a replayer, which sends commands to the order book service (gateway, matching engine, publisher). The publisher streams events to the web app and the test harness; Hyperliquid data also serves the harness as a correctness oracle." src="docs/architecture-light.svg">
</picture>

The diagram shows the target design. Principles:

- **Own protocol, adapters at the edge.** Every external source is translated into `crates/protocol` by an adapter. The engine only speaks this protocol, so swapping archived data for a live node means a new adapter and no engine change.
- **Single-threaded, deterministic engine.** One dedicated OS thread, in memory, price-time priority, no locks or database in the hot path. The same command sequence always produces the same event sequence; timestamps come from the input stream, never from a clock.
- **Integers for money.** Prices are ticks (`i64`), quantities are lots (`u64`). Decimals appear only at display time.
- **Publisher batching with snapshot and sequenced diffs.** The publisher keeps its own aggregated copy of the book and sends one combined diff every ~50 ms (design target). New clients receive a snapshot first; every message carries a sequence number, so clients detect gaps and resync from a snapshot.
- **Bounded queues only.** The engine's sole link to the networked side (Tokio, Axum) is bounded in-memory queues, and it never waits on the publisher.

## Web UI

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/ui-layout-dark.svg">
  <img alt="Web UI layout. Left: the order book is centered with asks above bids and a toggle button at the bottom right. Right: with the flow log open, the screen splits into halves, the book centered in the left half and the flow log centered in the right half." src="docs/ui-layout-light.svg">
</picture>

Desktop only, in the dense style of a professional exchange order book. The book fills the browser height; a toggle opens the flow log beside it, with filtering and pause. The feed is the source of truth for book state: the UI applies snapshots and diffs by sequence number and never derives the book from the event log.

## Status

Stack: Rust (Cargo workspace, Tokio, Axum) and React with TypeScript. Browsers receive JSON over WebSocket. TypeScript types are generated from the Rust protocol crate with [ts-rs](https://github.com/Aleph-Alpha/ts-rs).

- [x] Cargo workspace and `protocol` crate: `OrderId`, `Seq`, `Price`, `Qty`, `Ts`, `Side`, `Envelope<T>`, engine `Event`s (`Accepted`, `Trade`, `Replaced`, `Canceled`, `L2Update`) and the market-data `FeedMessage` (`Snapshot`, `Update`), with generated TypeScript types checked in CI
- [ ] Publisher, dummy random-walk source, WebSocket
- [ ] Web UI
- [ ] Matching engine and hand-written scenarios
- [ ] Fuzz generator and invariant checks
- [ ] Hyperliquid adapter and replayer
- [ ] Deployment to a small Hetzner server

## Getting started

The pinned toolchain (Rust 1.99.1, see `rust-toolchain.toml`) is installed automatically by [rustup](https://rustup.rs).

```bash
cargo test --workspace
```

This also regenerates the TypeScript types in `web/src/protocol/`. Those files are generated and committed; do not edit them by hand.

## Repository layout

```
crates/protocol/   shared message format; source of the TypeScript types
web/src/protocol/  generated TypeScript types
docs/              diagrams
```

Planned, not yet present: `crates/engine` (gateway, book, matching, publisher), `crates/adapters`, `crates/replayer`, `crates/generator`, `crates/harness`, the React app in `web/`, `scenarios/` (hand-written test cases) and `data/` (recorded sessions, gitignored).

## Trade-offs vs. production exchanges

- **Replication:** a single engine instance. Production runs several identical replicas kept in sync by a consensus protocol such as Raft, for instant failover.
- **Persistence:** no journal or snapshots; state is lost on restart. Production journals every input and snapshots the book for fast recovery.
- **Markets:** one market (BTC-USD). Production shards many markets across engines, each on a pinned CPU core.
- **Gateways:** one simple gateway with stateless checks only. Production runs many gateways with authentication, rate limits and risk checks, feeding a central sequencer.
- **Accounts and risk:** no balances, margin or pre-trade risk checks.
- **Market data:** JSON over WebSocket, batched (~50 ms). Production also offers binary feeds (e.g. SBE, multicast) for professional clients.
- **Infrastructure:** a small shared-vCPU cloud server. Production uses dedicated hardware, often co-located, with tuned networking.
