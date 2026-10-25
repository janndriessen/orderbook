import { useState } from "react";
import styles from "./App.module.css";
import { ConnectionStatus } from "./components/ConnectionStatus/ConnectionStatus";
import { FlowLog } from "./components/FlowLog/FlowLog";
import { FlowToggle } from "./components/FlowLog/FlowToggle";
import { OrderBook } from "./components/OrderBook/OrderBook";
import { useFeed } from "./feed/useFeed";
import { useMediaQuery } from "./useMediaQuery";

/**
 * The window width below which the panes stack. Keep in sync with the media
 * queries in App.module.css and FlowLog.module.css, which cannot read a shared
 * value.
 */
const STACKED_QUERY = "(width < 1064px)";

export function App() {
  const { status, book, market, log, lastTrade, restarts, tradingStatus } =
    useFeed();
  const [logOpen, setLogOpen] = useState(false);
  // Stacked, the log sits below the book and needs no toggle.
  const stacked = useMediaQuery(STACKED_QUERY);
  const logShown = stacked || logOpen;
  // Held here, not in the book, which unmounts while the feed resyncs.
  const [width, setWidth] = useState<number | null>(null);

  return (
    <div className={styles.layout} data-log-open={logShown}>
      <ConnectionStatus status={status} tradingStatus={tradingStatus} />
      <main className={styles.bookPane}>
        <div className={styles.bookSlot}>
          {market ? (
            <OrderBook
              book={book}
              market={market}
              lastTrade={lastTrade}
              width={width}
              onWidthChange={setWidth}
            />
          ) : (
            <p className={styles.waiting}>Waiting for the feed</p>
          )}
        </div>
      </main>
      <aside className={styles.logPane} inert={!logShown} aria-label="Flow log">
        <div className={styles.logSlot}>
          {market && (
            <FlowLog entries={log} market={market} restarts={restarts} />
          )}
        </div>
      </aside>
      {!stacked && (
        <FlowToggle
          open={logOpen}
          onToggle={() => setLogOpen((open) => !open)}
        />
      )}
    </div>
  );
}
