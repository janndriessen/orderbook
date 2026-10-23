import { useState } from "react";
import styles from "./App.module.css";
import { ConnectionStatus } from "./components/ConnectionStatus/ConnectionStatus";
import { FlowLog } from "./components/FlowLog/FlowLog";
import { FlowToggle } from "./components/FlowLog/FlowToggle";
import { OrderBook } from "./components/OrderBook/OrderBook";
import { useFeed } from "./feed/useFeed";

export function App() {
  const { status, book, market, log, lastTrade, restarts } = useFeed();
  const [logOpen, setLogOpen] = useState(false);

  return (
    <div className={styles.layout} data-log-open={logOpen}>
      <ConnectionStatus status={status} />
      <main className={styles.bookPane}>
        <div className={styles.bookSlot}>
          {book && market ? (
            <OrderBook book={book} market={market} lastTrade={lastTrade} />
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
