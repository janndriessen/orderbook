import type { Envelope } from "../protocol/Envelope";
import type { Event } from "../protocol/Event";
import type { Side } from "../protocol/Side";

/** Order-flow events shown in the log; book diffs are not part of it. */
export type FlowEvent = Exclude<Event, { type: "l2_update" }>;
export type LogEntry = Envelope<FlowEvent>;

/** Entries kept in memory and rendered; older ones are dropped. */
export const MAX_LOG_ENTRIES = 500;

export type LogFilter = "all" | "trades" | "orders";

function isFlow(envelope: Envelope<Event>): envelope is LogEntry {
  return envelope.body.type !== "l2_update";
}

/**
 * Builds the log newest first from events given oldest first. The feed already
 * omits book diffs; they are dropped again here so the log cannot depend on it.
 */
export function appendEvents(
  log: readonly LogEntry[],
  events: readonly Envelope<Event>[],
): readonly LogEntry[] {
  const flow = events.filter(isFlow);
  if (flow.length === 0) return log;
  return [...flow.reverse(), ...log].slice(0, MAX_LOG_ENTRIES);
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
