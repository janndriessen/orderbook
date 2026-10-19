import type { MarketInfo } from "../protocol/MarketInfo";
import { applyMessage, type BookState } from "./book";
import { parseFeedMessage } from "./guard";
import { appendEvents, type LogEntry } from "./log";

export type FeedStatus = "connecting" | "live" | "reconnecting";

export type FeedState = {
  status: FeedStatus;
  /** `null` until the first snapshot of the current connection arrives. */
  book: BookState | null;
  /**
   * Last known market. Kept across a reconnect, like `log`, so the flow log
   * stays on screen; the next snapshot replaces the log.
   */
  market: MarketInfo | null;
  /** Order-flow events, newest first. */
  log: readonly LogEntry[];
};

export type Connection = { close(): void };

/** Opens a socket to `url` and forwards its payloads and its closing. */
export type Transport = (
  url: string,
  handlers: { onMessage(data: unknown): void; onClose(): void },
) => Connection;

export type FeedHandlerOptions = {
  url: string;
  transport?: Transport;
  /** Returns a value in [0, 1); injectable so tests are deterministic. */
  random?: () => number;
};

export const BACKOFF_BASE_MS = 250;
export const BACKOFF_CAP_MS = 10_000;

/**
 * The server sends a heartbeat after this much silence; keep in step with
 * `HEARTBEAT_INTERVAL` in `crates/engine/src/publisher/state.rs`.
 */
export const HEARTBEAT_INTERVAL_MS = 1_000;
/**
 * Silence that marks an established connection as dead: three missed
 * heartbeats plus half an interval of slack for tick phase and latency, which
 * a healthy connection never reaches.
 */
export const SILENCE_TIMEOUT_MS =
  3 * HEARTBEAT_INTERVAL_MS + HEARTBEAT_INTERVAL_MS / 2;
/**
 * Time a new socket gets to deliver its first message (handshake plus
 * snapshot), so a slow link still connects while a hung attempt is caught.
 */
export const CONNECT_TIMEOUT_MS = 10_000;

const INITIAL: FeedState = {
  status: "connecting",
  book: null,
  market: null,
  log: [],
};

export const webSocketTransport: Transport = (url, { onMessage, onClose }) => {
  const socket = new WebSocket(url);
  socket.onmessage = (event) => onMessage(event.data);
  socket.onclose = onClose;
  return { close: () => socket.close() };
};

/**
 * Maintains a local copy of the book from the market-data feed, the way an
 * exchange client's feed handler does (not a DOM event callback): it applies
 * the initial snapshot and each sequenced update, and when it sees a sequence
 * gap, a malformed message, a closed socket or a silent one (no message, not
 * even a heartbeat, for `SILENCE_TIMEOUT_MS`, or none at all for
 * `CONNECT_TIMEOUT_MS` after opening) it throws the local book away
 * and reconnects with capped, jittered exponential backoff. A fresh connection
 * always starts with a snapshot, so recovery needs no replay logic.
 *
 * The class holds no React; `subscribe` and `getState` fit
 * `useSyncExternalStore`.
 */
export class FeedHandler {
  #state: FeedState = INITIAL;
  #listeners = new Set<() => void>();
  #connection: Connection | null = null;
  #timer: ReturnType<typeof setTimeout> | null = null;
  #watchdog: ReturnType<typeof setTimeout> | null = null;
  #attempt = 0;
  #everLive = false;
  #running = false;

  readonly #url: string;
  readonly #transport: Transport;
  readonly #random: () => number;

  constructor(options: FeedHandlerOptions) {
    this.#url = options.url;
    this.#transport = options.transport ?? webSocketTransport;
    this.#random = options.random ?? Math.random;
  }

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  getState = (): FeedState => this.#state;

  /** Connects; calling it again after `stop` starts over from scratch. */
  start(): void {
    if (this.#running) return;
    this.#running = true;
    this.#attempt = 0;
    this.#everLive = false;
    this.#set(INITIAL);
    this.#open();
  }

  stop(): void {
    this.#running = false;
    this.#drop();
  }

  #set(state: FeedState): void {
    this.#state = state;
    for (const listener of this.#listeners) listener();
  }

  #open(): void {
    const connection: Connection = this.#transport(this.#url, {
      onMessage: (data) => {
        if (this.#connection !== connection) return;
        this.#arm(SILENCE_TIMEOUT_MS);
        this.#receive(data);
      },
      onClose: () => {
        if (this.#connection === connection) this.#recover();
      },
    });
    this.#connection = connection;
    this.#arm(CONNECT_TIMEOUT_MS);
  }

  /** (Re)starts the watchdog of the current connection with `timeout`. */
  #arm(timeout: number): void {
    if (this.#watchdog !== null) clearTimeout(this.#watchdog);
    this.#watchdog = setTimeout(() => this.#recover(), timeout);
  }

  #drop(): void {
    // Clearing the reference first makes late callbacks from the old socket no-ops.
    const connection = this.#connection;
    this.#connection = null;
    connection?.close();
    if (this.#timer !== null) clearTimeout(this.#timer);
    this.#timer = null;
    if (this.#watchdog !== null) clearTimeout(this.#watchdog);
    this.#watchdog = null;
  }

  #receive(data: unknown): void {
    const message = parseFeedMessage(data);
    if (message?.type === "heartbeat") {
      const held = this.#state.book;
      if (held && message.seq !== held.seq) this.#recover();
      return;
    }
    const book = message && applyMessage(this.#state.book, message);
    if (!message || !book) {
      this.#recover();
      return;
    }
    const snapshot = message.type === "snapshot";
    if (snapshot) {
      this.#everLive = true;
    } else {
      // A snapshot alone proves little: a server that follows every snapshot
      // with a gap must keep backing off, so the delay resets on an update.
      this.#attempt = 0;
    }
    this.#set({
      status: "live",
      book,
      market: snapshot ? message.market : this.#state.market,
      log: appendEvents(snapshot ? [] : this.#state.log, message.events),
    });
  }

  #recover(): void {
    this.#drop();
    if (!this.#running) return;
    this.#set({
      ...this.#state,
      status: this.#everLive ? "reconnecting" : "connecting",
      book: null,
    });
    // Between the floor and the ceiling: spreads clients out without ever
    // retrying instantly, and the ceiling doubles up to the cap.
    const ceiling = Math.min(
      BACKOFF_CAP_MS,
      BACKOFF_BASE_MS * 2 ** this.#attempt,
    );
    const delay = Math.round(
      BACKOFF_BASE_MS + this.#random() * (ceiling - BACKOFF_BASE_MS),
    );
    this.#attempt += 1;
    this.#timer = setTimeout(() => {
      this.#timer = null;
      this.#open();
    }, delay);
  }
}
