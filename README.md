# orderbook

An exchange-style order book service and matching engine for a single market (BTC-USD), driven by order flow derived from Hyperliquid data. A full-screen web UI visualizes the live book and a log of the events that produced it. The project mirrors how real venues are built and documents where it deliberately does not.

Today it rebuilds Hyperliquid's live BTC book order by order through this engine (gateway, deterministic matcher, publisher) and serves it to the web UI. Recordings can be replayed, and a random-walk order flow runs without the network.

## Architecture

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/architecture-dark.svg">
  <img alt="Hyperliquid data (recordings or a live feed) passes through the Hyperliquid adapter to the replayer, which feeds the order book service: a gateway, a matching engine and a publisher, linked by bounded in-memory queues. A built-in random walk also feeds the gateway. The publisher streams to the web app. Separately, a generator (seeded fuzz flow) and scenarios (hand-written cases) feed a test harness that works against the matching engine, with Hyperliquid L2 books as a correctness oracle." src="docs/architecture-light.svg">
</picture>

Principles:

- **Own protocol, adapters at the edge.** Every external source is translated into `crates/protocol` by an adapter. The engine only speaks this protocol, so swapping archived data for a live node means a new adapter and no engine change.
- **Single-threaded, deterministic engine.** One dedicated OS thread, in memory, price-time priority, no locks or database in the hot path beyond the synchronization inside its bounded queues. The same command sequence always produces the same event sequence; timestamps come from the input stream, never from a clock.
- **Integers for money.** Prices are ticks (`i64`), quantities are lots (`u64`). Decimals appear only at display time.
- **Publisher batching with snapshot and sequenced diffs.** The publisher keeps its own aggregated copy of the book and sends one combined diff every 50 ms (only when something changed), or sooner when a burst reaches 2,000 events; an update never contains part of a command's events. New clients receive a snapshot first, and every message carries a sequence number, so clients detect gaps and resync from a snapshot. After about a second of silence the publisher sends a heartbeat that repeats the latest sequence number; a client that hears nothing for 3.5 seconds, or gets no snapshot within 10 seconds of connecting, reconnects. Limits for clients: see [Trade-offs](#trade-offs-vs-production-exchanges).
- **Bounded queues only.** The engine's sole link to the networked side (Tokio, Axum) is bounded in-memory queues. Its output queue holds one item per command, all of that command's events together; when it is full the engine waits for the publisher to catch up (backpressure) and never drops an event.
- **Throughput log.** Every 10 seconds the publisher logs one `info` line (`publisher throughput`) with update, event and byte counts and the largest lag; see [Performance](#performance).

## Web UI

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/ui-layout-dark.svg">
  <img alt="Web UI layout. The order book is centered with asks above bids and a price-grouping menu at its top right. With the flow log open, the screen splits in halves: book left, log right. In a narrow window the log stacks below the book, with no toggle." src="docs/ui-layout-light.svg">
</picture>

Desktop browsers and phones, in the dense style of a professional exchange order book.

The book shows up to 20 price levels per side around a spread bar with the spread and the last trade. Numbers and depth bars update instantly, and a new price level flashes briefly in its side color. The "Group prices" menu sums levels into wider price buckets.

The flow log lists the order events behind the book, with filters and pause. On wide screens a toggle opens it beside the book; in narrower windows and on phones it sits below the book, with shorter rows on phones.

The status line shows whether the feed is live. "Halted · source reconnecting" means the service has lost its connection to the source and accepts only cancels until it is back.

The UI applies the feed's snapshot and sequenced diffs and never derives the book from the event log.

## Stack

Rust (Cargo workspace, Tokio, Axum) and React with TypeScript. Browsers receive JSON over WebSocket. TypeScript types are generated from the Rust protocol crate with [ts-rs](https://github.com/Aleph-Alpha/ts-rs).

## Getting started

The pinned toolchain (Rust 1.99.1, see `rust-toolchain.toml`) is installed automatically by [rustup](https://rustup.rs). The web UI needs Node 24 (see `web/.nvmrc`) and pnpm.

Serve Hyperliquid's live BTC book through the engine and open the UI:

```bash
cargo run --release -p replayer --bin live   # live feed at ws://127.0.0.1:8080/ws, until Ctrl-C
pnpm -C web install
pnpm -C web dev                              # http://localhost:5173, /ws is proxied to the feed server
```

Other binaries and checks:

```bash
cargo run -p engine --bin orderbook          # random-walk order flow, same feed, no network needed
REPLAY_PATH=data/<recording>.jsonl cargo run -p replayer --bin replay   # serves a recording through the engine
REPLAY_SPEED=step REPLAY_PATH=data/<recording>.jsonl cargo run -p replayer --bin replay   # Enter plays one recorded moment
cargo run -p hyperliquid-recorder --bin record   # records the live BTC book, best bid and offer, and trades to data/, until Ctrl-C
cargo run --release -p harness --bin bench   # matcher throughput and cancel cost; numbers depend on the machine
REPLAY_PATH=data/<recording>.jsonl cargo run --release -p replayer --bin levels   # level lengths and cancel cost on a recording
cargo test --workspace                       # also regenerates the TypeScript types in web/src/protocol/ and checks a replayed recording against the source's books
```

The generated TypeScript files are committed; do not edit them by hand.

| Variable                | Default          | Meaning                                                      |
|-------------------------|------------------|--------------------------------------------------------------|
| `ORDERBOOK_ADDR`        | `127.0.0.1:8080` | Listen address                                               |
| `ORDERBOOK_MAX_CLIENTS` | `500`            | Cap on concurrent feed clients, for all three binaries; a client beyond it is refused with HTTP 503 before the WebSocket upgrade. Zero, above 100,000 or a non-number stops the start with an error |
| `ORDERBOOK_SEED`        | clock-derived    | Seed of the random-walk order flow; the same seed gives the same commands and events; their timestamps start at launch time |
| `RUST_LOG`              | `info`           | Log filter                                                   |
| `REPLAY_PATH`           | required         | Recording that `replay` serves, as written by `record` |
| `REPLAY_SPEED`          | `1`              | Playback speed of `replay`: `max`, `step` (Enter in the terminal plays one recorded moment: all commands that share a time), or a positive integer factor (`1` is real time) |
| `RECORD_PATH`           | `data/hyperliquid-btc-<unix seconds>.jsonl` | Output file of `record`; its directory is created if missing, and an existing file is never overwritten |
| `RECORD_SECONDS`        | run until Ctrl-C | Makes `record` stop after this many seconds; must be above zero |
| `LIVE_RECORD_PATH`      | not set          | Makes `live` also write every line it receives to this file, in the format `record` writes and `replay` serves; its directory is created if missing, and an existing file is never overwritten |

Three Node scripts in `scripts/` measure a running feed: `feed-latency.mjs`, `feed-load.mjs` and `feed-vs-source.mjs`. See [Performance](#performance).

## Performance

Measured on an Apple M3 Max with release builds unless noted.

### Latency

Nearly all of the lag a viewer sees is the source's own delay before its data reaches the service. The service adds tens of milliseconds, its code less than one.

| Stage of a live update | Typical |
| --- | --- |
| Source delay until a push reaches the service | 320–350 ms |
| Waiting until all seven source connections have the push | 20 ms (p90 115 ms) |
| Compute: decode, merge, match, encode | 0.2 ms |
| Publish batching (50 ms tick) | 25 ms on average |
| Browser: parse, apply, render | a few ms |

Compared with the source's own feed, on one clock (`scripts/feed-vs-source.mjs`):

| Service runs | Behind the source, p50 | p90 |
| --- | --- | --- |
| On the viewer's machine | 83 ms | 238 ms |
| On a cloud server | 177 ms | 310 ms |

Viewers of the source's own app see the same source delay.

### Throughput and resources

| Measure | Result |
| --- | --- |
| Matcher, synthetic fuzz flow | about 10 million commands per second, about 100 ns each |
| Cancel inside a price level | about 20 ns at the front, about 0.2 µs in the middle of 1,000 orders (real levels hold at most about 124) |
| Feed per client | about 1.8 updates per second, 4–7 KB/s |
| Browser per update | about 50 µs of JavaScript |
| 200 clients for 6 hours on a cloud server | none dropped, memory flat at about 89 MB, 2.4% of one core |

```bash
cargo run --release -p harness --bin bench                               # matcher throughput and cancel cost
node scripts/feed-vs-source.mjs 180 ws://127.0.0.1:8080/ws               # lag behind the source's own feed
node scripts/feed-latency.mjs 60 local=ws://127.0.0.1:8080/ws            # update rate, data rate, lag
node scripts/feed-load.mjs 3600 200 ws://127.0.0.1:8080/ws --pid <live>  # many clients, memory and CPU
```

Faster data structures, ring-buffer queues and link-time optimisation would save microseconds at this load and are left out. Compressing the feed would make it about 7 times smaller, but the WebSocket library does not support it.

## Trade-offs vs. production exchanges

| Area | Here | In production |
| --- | --- | --- |
| Distance to the source | General-purpose server; about 0.3 s of lag is the source's | Colocated with the matching engine, latency in microseconds |
| Replication | One engine instance | Replicas kept in sync by consensus, instant failover |
| Persistence | No journal or snapshots; state is lost on restart | Every input journaled, book snapshotted for recovery |
| Markets | One market (BTC-USD) | Many markets sharded across engines |
| Gateway | One gateway with checks that need no book state; rejects are only logged | Many gateways with authentication, rate limits and risk checks; rejects answered to the sender |
| Accounts and risk | None | Balances, margin and pre-trade risk checks |
| Sequence gaps | A gap stops the engine | Missing input requested again from the journal |
| Market data | JSON over WebSocket in 50 ms batches, price levels only, uncompressed | Binary and per-order feeds, compression |
| Order types | Market, limit (GTC, IOC, post-only), cancel, replace | More order types and time-in-force options |
| Price protection | None for market or limit orders | Price bands and protection limits |
| Trading status | Open or halted (cancels only) | More states such as auctions and cancel-only, per-instrument halts with reasons |
| Price levels | A queue per level; a cancel inside a level is linear, which real level sizes keep cheap | Linked structures with constant-time cancel |
| Order ids | The gateway remembers the newest 1,000 ids; a fast hash that assumes trusted ids | Ids checked against open orders; hashing hardened against crafted input |
| Client limits | 500 clients, 4 KiB inbound messages, no per-IP limits | Per-IP and rate limits at the edge |
| Replayed book | Rebuilt from price levels, so orders and queues are synthetic | Per-order feeds give the real queues |
| Source feed | Hyperliquid's undocumented `l2` feed, which can change without notice | Documented, versioned feeds under an agreement |
| Flow log | The newest 500 order events and 100 trades | Searchable history |
