import { useMemo, useRef } from "react";
import { type BookState, topRows } from "../../feed/book";
import type { MarketInfo } from "../../protocol/MarketInfo";
import { BookSide } from "./BookSide";
import styles from "./OrderBook.module.css";
import { SpreadBar } from "./SpreadBar";
import { useRowsPerSide } from "./useRowsPerSide";

/**
 * Levels shown per side when the window is tall enough. The panel keeps this
 * many slots on each side even when a side is thinner, so the spread bar never
 * moves. A replayed Hyperliquid book has at most 20 levels per side and is
 * usually deeper than 12; the service's own feed has no depth cap.
 */
const ROWS_PER_SIDE = 12;

type Props = {
  book: BookState;
  market: MarketInfo;
};

export function OrderBook({ book, market }: Props) {
  const ref = useRef<HTMLElement>(null);
  const slots = useRowsPerSide(ref, ROWS_PER_SIDE);
  const [base = "", quote = ""] = market.symbol.split("-");

  const { asks, bids, maxTotal } = useMemo(() => {
    const asks = topRows(book.asks, "ask", slots);
    const bids = topRows(book.bids, "bid", slots);
    const maxTotal = Math.max(asks.at(-1)?.total ?? 0, bids.at(-1)?.total ?? 0);
    return { asks, bids, maxTotal };
  }, [book.asks, book.bids, slots]);

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
          maxTotal={maxTotal}
          market={market}
        />
        <SpreadBar
          bestBid={bids[0]?.price}
          bestAsk={asks[0]?.price}
          market={market}
        />
        <BookSide
          side="bid"
          rows={bids}
          slots={slots}
          maxTotal={maxTotal}
          market={market}
        />
      </table>
    </section>
  );
}
