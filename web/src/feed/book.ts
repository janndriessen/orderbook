import type { FeedMessage } from "../protocol/FeedMessage";
import type { Level } from "../protocol/Level";

/** Resting quantity in lots, keyed by price in ticks. */
export type Levels = ReadonlyMap<number, number>;

/** Aggregated book as last confirmed by the feed. */
export type BookState = {
  /** Feed sequence number of the last message applied. */
  seq: number;
  bids: Levels;
  asks: Levels;
};

/** One displayed level: its own quantity plus everything better than it. */
export type Row = {
  price: number;
  qty: number;
  total: number;
};

function toMap(levels: Level[]): Map<number, number> {
  const map = new Map<number, number>();
  for (const { price, qty } of levels) {
    if (qty > 0) map.set(price, qty);
  }
  return map;
}

function patch(levels: Levels, changes: Level[]): Levels {
  if (changes.length === 0) return levels;
  const next = new Map(levels);
  for (const { price, qty } of changes) {
    if (qty === 0) next.delete(price);
    else next.set(price, qty);
  }
  return next;
}

/**
 * Applies one feed message to the held book.
 *
 * A snapshot always replaces the state. An update is accepted only when its
 * `seq` is exactly one past the held one; anything else (including an update
 * before any snapshot) is a gap, reported as `null` so the caller can resync.
 */
export function applyMessage(
  book: BookState | null,
  message: Exclude<FeedMessage, { type: "heartbeat" }>,
): BookState | null {
  if (message.type === "snapshot") {
    return {
      seq: message.seq,
      bids: toMap(message.bids),
      asks: toMap(message.asks),
    };
  }
  if (book === null || message.seq !== book.seq + 1) return null;
  return {
    seq: message.seq,
    bids: patch(book.bids, message.bids),
    asks: patch(book.asks, message.asks),
  };
}

/**
 * The `count` best levels of one side with cumulative totals, best first.
 *
 * The side is sorted and then cut to the visible depth before summing; a
 * level's total only includes better levels, so the cut does not change it.
 */
export function topRows(
  levels: Levels,
  side: "bid" | "ask",
  count: number,
): Row[] {
  const sorted = [...levels.entries()].sort(([a], [b]) =>
    side === "bid" ? b - a : a - b,
  );
  const rows: Row[] = [];
  let total = 0;
  for (const [price, qty] of sorted.slice(0, count)) {
    total += qty;
    rows.push({ price, qty, total });
  }
  return rows;
}
