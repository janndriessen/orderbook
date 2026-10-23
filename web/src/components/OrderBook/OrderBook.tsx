import { useMemo, useRef, useState } from "react";
import { type BookState, topRows } from "../../feed/book";
import type { LastTrade } from "../../feed/log";
import type { MarketInfo } from "../../protocol/MarketInfo";
import { BookSide } from "./BookSide";
import styles from "./OrderBook.module.css";
import { SpreadBar } from "./SpreadBar";
import { useRowsPerSide } from "./useRowsPerSide";

/**
 * Levels shown per side when the window is tall enough. The panel keeps this
 * many slots on each side even when a side is thinner, so the spread bar never
 * moves. The service's feed has no depth cap, so a deeper book is cut here.
 */
const ROWS_PER_SIDE = 20;

type Props = {
  book: BookState;
  market: MarketInfo;
  /** Newest trade seen on the feed; `null` before the first. */
  lastTrade: LastTrade | null;
};

export function OrderBook({ book, market, lastTrade }: Props) {
  const ref = useRef<HTMLElement>(null);
  const slots = useRowsPerSide(ref, ROWS_PER_SIDE);
  const [base = "", quote = ""] = market.symbol.split("-");

  const { asks, bids, largest } = useMemo(() => {
    const asks = topRows(book.asks, "ask", slots);
    const bids = topRows(book.bids, "bid", slots);
    const largest = Math.max(asks.at(-1)?.total ?? 0, bids.at(-1)?.total ?? 0);
    return { asks, bids, largest };
  }, [book.asks, book.bids, slots]);

  // Every price of the book, visible or not. A row flashes only if its price
  // was not in the previous book, so a taller panel or a vanished better
  // level never lights up a price that was already there. The previous book is
  // kept as state adjusted during render, which a repeated render sees the
  // same way.
  const prices = useMemo(
    () => new Set([...book.asks.keys(), ...book.bids.keys()]),
    [book.asks, book.bids],
  );
  const [seen, setSeen] = useState<{
    prices: ReadonlySet<number>;
    previous: ReadonlySet<number> | null;
  }>({ prices, previous: null });
  if (seen.prices !== prices) setSeen({ prices, previous: seen.prices });

  return (
    <section
      ref={ref}
      className={styles.book}
      aria-label={`${market.symbol} order book`}
    >
      <table className={styles.table}>
        <thead>
          <tr>
            <th scope="col">Price ({quote})</th>
            <th scope="col">Size ({base})</th>
            <th scope="col">Total ({base})</th>
          </tr>
        </thead>
        <BookSide
          side="ask"
          rows={asks}
          slots={slots}
          maxTotal={largest}
          known={seen.previous}
          market={market}
        />
        <SpreadBar
          bestBid={bids[0]?.price}
          bestAsk={asks[0]?.price}
          lastTrade={lastTrade}
          market={market}
        />
        <BookSide
          side="bid"
          rows={bids}
          slots={slots}
          maxTotal={largest}
          known={seen.previous}
          market={market}
        />
      </table>
    </section>
  );
}
