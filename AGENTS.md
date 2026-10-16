# AGENTS.md

## Project

An exchange-style order book service and matching engine for one market (BTC-USD), driven by order flow derived from Hyperliquid data, with a web UI that visualizes the live book. Rust (Cargo workspace, Tokio, Axum) for the backend; React 19 + Vite + TypeScript strict (pnpm) for the frontend.

Start with [README.md](./README.md) for how to run it and [docs/](./docs/) for the architecture.

## Layout

```
crates/
  protocol/     shared message format: commands in, events out, market-data feed; source of the TS types
  engine/       gateway (validate + sequence), book, matching, publisher
  adapters/     one crate per external source, translating into the protocol
  replayer/     plays recorded input: real-time, accelerated, step
  generator/    synthetic and fuzz order flow
  harness/      invariants, oracle diffs, benchmarks
web/            React frontend; web/src/protocol/ holds the generated TS types
scenarios/      hand-written deterministic test cases
data/           recorded sessions (gitignored)
docs/           architecture notes and diagrams
```

Crates appear as they are built; the layout above is the target.

## Before you code

State the plan in a few bullets first: the types, the modules, the edge cases. Wait for an OK. Ask when the requirement is unclear instead of guessing. Decisions about scope, dependencies, the protocol, and guardrails are the developer's: propose, then wait.

## Guardrails

A change that conflicts with one of these gets **flagged to the developer, not silently implemented**, even if the task seems to ask for it:

- **The protocol is the contract.** Every external source enters through an adapter that translates into `crates/protocol`; nothing else crosses a component boundary. Protocol changes are proposed, never slipped in.
- **The engine is deterministic.** Same command sequence in, same event sequence out. No clocks, randomness, I/O, or hash-iteration order in the matching path; timestamps come from the commands.
- **The engine runs alone.** One dedicated OS thread, not a Tokio task. No locks, no async, no database in the hot path. Bounded queues are its only link to the rest of the service, and it never waits on the publisher.
- **Integers for money.** Prices are ticks (`i64`), sizes are lots (`u64`). No floats in the engine or the protocol; conversion to decimals happens only for display.
- **Price-time priority.** Price levels sorted per side, FIFO within a level. Any deviation (e.g. Hyperliquid's in-block ordering) lives in an adapter or replayer, not in the book.
- **The gateway checks only what it can know.** Stateless checks (tick, size, duplicate id) in the gateway; anything that needs book state is the engine's to reject.

## Rust

- Edition 2024, toolchain pinned in `rust-toolchain.toml`. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are clean.
- Dependencies are declared once in the root `[workspace.dependencies]` and referenced with `workspace = true`. `Cargo.lock` is committed.
- Errors: `thiserror` enums in library crates; `anyhow` only in binaries. No `unwrap`/`expect` outside tests unless the invariant is proven locally, and then `expect` with a message stating it.
- No `unsafe`. Logging with `tracing`, never `println!` in library code.
- Doc comments (`///`) on every public item; `//!` at the top of each crate and module.
- Prefer plain, idiomatic code over clever code: owned types and clear control flow before lifetimes and generics.

## Protocol and generated types

- Types in `crates/protocol` derive `serde` and `ts_rs::TS`. JSON enums are internally tagged (`{"type": "...", ...}`), fields `snake_case`.
- `u64`/`i64` fields are exported to TypeScript as `number`. Values stay below 2^53, which is documented on the types.
- TS types are generated into `web/src/protocol/` by `cargo test -p protocol` and committed. Never hand-edit them; CI fails if they are stale.

## Web

- React 19, Vite, TypeScript strict, pnpm. Biome for lint and format (2-space indent, double quotes); no ESLint or Prettier.
- Desktop only. Styling with CSS Modules on CSS custom-property tokens defined once in `src/styles/`; never raw hex values or magic numbers in components.
- `type` over `interface`; no `any` (use `unknown` and narrow); `import type` for type-only imports.
- The feed is the source of truth: apply snapshots and diffs by sequence number, detect gaps, resync from a snapshot. Never derive book state from the event log.
- Prices and sizes arrive as integer ticks and lots; convert with the market's tick and lot size and format with `Intl.NumberFormat`.
- Updates arrive every ~50 ms: keep renders cheap, give rows stable keys, and virtualize unbounded lists (the flow log). Measure before adding `memo`.
- Accessibility: semantic elements, labels on icon-only buttons, keyboard reachable.

## Tests

- Think about tests first. Cover edge cases and failure paths, not just the happy path.
- Rust: unit tests in a `#[cfg(test)] mod tests` beside the code; cross-crate behavior in `tests/`. Matching logic gets scenario tests (`scenarios/`) and property tests for book invariants.
- Web: Vitest + React Testing Library, tests beside the source (`foo.ts` → `foo.test.ts`), explicit imports (no globals). Query by role and accessible name, never by class name.

## Code comments

Comments explain the why and stand on their own. Never cite plans, phases, task ids, or review rounds; that context belongs in commit messages and docs.

## Docs

- The README describes what exists today; planned work is listed under a clearly marked roadmap.
- Every deliberate simplification versus a production exchange is recorded in the README's trade-offs section when it is made.
- Diagrams are Mermaid in Markdown, so they render on GitHub and diff as text.

## Dependencies

- Add a dependency only when the feature needs it, and propose it first.
- npm packages at exact versions (`pnpm add -E <pkg>`). Prefer stable releases.

## Commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace         # also regenerates web/src/protocol/

pnpm -C web dev                # Vite dev server
pnpm -C web lint               # Biome
pnpm -C web typecheck
pnpm -C web test
pnpm -C web build
```

Before committing, every command above that applies to the change passes.

## Commits

- [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): description`, lowercase, imperative. Types: `feat`, `fix`, `refactor`, `test`, `chore`, `docs`, `perf`, `ci`. Scope is the crate or `web`.
- Atomic: one concern per commit. No `Co-Authored-By` trailer.
- Wait for the developer's approval before committing.
- Never touch or commit the developer's uncommitted work; check `git status` first.
