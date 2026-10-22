# AGENTS.md

## Project

An exchange-style order book service and matching engine for one market (BTC-USD), driven by order flow derived from Hyperliquid data, with a web UI that visualizes the live book. Rust (Cargo workspace, Tokio, Axum) for the backend; React 19 + Vite + TypeScript strict (pnpm) for the frontend.

Start with [README.md](./README.md) for how to run it and [docs/](./docs/) for the architecture. This file is how to work in the repo; the README is what exists. When they disagree, ask.

Don't add tooling, frameworks, or packages beyond the layout and commands below without asking.

**Check versions, not memory.** Several tools here are newer than most training data (Rust edition 2024, React 19, TypeScript 7, Vite 8, Vitest 5, Biome 2, ts-rs 12). Read the installed version's docs before relying on remembered APIs.

## Layout

```
crates/
  protocol/     shared message format: commands in, events out, market-data feed; source of the TS types
  matching/     order book and matching; depends on nothing but protocol
  engine/       gateway (validate + sequence), publisher
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

State the plan in a few bullets first: the types, the modules, the edge cases, and the check that will show it works (see "Verifiable results"). Wait for an OK. Ask when the requirement is unclear instead of guessing. Decisions about scope, dependencies, the protocol, the look of the UI, and guardrails are the developer's: propose, then wait.

## The simplest thing first

Before writing code, stop at the first rung that holds:

1. Does this need to exist? No: skip it.
2. Already in this codebase? Reuse it.
3. Does the standard library or the platform do it? Use it.
4. An installed dependency? Use it.
5. Only then: the minimum that works, plus a test if it has logic.

Lazy about the solution, never about reading the code first. Validation, correctness checks, and accessibility are never cut.

## Small, focused changes

Work on one thing at a time, so every change is understood by the next agent or human without extra context:

- One concern per change: one module, one protocol change, or one behavior. A refactor, a fix, and a feature are three changes, not one.
- Keep diffs small enough to review in a few minutes. When a task grows beyond that, stop, propose a split, and do the first part only.
- No drive-by changes: no reformatting, renaming, or "while I'm here" fixes outside the task; note them for the developer instead.
- No speculative abstractions or features from later steps; build for today behind the protocol.
- Every change leaves the repo green (the gates under Commands) and ends with a short summary: what changed, why, the evidence, and what comes next. Explanations of language or library concepts belong in that summary, never in code comments.

## Verifiable results

Every task, code or not, ends in a check that could have failed. "Done" means the check ran and passed, not that the work looks finished.

- **Name the check first.** The plan says what will show success or failure: a test, a command, a measurement, or an artifact. If no check can be named, the task is unclear; ask.
- **Ask when the check needs the developer.** How the UI should look and feel is the developer's call; agree on it before declaring success.
- **End with evidence.** The summary lists the commands run and what they returned (exit status, counts, numbers), split into **Verified** (observed in this session) and **Assumed** (not checked, and why). A subagent's report is assumed until its evidence is seen.
- **Red before green.** Watch every new or changed test fail without the change (revert it, or break the line it guards), then pass with it. A bug fix starts from a failing test that reproduces the bug. Make deliberate breaks in a throwaway copy (e.g. a `git worktree`), never in the working tree, so a failed restore can't leave a break behind.
- **Check the checks.** Expected values and thresholds can be wrong: derive them independently, and prove a guard by making the break it guards against.
- **Per kind of task:**
  - Engine and matching: a scenario or property test that pins the behavior.
  - Feed and service: a run against the live server, not only unit tests.
  - UI: in a real browser, every state the change touches, keyboard only for interactive parts.
  - Docs: every command, path, and link added or changed is run or opened; diagrams are checked in both variants.
  - Research and proposals: every claim carries a source; anything from memory or judgement says so.
  - Refactors: the same tests pass unchanged; a test removed as redundant names the test that still catches its break.
  - Config, tooling, dependencies: run what they affect; after a lint or type config change, show a planted violation is still caught.

## Guardrails

A change that conflicts with one of these gets **flagged to the developer, not silently implemented**, even if the task seems to ask for it:

- **The protocol is the contract.** Every external source enters through an adapter that translates into `crates/protocol`; nothing else crosses a component boundary. Protocol types are what crosses threads, queues and the wire between components; a binary that hosts the engine in-process (such as the replay binary) drives it through the engine's public API (`engine::run`, `engine::Step`). Protocol changes are proposed, never slipped in.
- **The engine is deterministic.** Same command sequence in, same event sequence out. No clocks, randomness, I/O, or hash-iteration order in the matching path; timestamps come from the input envelopes.
- **The engine runs alone.** One dedicated OS thread, not a Tokio task. No locks, no async, no database in the hot path. Bounded queues are its only link to the rest of the service; their internals are the one exception to the rules above: a lock taken on every receive and on any send that has to wait, and the parking a blocking send or receive uses. It never touches network I/O; when its output queue is full it waits (backpressure) and never drops events.
- **Integers for money.** Prices are ticks (`i64`), sizes are lots (`u64`). No floats in the engine or the protocol; conversion to decimals happens only for display.
- **Price-time priority.** Price levels sorted per side, FIFO within a level. Any deviation (e.g. Hyperliquid's in-block ordering) lives in an adapter or replayer, not in the book.
- **The gateway checks only what it can know.** Checks that need no book state (size, price and id bounds, duplicate id) in the gateway; anything that needs book state is the engine's to reject.

## Rust

- Edition 2024, toolchain pinned in `rust-toolchain.toml`. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are clean.
- Dependencies are declared once in the root `[workspace.dependencies]` and referenced with `workspace = true`. `Cargo.lock` is committed.
- Errors: `thiserror` enums in library crates; `anyhow` only in binaries. No `unwrap`/`expect` outside tests unless the invariant is proven locally, and then `expect` with a message stating it.
- No `unsafe`. Logging with `tracing`, never `println!` in library code.
- Doc comments (`///`) on every public item; `//!` at the top of each crate and module.
- Prefer plain, idiomatic code over clever code: owned types and clear control flow before lifetimes and generics.

## Protocol and generated types

- Types in `crates/protocol` derive `serde`; types the web client reads also derive `ts_rs::TS`, and types it never receives (commands) do not. JSON enums are internally tagged (`{"type": "...", ...}`), fields `snake_case`.
- `u64`/`i64` fields are exported to TypeScript as `number`. Values stay below 2^53, which is documented on the types.
- TS types, for the types that derive `TS`, are generated into `web/src/protocol/` by `cargo test -p protocol` and committed. Never hand-edit them; CI fails if they are stale.
- Names follow established trading terminology (order types, time in force, execution events), not invented terms.

## Web

- React 19, Vite, TypeScript strict, pnpm. Biome for lint and format (2-space indent, double quotes); no ESLint or Prettier.
- Desktop only. Styling with CSS Modules on CSS custom-property tokens defined once in `src/styles/`; never raw hex values or magic numbers in components.
- `type` over `interface`; no `any` (use `unknown` and narrow); `import type` for type-only imports.
- The feed is the source of truth: apply snapshots and diffs by sequence number, detect gaps, resync from a snapshot. Never derive book state from the event log.
- Prices and sizes arrive as integer ticks and lots; convert with the market's tick and lot size and format with `Intl.NumberFormat`.
- Updates arrive every 100 ms: keep renders cheap, give rows stable keys, and never render an unbounded list: cap it (the flow log keeps the newest entries) or virtualize it. Measure before adding `memo`.
- Accessibility: semantic elements, labels on icon-only buttons, keyboard reachable.

## Tests

- Think about tests first. Cover edge cases and failure paths, not just the happy path.
- Rust: unit tests in a `#[cfg(test)] mod tests` beside the code; cross-crate behavior in `tests/`. Matching logic gets scenario tests (`scenarios/`) and property tests for book invariants.
- Web: Vitest + React Testing Library, tests beside the source (`foo.ts` → `foo.test.ts`), explicit imports (no globals). Query by role and accessible name, never by class name.

## Code comments

Comments explain the why and stand on their own. Never cite plans, phases, task ids, or review rounds; that context belongs in commit messages and docs.

Never name real exchanges or trading venues in code, comments, tests, or docs; describe the practice instead ("production venues"). The one exception is Hyperliquid where it is the actual data source (adapter, replay, oracle).

## Docs and diagrams

- The README describes what exists today; planned work is listed under a clearly marked roadmap.
- Docs change in the same commit as the behavior they describe: README, this file, doc comments, and diagrams.
- Every deliberate simplification versus a production exchange is recorded in the README's trade-offs section when it is made.
- Diagrams are SVGs in `docs/` with `-light` and `-dark` variants, embedded via `<picture>` so GitHub picks the one matching the reader's theme. Both variants always change together.

## Dependencies

- Add a dependency only when the feature needs it, and propose it first.
- npm packages at exact versions (`pnpm add -E <pkg>`). Prefer stable releases.

## Environment

Env files (`.env`, `.env.local`) belong to the developer; don't create, edit, or print them. Settings come from environment variables documented in the README. Recorded data lives in the gitignored `data/`. The repo is public: no secrets, credentials, or personal data in code, fixtures, docs, or logs.

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

Before committing, run the gates that apply in this order (each one blocks the next): `cargo fmt` → `cargo clippy` → `cargo test` → `pnpm lint` → `pnpm typecheck` → `pnpm test` → `pnpm build`. CI (`.github/workflows/ci.yml`) runs the same gates on every push to `main` and every pull request.

## Reviews

Before a change is proposed for commit, independent reviewers read the diff. They run in the background and report only: they never edit files, commit, or call external services. Every brief includes the Guardrails above and the comment rules (agents default to citing the spec they were handed).

- **Correctness and guardrails:** bugs with a concrete failure case, guardrail violations, missing tests for failure paths, tests that would still pass with the change reverted, and summary claims without evidence.
- **Simplicity:** walk every new file, function, abstraction, and dependency down "The simplest thing first" and report the first rung that would have held. Never suggest cutting validation, correctness checks, or accessibility.

Findings are reported as `file:line`, the rule or bug, and a concrete failure case; nothing else. The main agent verifies them and relays them to the developer; fixing them is a normal task that goes through "Before you code".

## Commits

- [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): description`, lowercase, imperative. Types: `feat`, `fix`, `refactor`, `test`, `chore`, `docs`, `perf`, `ci`. Scope is the crate or `web`.
- Atomic: one concern per commit, and every commit builds and passes the gates on its own. No `Co-Authored-By` trailer.
- Wait for the developer's approval before committing.
- Never touch or commit the developer's pre-existing uncommitted work; check `git status` first.
