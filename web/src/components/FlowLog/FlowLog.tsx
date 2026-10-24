import { useState } from "react";
import {
  type LogEntry,
  type LogFilter,
  MAX_ORDER_ENTRIES,
  MAX_TRADE_ENTRIES,
  matchesFilter,
} from "../../feed/log";
import type { MarketInfo } from "../../protocol/MarketInfo";
import hidden from "../../styles/a11y.module.css";
import { FlowEntry } from "./FlowEntry";
import styles from "./FlowLog.module.css";

type Props = {
  /** Newest first. */
  entries: readonly LogEntry[];
  market: MarketInfo;
  /**
   * Changes when the feed's server restarted. A paused view is dropped then:
   * the new log's sequence numbers start over below the frozen view's, which
   * would hide every new event from the count. A resync from the same server
   * keeps the pause.
   */
  restarts?: number;
};

const FILTERS: { value: LogFilter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "trades", label: "Trades" },
  { value: "orders", label: "Orders" },
];

const isTrade = (entry: LogEntry) => entry.body.type === "trade";

/**
 * Entries that arrived after the frozen view and pass the filter. The two
 * kinds are capped separately, so a kind may have lost entries newer than the
 * frozen mark unless it still holds one at or before the mark, or is below its
 * cap (nothing of it was ever dropped). The count is a lower bound otherwise.
 */
function newSince(
  entries: readonly LogEntry[],
  frozen: readonly LogEntry[],
  filter: LogFilter,
) {
  const mark = frozen[0]?.seq ?? -1;
  const index = entries.findIndex((entry) => entry.seq <= mark);
  const fresh = index === -1 ? entries : entries.slice(0, index);
  return {
    count: fresh.filter((entry) => matchesFilter(entry, filter)).length,
    complete: [true, false].every(
      (trades) =>
        entries.some((e) => isTrade(e) === trades && e.seq <= mark) ||
        entries.filter((e) => isTrade(e) === trades).length <
          (trades ? MAX_TRADE_ENTRIES : MAX_ORDER_ENTRIES),
    ),
  };
}

export function FlowLog({ entries, market, restarts = 0 }: Props) {
  const [filter, setFilter] = useState<LogFilter>("all");
  // While paused the list shows the entries held at the moment of pausing; the
  // feed keeps filling `entries`, so resuming loses nothing.
  const [held, setHeld] = useState<{
    entries: readonly LogEntry[];
    restarts: number;
  } | null>(null);
  const frozen = held?.restarts === restarts ? held.entries : null;

  const shown = (frozen ?? entries).filter((e) => matchesFilter(e, filter));
  const pending = frozen ? newSince(entries, frozen, filter) : null;

  return (
    <div className={styles.log}>
      <header className={styles.toolbar}>
        <fieldset className={styles.filters}>
          <legend className={hidden.visuallyHidden}>Show</legend>
          {FILTERS.map(({ value, label }) => (
            <label key={value} className={styles.filter}>
              <input
                type="radio"
                name="flow-filter"
                value={value}
                checked={filter === value}
                onChange={() => setFilter(value)}
              />
              <span>{label}</span>
            </label>
          ))}
        </fieldset>
        {pending && pending.count > 0 && (
          <span className={styles.pending}>
            {pending.count}
            {pending.complete ? "" : "+"} new
          </span>
        )}
        <button
          type="button"
          className={styles.pause}
          onClick={() => setHeld(frozen ? null : { entries, restarts })}
        >
          {frozen ? "Resume" : "Pause"}
        </button>
      </header>
      {shown.length === 0 ? (
        <p className={styles.empty}>No events to show.</p>
      ) : (
        <ol className={styles.list}>
          {shown.map((entry) => (
            <FlowEntry key={entry.seq} entry={entry} market={market} />
          ))}
        </ol>
      )}
    </div>
  );
}
