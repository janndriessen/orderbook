import { useState } from "react";
import styles from "./App.module.css";
import { ConnectionStatus } from "./components/ConnectionStatus/ConnectionStatus";
import { FlowLog } from "./components/FlowLog/FlowLog";
import { FlowToggle } from "./components/FlowLog/FlowToggle";
import { OrderBook } from "./components/OrderBook/OrderBook";
import { useFeed } from "./feed/useFeed";

export function App() {
  const { status, book, market, log, lastTrade, restarts, tradingStatus } =
    useFeed();
  const [logOpen, setLogOpen] = useState(false);
  // Held here, not in the book, which unmounts while the feed resyncs.
  const [width, setWidth] = useState<number | null>(null);

  return (
    <div className={styles.layout} data-log-open={logOpen}>
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
      <aside className={styles.logPane} inert={!logOpen} aria-label="Flow log">
        <div className={styles.logSlot}>
          {market && (
            <FlowLog entries={log} market={market} restarts={restarts} />
          )}
        </div>
      </aside>
      <FlowToggle open={logOpen} onToggle={() => setLogOpen((open) => !open)} />
    </div>
  );
}
