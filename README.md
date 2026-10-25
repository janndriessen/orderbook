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

- [x] Cargo workspace and `protocol` crate: `OrderId`, `Seq`, `Price`, `Qty`, `Ts`, `Side`, `Envelope<T>`, engine `Command`s (`Place`, `Cancel`, `Replace`, `SetTradingStatus`), engine `Event`s (`Accepted`, `Rejected`, `CancelRejected`, `Trade`, `Replaced`, `Canceled`, `L2Update`, `TradingStatusChanged`) and the market-data `FeedMessage` (`Snapshot`, which carries the trading status, `Update`, `Heartbeat`), with generated TypeScript types for the types the web client reads (not commands) checked in CI
- [x] Publisher and WebSocket feed (`orderbook` binary), with server-side pings that drop a client silent for 15–20 s
- [x] Web UI: live order book (up to 20 rows per side) with depth bars and a spread bar, and a sliding flow log with filter and pause
- [x] `matching` crate: order book (sorted price levels, FIFO queues) and matching of good-till-cancel, IOC and post-only limit orders and market orders with price-time priority, and cancel and replace of resting orders; while the trading status is `Halted` only cancels run, and places and replaces are refused with `TradingHalted`
- [x] Hand-written deterministic scenarios in `scenarios/`, run by `cargo test -p matching`
- [x] The `matching` crate runs alone on its own engine thread, driven by random-walk order flow from the source thread through a bounded command queue
- [x] Gateway that validates and sequences commands in front of the engine thread: checks that need no book state (bounds of order size, price and order id, and order id reuse within a window of recent ids), then gap-free command sequence numbers (`SetTradingStatus` names no order and has no values, so it is only sequenced)
- [x] Fuzz generator, invariant checks and benchmarks
  - [x] `generator` crate: seeded fuzz order flow (all time-in-force values, market orders, cancels and replaces of live, stale and unknown ids, and now and then a halt or reopening of trading) that stays inside the gateway's bounds, checked against the matcher in its tests
  - [x] `harness` crate: structural invariants (book structure, event consistency, the feed's levels matching the book) checked after every command of 50 seeded fuzz runs of 2,000 commands, plus a determinism check. They catch an inconsistent book or feed, not a consistent but wrong one; the reference model below checks the outcomes
  - [x] Reference model: a naive second matcher in the `harness` crate, whose events its fuzz test compares with the real matcher's exactly. It shares no code with the matcher, but it could share a misreading of the docs
  - [x] Benchmark: `cargo run -p harness --release --bin bench` measures throughput and cancel cost (results under [Performance](#performance))
  - [x] Level totals: left unbounded on purpose, since real BTC flow stays far below 2^53 (see the trade-offs)
- [ ] Hyperliquid adapter and replayer
  - [x] `hyperliquid-adapter` crate: decimal strings from Hyperliquid parsed into integer ticks and lots, exactly and without floats, bounded at the protocol's 2^53 - 1
  - [x] `hyperliquid-adapter`: serde types for the `l2Book`, `bbo` (best bid and offer, one level per side, `null` for an empty side) and `trades` WebSocket messages (other channels are reported as errors; messages for other coins are rejected), converted to ticks, lots and microseconds; a best bid and offer whose bid is at or above its ask is reported as its own error
  - [x] `hyperliquid-adapter`: a `Translator` turns full snapshots (20 levels per side, about every 5 s), top-of-book snapshots (5 levels per side, about every 0.5 s; only the price window they span is mirrored), the best bid and offer (one level per side, sent whenever either changes, about every 0.1 s; a `null` side is empty and changes nothing; a locked or crossed one is its own conversion error) and trades into protocol commands through a shadow book of what the engine holds, with counters where it diverges from the source
  - [x] `replayer` crate: paces recorded `(time, command)` pairs into engine steps at real time, n times or maximum speed
  - [x] `replay` binary: loads a recording and serves it through the engine
    - a line on another channel, a malformed line or an unconvertible value fails the load with its line number; `error` lines (the source refusing a request) are skipped and counted; a cut-off last line of a recording still being written is left out with a warning
    - a snapshot with at most 5 levels per side is a top snapshot, a deeper one a full snapshot; a recording without a full snapshot is refused
    - a best bid and offer is a top snapshot of one level per side; one with a bid at or above the ask is skipped and counted
    - the replay starts at the first full snapshot: the trades at or before it (the backlog sent on subscribing, which the snapshot already reflects), and the top snapshots and best bid and offer messages before it, are dropped
    - the rest is translated with the adapter's `Translator`
    - a recording with `l2` lines is replayed from them alone (see the replay items under the price-grouping roadmap below): its `l2Book` and `bbo` lines are skipped and counted without being read, and the rules above about full and top snapshots apply only to recordings without `l2` lines
    - logs the translator's divergence counters, the dropped trades and top snapshots (best bid and offer messages included), the skipped locked or crossed ones, the skipped documented-feed lines, the `l2` pushes left out (start-up, later, failed merges), trades dropped after the last push, the merged books' conflicting lots, the skipped `error` lines (a warning), and recorded gaps over 60 s
    - after the last command it keeps serving the final book until Ctrl-C
  - [x] Oracle test (`crates/replayer/tests/oracle.rs`) against two recordings of public BTC data: 40 s without and 50 s with the best bid and offer
    - after replaying one, the matcher's book equals every recorded full snapshot exactly, and the top 5 levels per side equal every recorded top snapshot
    - the best bid and ask equal every recorded best bid and offer (price and total)
    - the feed of the running service, followed from its snapshot to the end of the flow, ends on the book of an independent level model that applies the recorded full snapshots, top snapshots, best bid and offer messages and trades in time order
    - the exact number of records compared is asserted, counted from the fixtures' lines separately
  - [x] Step mode of the replayer: `REPLAY_SPEED=step` waits for Enter before each recorded moment
  - [x] `hyperliquid-recorder` crate, a library of the connections (`Connections::connect`, `next`) and the `record` binary on it: writes the live BTC book (the standard and the fast feed), the best bid and offer, trades and the `l2` feed at all six price groupings and the one without rounding from Hyperliquid's public WebSocket to a JSON Lines file under `data/`, with every `users` field (wallet addresses) removed; the run ends with an error, with no reconnect, on a closed or lost connection, on a text message that is not valid JSON (never written), after 30 seconds without a full book snapshot on the main connection (more than 5 levels on a side; catches a stalled standard feed and a silent main connection), or after 5 seconds without an `l2` message on one of the seven connections (every connection sends one about every half second, the longest gap measured over minutes was 0.63 s); acknowledgements, `pong` and `error` messages count as no progress
  - [x] Re-measure level lengths and cancel cost on replayed Hyperliquid data: real levels stay short, so levels remain a `VecDeque` (`levels` binary, results under the price-levels trade-off)
- [ ] A book that works like Hyperliquid's own, at every price grouping
  - [x] `hyperliquid-adapter`: `L2Decoder` for the `l2` feed that Hyperliquid's web app subscribes to (not in its API documentation): 20 levels per side about every 0.54 s, a full book on subscribing, then changes that remove levels by index and set levels by price, usually as base64 of a raw deflate stream of JSON and, for some small changes, as the same JSON uncompressed. Every resulting book is checked (strictly best first, at most 20 levels per side, no size of zero, best bid below best ask), and so is every change (a book to apply to, removed indices that exist once, each price set once per side, not older than its book, valid base64 and deflate, at most 1 MiB inflated, convertible values); any failed message drops the book until the next one, since a later change's indices would name the wrong levels. Replaying recorded changes gives exactly the book of a fresh subscription, and of the documented 20-level `l2Book` feed, at the same push (`crates/adapters/hyperliquid/tests/l2.rs`)
  - [x] Recorder: one connection per price grouping, seven in all (six of the app's options and a seventh that subscribes without `s`, whose buckets stay one tick wide above $100,000 where five figures turn $10 wide; the app uses it for its finest option there), since no `l2` message says which grouping it is for; the recorder adds the connection's `subscription` to every `l2` line, without `s` for the seventh
  - [x] `hyperliquid-adapter`: `merge` turns the groupings of one push (the six and the one without rounding, which is the finest) into one book of $1 levels that, grouped by any input's rule, shows exactly that input's buckets. One bucket width per book, from the best bid of the finest input that has bids (an asks-only book uses the finest best ask, read as just below a power of ten when a rounded grouping shows one); bids round down, asks up. Sides deeper than 20 buckets and zero sizes are refused. The finest grouping fixes single prices, what a coarser bucket holds beyond them goes to its far end, and a grouping that lists fewer than 20 buckets shows its whole side. Only listed prices are visited (at most 120 per side), so a far-off level costs nothing extra. Inputs that disagree are counted (`conflicting_qty`, per input bucket put off) and the finer grouping wins. Tests: 600 seeded random books (random subsets of the groupings, thin and one-sided books, over 100 with asks past $100,000) and one recorded push; a one-off run over the 384 pushes that all six groupings share in two live recordings found zero conflicts
  - [x] Replay: align the groupings by push time and hold trades until the next push
    - loading reads every line's header first; a recording with `l2` lines uses them alone, one decoder per `subscription` (a subscription without `s`, or with `s: null`, is the grouping without rounding; an `l2` line without a BTC grouping the source offers, one that does not parse, or one that does not apply fails the load with its line number; errors come out in line order, except that the ones of an `l2` line that does not apply to its grouping's book come after those of all other lines)
    - a push is complete when every grouping in the recording has a book at exactly that time; the groupings of a complete push are merged into one full snapshot. A complete push is replayed the moment its last book arrives, without waiting for later books; one where a grouping sent no book is replayed (left out and counted) once every grouping has sent a later book, so loading holds only the incomplete pushes still open, unless a grouping stops sending, which keeps every later incomplete push open until the end of the file. A grouping that sends a second book at the time of the push replayed last (the decoder accepts a change with the same time; it happened in none of the fixture and the three live recordings tried) makes that push be merged and translated again (`reapplied_pushes`); the translator mirrors whole snapshots, so this only brings the engine to the later books, and a book older than the push is still an error
    - a push where a grouping sent no book is left out and counted: as a start-up push before the push of the last grouping's first book, as a trailing push if it is still open at the end of the file (the recording ended, or a grouping stopped), otherwise as skipped. On two live recordings of 2026-10-23, a 90-second one has 174 push times, of which 162 are complete, 5 are start-up pushes (while the six connections open), 6 are later skipped ones and 1 is a trailing push (still open at the end of the file); a two-minute one has 226, of which 222 are complete, 4 start-up pushes, no later skipped ones and no trailing ones. Neither had a failed merge or a conflicting lot, and the recordings are not in the repository. The fixture of the test below has 5 start-up and 2 later skipped ones of its 25, and no trailing one
    - a push whose groupings do not merge is left out and counted, with the first error logged
    - the assembly is a public type, `L2Flow`, that takes lines one at a time in the order they were written, `l2` and `trades` interleaved, and returns the commands of the pushes it settles; `load` is built on it. A trade whose push was already replayed when it was written goes to the next push and is counted (`late_trades`, which releasing a complete push at once makes more likely: 0 on the fixture, 3 and 3 on two 90 and 120-second live recordings and 1 on a 60-second one, 0 on all of them before). `reset_connection` forgets what was learned from the connections (books, open pushes, held trades, who has started) and keeps the translator, so the first push after a reset is mirrored in full against the engine's book
    - the first complete push that merges starts the replay; trades at or before it (the backlog sent on subscribing) are dropped, trades after the last complete push that merges are dropped and counted
    - every other trade is translated at the time of the first complete push at or after its own time, before that push's snapshot, so the engine's book changes only at pushes, as the source's web app does
    - test: a recorded fixture of all six groupings (`crates/replayer/tests/l2.rs`) checks that after every complete push the matcher's book equals the merged book exactly and, grouped by each grouping's rule, gives that grouping's own book, and that every held trade became an order
  - [x] Oracle test per grouping (`crates/replayer/tests/l2_oracle.rs`): the `l2` fixture runs through the running service and a client follows the feed from its snapshot, with each of the 18 complete pushes released only once the client has all of the previous push's events, so a push never shares an update with another
    - a push is complete when the client has received as many feed events as a local matcher produces for the push's commands (the engine is deterministic; level updates are not counted, the feed turns them into level changes); then the client's book equals that push's merged book exactly and, grouped by each of the six groupings' rules (top 20 buckets), equals that grouping's decoded view; the 18 pushes compared are asserted, counted from the fixture separately
  - [x] Web UI: rows filling the panel (up to 20), bars without easing, a flash on new price levels only, and a grouping menu
  - [x] Live mode: the `live` binary connects through the recorder's library (seven connections), feeds every line through `L2Flow` and sends the commands to the engine as the pushes settle, without pacing, serving the feed at `ORDERBOOK_ADDR`; `LIVE_RECORD_PATH` also writes the lines received
    - when any connection fails (closed, lost, without a full book snapshot for 30 s, without an `l2` message for 5 s, a message that does not apply, or no push merged and no stale book skipped for 10 s since they opened or since the last one, which catches groupings whose times never match while every connection keeps sending) all seven are opened again after a pause of 1 s, doubling to 30 s and starting over once a connection has produced commands, logged each time; what was learned from the old connections is forgotten and the translator is kept, so the first push of the new connections mirrors the whole book and the engine catches up. The market is halted (`SetTradingStatus`) as soon as the connections fail, unless nothing was replayed yet (the book is empty and the halt would have no time from the source, so a source that is unreachable at start-up leaves the market open), and reopened when the first push of the new connections merges, with that push's first command or, if the book did not change and there is none, with the push's time, each only if it is a change from what was last sent; stopping sends nothing. Books of the new connections at or before the last push replayed (the source's clock stepped back) are skipped and counted (`stale_books`, one warning) until every grouping is past it, and count as progress for the 10 s limit only while their grouping's time moves forward (a frozen clock does not); in a recording that is still an error. Ctrl-C stops the connections task, the engine and the server and waits for all of them; so does any end of the connections task, an error (such as a recording that cannot be written) or a panic, and `live` then exits with that error. The recording is written on a thread of its own, fed by a bounded queue
    - tests (`crates/replayer/tests/live.rs`, no network): a local WebSocket server stands in for the source and answers each connection's subscriptions with the fixture's lines of the matching grouping (the grouping without rounding gets the `s: 5` lines, which are the same below $100,000) and its trades; a client follows the service's feed and its book must end equal to what `load` makes of the same lines. A second test has the server close all seven connections once the client has the first half's book and serve the second half to the new ones, which start with a full book as new subscriptions do; the client following the feed must see the market halted, a client connecting meanwhile gets a snapshot saying so, and the market must reopen before any level changes again; the book still ends right and fourteen connections were accepted. A third has a source that keeps all seven connections open and sending but with one grouping's times 1 ms off, so no push ever completes; with a shortened limit the service must open seven new connections. Another serves, after the outage, only the book the service already holds, so the first push of the new connections makes no commands, and the market must still reopen. Two more check that a stop completes while the source is unreachable (during the pause between attempts; the market must still be open then, as nothing was replayed) and while it closes every connection it accepts
- [ ] Deployment to a small Hetzner server

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

**Live pipeline, where an update's time goes** (one-off timing on the 10 s fixture and a 60 s live run, 2026-10-24):

| Step                                            | Time                                          |
| ----------------------------------------------- | --------------------------------------------- |
| The source's own delay until a push reaches us  | p50 about 320 ms, p90 376 ms (includes clock offset) |
| Waiting for the slowest of the seven connections | p50 20 ms, p90 115 ms                        |
| Our compute per push                            | about 0.2 ms                                  |
| Publish tick                                    | 0 to 50 ms, 25 ms on average (the tick is 50 ms) |

Of the 0.2 ms, `L2Flow` (decode, merge, translate) takes about 86 µs, the engine about 100 µs, the queues 25 to 60 µs and the publisher's encoding about 10 µs. Nearly all of the lag a client sees is the source's.

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

## Trade-offs vs. production exchanges

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
