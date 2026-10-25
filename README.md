# orderbook

An exchange-style order book service and matching engine for a single market (BTC-USD), driven by order flow derived from Hyperliquid data. A full-screen web UI visualizes the live book and a log of the events that produced it. The project mirrors how real venues are built and documents where it deliberately does not.

> Early stage: the protocol, the matching crate, a live feed driven by random-walk order flow through the gateway and the matcher, and the web UI exist so far. See [Status](#status).

## Architecture

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/architecture-dark.svg">
  <img alt="Three input sources (Hyperliquid data via an adapter, a synthetic generator, hand-written scenarios) feed a replayer, which sends commands to the order book service (gateway, matching engine, publisher). The publisher streams events to the web app and the test harness; Hyperliquid data also serves the harness as a correctness oracle." src="docs/architecture-light.svg">
</picture>

The diagram shows the target design. Principles:

- **Own protocol, adapters at the edge.** Every external source is translated into `crates/protocol` by an adapter. The engine only speaks this protocol, so swapping archived data for a live node means a new adapter and no engine change.
- **Single-threaded, deterministic engine.** One dedicated OS thread, in memory, price-time priority, no locks or database in the hot path beyond the synchronization inside its bounded queues. The same command sequence always produces the same event sequence; timestamps come from the input stream, never from a clock.
- **Integers for money.** Prices are ticks (`i64`), quantities are lots (`u64`). Decimals appear only at display time.
- **Publisher batching with snapshot and sequenced diffs.** The publisher keeps its own aggregated copy of the book and sends one combined diff every 50 ms (only when something changed), or sooner when a burst reaches 2,000 events (so a message does not grow with the burst: in a live run the publisher sent 85,912 bytes for 487 events, about 175 bytes per event in JSON, so a full update of 2,000 events is roughly 350 KB; a single command with more events goes out alone), and an update never contains part of a command's events. A client is dropped once 64 updates are waiting unread for it, which a large burst can now reach in one go; see the slow-consumers trade-off. New clients receive a snapshot first (at most `ORDERBOOK_MAX_CLIENTS` are served at once; see the client-limits trade-off); every message carries a sequence number, so clients detect gaps and resync from a snapshot. After about a second without any message the publisher sends a heartbeat that repeats the latest sequence number without advancing it. A client that hears nothing for 3.5 seconds (three missed heartbeats plus half an interval of slack), or gets no snapshot within 10 seconds of connecting, treats the connection as dead and reconnects with a fresh snapshot. A flow log paused in the UI stays paused across such reconnects and resumes only when the server restarts, which the client recognizes by a snapshot whose sequence number is lower than one it has already seen.
- **Bounded queues only.** The engine's sole link to the networked side (Tokio, Axum) is bounded in-memory queues. Its output queue holds one item per command, all of that command's events together; when it is full the engine waits for the publisher to catch up (backpressure) and never drops an event.
- **Throughput log.** Every 10 seconds the publisher logs one `info` line (`publisher throughput`) with the updates, events and bytes sent in that window, the largest update in bytes and in events, and the largest lag between the wall clock and the newest event's `ts`. The lag only means something in live mode; in a replay at another speed `ts` follows the recording's clock. Windows without an update log nothing.

## Web UI

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/ui-layout-dark.svg">
  <img alt="Web UI layout. Left: the order book is centered with asks above bids, a price-grouping menu in the top right corner of the book and a toggle button at the top right. Right: with the flow log open, the screen splits into halves, the book and its grouping menu centered in the left half and the flow log centered in the right half, with the toggle at the right end of its toolbar. Far right: in a narrow window, the flow log stacks below the book, each one window tall, with the toggle at the top right." src="docs/ui-layout-light.svg">
</picture>

Desktop browsers, in the dense style of a professional exchange order book.
The book is a panel of up to 20 rows per side around a spread bar that stays put.
A side with fewer levels keeps its slots and leaves its far end blank.
On a window too short for 20 rows per side, the panel shows as many as fit.
Numbers and depth bars change at once, without easing.
A price that was not anywhere in the previous book (visible or not) flashes in its side color, each row after its own short delay so a batch lights up in a scattered order. There is no flash for the first book, the book right after a grouping change or for a size change, and none under reduced motion, where the duration tokens shrink it to nothing.
A menu at the top right of the book panel, above the Total column (labelled "Group prices"), sums levels into wider price buckets: bids round down and asks round up, and the finest width, one tick, is the default. The offered widths follow the number of digits of the best bid, or with no bids of the best ask minus one tick. A width the user picked stays until they pick another, even when the price drifts to a digit count that does not list it, and it survives a feed resync. The spread bar always shows the raw spread of the ungrouped book, as a percentage of the mid price with three decimals; a positive spread never shows below 0.001%, while a locked book shows 0.000% and a crossed one a negative value.
The spread bar shows the last trade price with an arrow for the taker side, in the bid color for a buy and the ask color for a sell.
A toggle opens the flow log beside it, with filtering and pause. In a window narrower than 1064 px (half a window would no longer fit the book) the log opens below the book instead: the page scrolls, the book and the log are each one window tall, the toggle stays at the top right and the connection status scrolls away with the book.
The connection status at the top left reads Live (green dot), Connecting or Reconnecting (amber dot, pulsing). When the feed is live but the market is halted it reads "Halted · source reconnecting" with a steady amber dot: the service's connections to its order source are down and it accepts only cancels until they are back; the book on screen is unchanged.
The log is ready to show rejected orders and refused cancels and replaces; the random-walk order flow never provokes them.
The feed is the source of truth for book state: the UI applies snapshots and diffs by sequence number and never derives the book from the event log.

## Status

Stack: Rust (Cargo workspace, Tokio, Axum) and React with TypeScript. Browsers receive JSON over WebSocket. TypeScript types are generated from the Rust protocol crate with [ts-rs](https://github.com/Aleph-Alpha/ts-rs).


## Getting started

The pinned toolchain (Rust 1.99.1, see `rust-toolchain.toml`) is installed automatically by [rustup](https://rustup.rs).

```bash
cargo test --workspace                   # also regenerates the TypeScript types in web/src/protocol/ and checks a replayed recording against the source's books
cargo run -p engine --bin orderbook      # serves the live feed at ws://127.0.0.1:8080/ws
cargo run -p harness --release --bin bench   # matcher throughput and cancel cost; release build only, numbers depend on the machine
cargo run -p hyperliquid-recorder --bin record   # records the live BTC book (standard, fast and l2 feeds), the best bid and offer, and trades from Hyperliquid to data/, until Ctrl-C
REPLAY_PATH=data/<recording>.jsonl cargo run -p replayer --bin replay   # serves a recording through the engine
cargo run -p replayer --bin live         # serves Hyperliquid's live BTC book through the engine, until Ctrl-C
REPLAY_PATH=data/<recording>.jsonl cargo run --release -p replayer --bin levels   # level lengths and cancel cost on a recording; release build only
REPLAY_SPEED=step REPLAY_PATH=data/<recording>.jsonl cargo run -p replayer --bin replay   # Enter plays one recorded moment
```

The generated TypeScript files are committed; do not edit them by hand.

To run the web UI against the feed (Node 24, see `web/.nvmrc`, and pnpm):

```bash
pnpm -C web install
pnpm -C web dev                          # http://localhost:5173, /ws is proxied to the feed server
```

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

## Repository layout

```
crates/protocol/   shared message format; source of the TypeScript types
crates/matching/   order book, limit- and market-order matching, cancel and replace: price levels with FIFO queues; no async or network crates, depends only on protocol (plus compile-time thiserror)
crates/engine/     order book service: random-walk order flow, gateway, matcher on its own thread, publisher, WebSocket feed
crates/generator/  seeded fuzz order flow, adversarial yet within the gateway's bounds; depends only on protocol (plus rand and rand_chacha)
crates/harness/    invariant checks and a naive reference matcher; its fuzz test runs generated flow through both matchers and compares their events exactly; depends on protocol and matching (plus generator in its tests)
crates/adapters/hyperliquid/  Hyperliquid adapter (library): `l2Book`, `l2`, `bbo` and `trades` message types, decimal strings to ticks and lots; `l2` keeps the book of the `l2` feed from its changes; `merge` combines its price groupings into one book; `translate` turns snapshots and trades into commands; depends on protocol and matching (plus serde, serde_json, base64, flate2 and compile-time thiserror)
crates/adapters/hyperliquid-recorder/  library that opens the seven connections to Hyperliquid's public WebSocket, subscribes, watches them (silence limits, pings, closed connections) and yields one JSON value per message with the `l2` ones tagged, and the `record` binary that writes them to a JSON Lines file; depends on tokio, tokio-tungstenite (rustls), serde_json and compile-time thiserror, not on the other crates
crates/replayer/   loads a recording into commands and paces them for the service (real time, n times or maximum speed); the `replay` binary serves a recording through the engine; `L2Flow` assembles the `l2` feed line by line; the `live` binary serves Hyperliquid's live book through the engine; the `levels` binary measures level lengths and cancel cost on a recording. The library depends on engine, protocol, the Hyperliquid adapter and recorder, serde, serde_json, thiserror, tokio and tracing; the binaries add anyhow, matching and tracing-subscriber; its tests add futures-util and tokio-tungstenite
web/               React frontend; src/protocol/ holds the generated TypeScript types
scenarios/         hand-written deterministic test cases (JSON), run by the matching crate's tests
docs/              diagrams
scripts/           small dependency-free tools (Node): `feed-latency.mjs` measures update rate, data rate and lag as a client sees them
```

Planned, not yet present: `data/` (recorded sessions, gitignored).

## Performance

Figures were measured on an Apple M3 Max with release builds. Each says when and how to reproduce it, or that it was a one-off measurement.

**Matcher, synthetic flow** (2026-10-24; `cargo run --release -p harness --bin bench`): about 10.3 million fuzz commands per second, 96.6 to 97.3 ns per command over three runs. The previous build, run alternately with it, measured 108.5 to 118.0 ns before the order index switched to a faster hasher.

Cancel cost by position in the queue, from the same bench (one run under heavy system load, so read these as upper bounds):

| Orders at the price | Front of the level | Middle of the level |
| ------------------- | ------------------ | ------------------- |
| 1,000               | about 20 ns        | 214 ns              |
| 10,000              | about 20 ns        | 1.9 µs              |
| 100,000             | about 20 ns        | 20 µs               |

Real BTC-USD levels are much shorter: at most about 124 orders, median 6 (see the replayed measurement under the price-levels trade-off in [Trade-offs](#trade-offs-vs-production-exchanges)).

**Publish tick, 50 ms against 100 ms** (2026-10-24; two builds side by side on the live source for 3 minutes, one client each, measured with [`scripts/feed-latency.mjs`](scripts/feed-latency.mjs)):

The tick is the constant `TICK` in `crates/engine/src/publisher/mod.rs`, so each build is a checkout with its own value, started on its own port (one with `from_millis(100)`, one with `from_millis(50)`):

```bash
ORDERBOOK_ADDR=127.0.0.1:8081 cargo run --release -p replayer --bin live   # in the 100 ms checkout
ORDERBOOK_ADDR=127.0.0.1:8082 cargo run --release -p replayer --bin live   # in the 50 ms checkout
node scripts/feed-latency.mjs 180 t100=ws://127.0.0.1:8081/ws t50=ws://127.0.0.1:8082/ws
```

| Tick   | Lag p10 / p50 / p90 | Updates sent | Per client | Server CPU over 180 s     |
| ------ | ------------------- | ------------ | ---------- | ------------------------- |
| 100 ms | 365 / 426 / 572 ms  | 1.78 per s   | 7.0 KB/s   | 1.07 s (0.6% of one core) |
| 50 ms  | 354 / 397 / 537 ms  | 1.78 per s   | 6.7 KB/s   | 1.11 s (0.6% of one core) |

The lag includes the source's own delay and any clock offset, so only the difference between the rows means anything.

**Throughput log** (the `live` binary logs one line every 10 s: updates, events, bytes, largest update, largest lag). A typical live window: 18 updates, about 600 events, about 92 KB. The largest update is the first one, which mirrors the whole book (about 36 KB).

**Browser** (one-off, 2026-10-24): handling one update costs about 50 µs of JavaScript (parse, book apply, grouping, flow log), measured in Node with the real modules. With the flow log open the main thread is about 1.4% busier than with it closed (production build, Chrome).

**Memory and clients under load** (2026-10-24 and 2026-10-25; [`scripts/feed-load.mjs`](scripts/feed-load.mjs) opens 200 WebSocket clients plus one that stops reading, against the `live` binary, and samples the server's RSS and CPU every 30 s):

```bash
node scripts/feed-load.mjs <seconds> 200 ws://127.0.0.1:8080/ws --pid <live-pid> --every 30
```

| Run                                                         | Length  | Clients                                                          | RSS                                                                                                       | CPU of `live`      |
| ----------------------------------------------------------- | ------- | ---------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ------------------ |
| Apple M3 Max, release build                                 | 1 hour  | None dropped; the stalled client was disconnected after 95 s     | 42 to 69 MB after a 10-minute warm-up (87 MB at most during it), no upward trend                          | 2.7% of one core   |
| Small 2-vCPU x86-64 Linux cloud server (clients on the same machine) | 6 hours | None dropped or failed, about 8 million updates delivered; the stalled client was disconnected after 205 s | Rose in steps over the first three hours (60, 84.5, 89 MB), then flat at 88.9 to 89.3 MB for the last three | 2.4% of one core; never restarted |

Interpretation, not measured: the steps look like the allocator keeping its high-water mark after busier periods rather than a leak, since memory stayed flat afterwards. A run of days would be needed to rule out a very slow leak.

**Not worth it at this load:**

- Arena or intrusive-list matcher: about 100 ns per command against about 75 commands per second.
- Ring-buffer queues: the queues cost 25 to 60 µs of a push that takes about 0.2 ms.
- Link-time optimisation: no measurable gain.
- A faster inflate: saves about 10 µs per push.
- Parsing each source line once: about 3 µs per line, and it would break the record and replay path that shares the parser.
- Compressing messages to clients: zlib makes real updates about 7 times smaller, but the WebSocket library has no permessage-deflate (already a [trade-off](#trade-offs-vs-production-exchanges)).

### Latency

The lag a viewer sees is mostly the source's own delay before its data reaches the service. The service itself adds tens of milliseconds, and its code less than one millisecond.

Where the time goes for a live update (one-off timing on the 10 s fixture and a 60 s live run, 2026-10-24):

- The source's delay until a push reaches the service: p50 about 320 ms, p90 376 ms (includes clock offset). The range 320 to 350 ms recurs across runs.
- Waiting for the slowest of the seven source connections: p50 20 ms, p90 115 ms.
- Compute per push: about 0.2 ms.
- Publish tick: 0 to 50 ms, 25 ms on average.
- Network from the service to the viewer: depends on the distance.
- Browser: a few milliseconds.

Of the 0.2 ms, `L2Flow` (decode, merge, translate) takes about 86 µs, the engine about 100 µs, the queues 25 to 60 µs and the publisher's encoding about 10 µs. Nearly all of the lag a client sees is the source's.

Against the source's own feed (2026-10-25, 3 minutes each, [`scripts/feed-vs-source.mjs`](scripts/feed-vs-source.mjs)): the script subscribes to the source and to one of our feeds and, for each push, compares when it first arrives from each, on one clock, so there is no clock offset in the result.

- Service on the same machine as the viewer: our feed is p50 83 ms behind the source's (p10 35, p90 238 ms); 323 of 325 pushes matched.
- Service on a small cloud server: p50 177 ms (p10 126, p90 310 ms); 326 of 332 matched.

On the same machine, about 25 ms of the 83 is the publish tick and the rest is waiting for all seven connections. On the server, the extra 95 ms or so is the detour source to server to viewer instead of source to viewer. The source's 320 to 350 ms is the same for viewers of the source's own app.

```bash
node scripts/feed-vs-source.mjs 180 ws://127.0.0.1:8080/ws
```

## Trade-offs vs. production exchanges

- **Distance to the source:** nearly all of the lag a viewer sees (about 0.3 s) is the source's delay until its data reaches the service, which runs on a general-purpose server rather than next to the source. Production market-data systems run in the same data centre as the matching engine (colocation) and measure latency in microseconds. See [Latency](#latency).
- **Replication:** a single engine instance. Production runs several identical replicas kept in sync by a consensus protocol such as Raft, for instant failover.
- **Persistence:** no journal or snapshots; state is lost on restart. Production journals every input and snapshots the book for fast recovery.
- **Markets:** one market (BTC-USD). Production shards many markets across engines, each on a pinned CPU core.
- **Gateways:** one simple gateway whose checks need no book state. Production runs many gateways with authentication, rate limits and risk checks, feeding a central sequencer.
- **Gateway rejects:** a rejected command is logged on the submitting thread and never reaches the event stream or the public feed; production gateways answer the sender privately. The gateway remembers only the 1,000 highest `Place` order ids, so its memory is bounded: an id is refused if it was used or, with the window full, is below the smallest remembered one. A `Place` refused for its size or price still uses up its id and takes a slot in the window; that is not guarded against, because an id is unique once sent and the only submitter is the service's own translator. Production venues check client ids only against open orders or within a bounded window.
- **Sequence gaps:** a command whose sequence number is not the next one stops the engine with a panic, and the service with it. Production engines request missing input from the sequencer's journal on a gap and drop a repeated number as a duplicate, then carry on.
- **Accounts and risk:** no balances, margin or pre-trade risk checks.
- **Market data:** JSON over WebSocket, batched (50 ms). Production also offers binary feeds (e.g. SBE, multicast) for professional clients, and per-order (market-by-order) feeds. This feed is level-based only: aggregated quantity per price.
- **Compression:** feed messages are sent uncompressed. Production feeds commonly offer WebSocket compression (permessage-deflate) or compact binary encodings. Measured gain: see [Performance](#performance).
- **Heartbeat:** the heartbeat repeats only the latest sequence number. Production feeds' heartbeats also carry a time and the last trade id, so a client can tell a quiet market from a stalled one.
- **Measurement:** the matcher is benchmarked on its own (see [Performance](#performance)), and the publisher logs message rates, sizes and lag every 10 seconds. There is no continuous latency percentile, no end-to-end measurement from command to client inside the service (only the one-off client-side lag in [Performance](#performance)) and no queue-depth metric; production venues measure end-to-end latency percentiles and monitor queue depth and message rates continuously.
- **Price grouping:** the client receives the full-depth book and groups it itself. Production venues do both: some send the full level book and leave grouping to the client, others serve a few levels per side already grouped at the precision the client asks for, which keeps messages small.
- **Level totals:** nothing bounds a level's total quantity, and above 2^53 lots the feed's integers are no longer exact. This is a workload assumption, not a property of the market: the service has no balance checks and the source is a derivatives market, so physical supply bounds nothing. Real BTC-USD levels stay many orders of magnitude below 2^53 lots, but synthetic flow could exceed it (about 90 million maximum-size orders at one price). The web client refuses such a message and can't show the book until the level shrinks. A production venue would cap open orders per account or publish sizes as decimal strings or binary integers.
- **Price levels:** each level is a `VecDeque` FIFO, so canceling inside a level is linear in its length. An index-based linked list gives O(1) cancel.
  - Synthetic benchmark: see [Performance](#performance).
  - Replayed measurement (`levels` binary on 22 minutes of BTC-USD, 247 snapshots, 17,669 commands; Apple M3 Max, release build, 2026-10-22): the source reports a median of 6 orders per level (p99 41, max 124), and 22 at the best bid and ask (p99 92, max 124). Our replayed book holds at most 11 orders in a level (6 at the best bid and ask), since synthetic orders are placed one per growth of a level. The 5,227 cancels in the replay cost about 100 ns on average (p50 84 ns, p99 about 170 to 210 ns over repeated runs). Timestamps on this machine tick in steps of about 41.67 ns, so single-sample percentiles are quantized and a single-sample maximum (291 ns in one run, 2.8 µs in another) is noise, not a result. For the observed maximum of 124 orders, the bench at lengths 10 and 100 (same machine, 2026-10-22) gives a middle cancel of 250 ns and 83 ns (the length-10 figure is one cancel per batch, so it is quantized and noisy) and a front cancel of about 41 ns at both.
  - Decision: real levels are too short for the linear cancel to matter, since a cancel costs about as much as an ordinary command. Levels stay a `VecDeque`, and an index-based linked list is not warranted. This is one short session; a much busier market could be measured again with the same binary.
- **Order index hash:** the book's order index hashes ids with a fast multiplicative hasher instead of std's SipHash; unlike SipHash, it does not resist crafted colliding keys. This is safe only because order ids reach the book through our own gateway; if ids could ever come from untrusted clients, this must be revisited. The multiplication spreads sequential ids (the gateway's ids are counters), but ids that are all multiples of a large power of two would use only part of the table, so the hasher suits counter-like ids.
- **Price protection:** market orders have no protection, so on a thin book one can sweep far through the levels. Production venues add price bands or protection limits. Limit prices have no bands either: the gateway only checks that a price is positive and below 2^53, while production venues reject limit prices too far from a reference price, which needs book state.
- **Order types:** market orders trade immediately at any price, cancel any remainder and are rejected on an empty opposite side. Limit orders support GTC, IOC and post-only (a post-only order that would cross is rejected, never re-priced). A replace that changes the price or grows the quantity is a cancel followed by a new arrival: it loses time priority, and if the new price crosses it trades, except that a post-only order is refused instead. The `qty` of a replace is the new open quantity, not the total order size including fills; production venues differ on this. There is no fill-or-kill, stop or iceberg.
- **Trading status:** the market is `Open` or `Halted`; a halt refuses every place and replace (`TradingHalted`) and accepts cancels, and setting the status it already has produces no event. Production venues have more states (pre-open auction, cancel-only, closing) and halt per instrument with a reason code. Live mode halts the market while its connections are down and reopens it with the first push of the new ones; replay never halts, and the fuzz flow sets the status too.
- **Cancel rejects:** `unknown_order` also covers "too late to cancel" (the order already filled or was canceled), which production venues report separately. Telling the two apart needs a record of past order ids in the engine, which answers cancels; the gateway keeps a bounded window of recent ids but never sees whether an order filled.
- **Order flow:** a seeded random walk generates the commands: GTC limits around the touch, cancels, replaces and IOC takers that never exceed the touch, which the real matcher turns into events. It therefore never produces a `Canceled` IOC remainder or a reject.
- **Client limits:** the service caps concurrent feed clients (`ORDERBOOK_MAX_CLIENTS`, default 500; the next gets a 503 before the upgrade) and the size of what a client may send (4 KiB per message and frame; the feed takes no input, so a client that sends a text or binary message is disconnected). There is no per-IP limit or rate limit in the service: one address can use every slot. Production deployments put those in a reverse proxy in front of the feed, which is where they would go here.
- **Slow consumers:** a client that has 64 updates waiting unread in the broadcast buffer (every update of a split burst counts, so a large burst can fill it for a client that is not reading at that moment) or stalls a send for over a second is disconnected and resyncs from a fresh snapshot. There is no per-client replay of missed updates.
- **Replayed book:**
  - With the `l2` feed the book is rebuilt from the merged price groupings at each push (about every 0.54 s), and trades are held to the next complete push, so the replayed book changes only then. A push where the groupings do not all arrive at the same time is left out, and the book stays as it was for as long as the pushes stay incomplete.
  - The documented book feeds are used only for recordings without `l2` lines; the rest of this list describes that path.
  - A level's growth becomes one synthetic order, or several of the gateway's maximum size when it is larger.
  - A trade becomes one IOC order or several at its price. A level's order count therefore differs from the source's `n` and can exceed it.
  - The source publishes price levels, not orders, so every order in the replayed book is synthetic, and a level's queue (order count, sizes, time priority) is not the source's. Production venues publish per-order feeds from which real queues can be rebuilt; Hyperliquid's per-order data is available only from running one of its nodes.
  - Every level missing from a snapshot is canceled, which mirrors its window.
  - The touch (best bid and offer) is refreshed about every 0.1 s, the top 5 levels per side about every 0.5 s from the fast feed and the top 20 about every 5 s.
  - Deeper levels go stale in between, and changes between snapshots show up only as trades.
  - Post-only makers never produce trades; trades come only from the source's trades, as IOC orders.
  - A trade at a price also shows that the opposite side had nothing resting at a better one.
  - The shadow's better-priced orders are therefore stale and are canceled before the trade's orders are placed (`stale_orders_canceled`), so a trade never fills at a price the source did not print.
  - A full snapshot of a book with at most 5 levels per side cannot be told from a top snapshot, so it is mirrored only inside its window.
  - Stale levels beyond that window, or on a side that is empty in it, stay until the next full snapshot.
  - A stale side can make new orders on the other side be rejected (`rejected_makers`).
- **Live mode:** a push is emitted the moment every grouping has a book at its time, so the served book is behind the source by the spread of the seven connections' arrival times plus the publisher's 50 ms tick (an incomplete push waits for later books but makes no commands), and nothing is paced or buffered beyond a 4,096-command queue, behind which the connections are not read. A lost connection leaves a gap: the book stays as it was and the market is halted (the engine accepts only cancels) until the connections are back, and the trades in between are not replayed (the first push afterwards mirrors the book, and the backlog of trades that comes with a new subscription is dropped). There is no resume from the last push, which production feeds offer by sequence number. A recording written by `LIVE_RECORD_PATH` across a reconnect holds both sessions one after the other. While the connections are down the feed keeps its heartbeats but reports the market as halted, in the snapshot of new clients and as a `TradingStatusChanged` event to the others, as production venues publish a trading status alongside market data so clients can tell a halted market from a quiet one; the halt carries the time of the last push replayed and the reopening the time of the first push afterwards.
- **Source feed:** the book at every price grouping comes from the `l2` feed Hyperliquid's web app uses, which its API documentation does not list. Its format was inferred from recordings and checked against fresh subscriptions and the documented 20-level feed; it can change without notice. The documented feeds are recorded too: the fast one has only 5 levels per side and the 20-level one comes only about every 5 s, too little to show the book the way the app does. Production systems that rely on outside market data use documented, versioned feeds under an agreement with the provider.
- **Flow log:** the web UI keeps only the most recent events (the newest 500 order events and, separately, the newest 100 trades, so a busy book cannot push trades out); production terminals offer a searchable history.
- **Infrastructure:** a small shared-vCPU cloud server. Production uses dedicated hardware, often co-located, with tuned networking.
