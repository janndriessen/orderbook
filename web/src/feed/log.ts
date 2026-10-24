import type { Envelope } from "../protocol/Envelope";
import type { Event } from "../protocol/Event";
import type { Side } from "../protocol/Side";

/** Order-flow events shown in the log; book diffs are not part of it. */
export type FlowEvent = Exclude<Event, { type: "l2_update" }>;
export type LogEntry = Envelope<FlowEvent>;

/** Order events (everything but trades) kept in memory and rendered; older ones are dropped. */
export const MAX_ORDER_ENTRIES = 500;

/**
 * Trades kept in memory and rendered. Capped apart from the order events so
 * that the many order events a live book produces cannot push trades out.
 */
export const MAX_TRADE_ENTRIES = 100;

export type LogFilter = "all" | "trades" | "orders";

function isFlow(envelope: Envelope<Event>): envelope is LogEntry {
  return envelope.body.type !== "l2_update";
}

/**
 * Builds the log newest first from events given oldest first, keeping the
 * newest {@link MAX_TRADE_ENTRIES} trades and {@link MAX_ORDER_ENTRIES} other
 * events. The feed already omits book diffs; they are dropped again here so the
 * log cannot depend on it.
 */
export function appendEvents(
  log: readonly LogEntry[],
  events: readonly Envelope<Event>[],
): readonly LogEntry[] {
  const flow = events.filter(isFlow);
  if (flow.length === 0) return log;
  const merged = [...flow.reverse(), ...log];
  const trades = merged.filter((e) => e.body.type === "trade");
  const orders = merged.filter((e) => e.body.type !== "trade");
  // Each list is already newest first; merging by seq restores the common order.
  return [
    ...trades.slice(0, MAX_TRADE_ENTRIES),
    ...orders.slice(0, MAX_ORDER_ENTRIES),
  ].sort((a, b) => b.seq - a.seq);
}

export function matchesFilter(entry: LogEntry, filter: LogFilter): boolean {
  switch (filter) {
    case "all":
      return true;
    case "trades":
      return entry.body.type === "trade";
    case "orders":
      return entry.body.type !== "trade";
  }
}

/** Price and taker side of a trade. */
export type LastTrade = { price: number; side: Side };

/** The newest trade in events given oldest first, if there is one. */
export function newestTrade(
  events: readonly Envelope<Event>[],
): LastTrade | null {
  for (const { body } of events.toReversed()) {
    if (body.type === "trade") {
      return { price: body.price, side: body.aggressor_side };
    }
  }
  return null;
}
