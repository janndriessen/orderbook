import { useMemo, useRef, useState } from "react";
import { type BookState, topRows } from "../../feed/book";
import { groupLevels, groupWidths } from "../../feed/grouping";
import type { LastTrade } from "../../feed/log";
import type { MarketInfo } from "../../protocol/MarketInfo";
import { BookSide } from "./BookSide";
import { GroupSelect } from "./GroupSelect";
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
  /** The held book, or `null` while the feed resyncs. */
  book: BookState | null;
  market: MarketInfo;
  /** Newest trade seen on the feed; `null` before the first. */
  lastTrade: LastTrade | null;
  /**
   * Bucket width in ticks the user picked, or `null` before any choice. It
   * stays in force even when the price drifts to a digit count that does not
   * offer it, so a price hovering at a power of ten keeps its grouping.
   */
  width: number | null;
  onWidthChange: (width: number) => void;
};

function sameWidths(a: number[], b: number[]): boolean {
  return a.length === b.length && a.every((width, i) => width === b[i]);
}

export function OrderBook({
  book,
  market,
  lastTrade,
  width: chosen,
  onWidthChange,
}: Props) {
  const ref = useRef<HTMLElement>(null);
  const slots = useRowsPerSide(ref, ROWS_PER_SIDE);

  // The real book's best prices, not the grouped ones, drive the menu and the
  // spread bar.
  const best = useMemo(
    () => ({
      bid: book ? topRows(book.bids, "bid", 1)[0]?.price : undefined,
      ask: book ? topRows(book.asks, "ask", 1)[0]?.price : undefined,
    }),
    [book],
  );
  // The menu is part of the panel even without a book, so keyboard focus
  // survives a resync: it keeps the last widths, disabled. A chosen width the
  // price no longer offers is added to the options so the menu still shows it.
  const width = chosen ?? 1;
  const offered = book ? groupWidths(best.bid, best.ask) : null;
  const options =
    offered && !offered.includes(width)
      ? [...offered, width].sort((a, b) => a - b)
      : offered;
  const [last, setLast] = useState<number[]>([1]);
  if (options && !sameWidths(options, last)) setLast(options);

  return (
    <section
      ref={ref}
      className={styles.book}
      aria-label={`${market.symbol} order book`}
    >
      <GroupSelect
        widths={options ?? last}
        value={width}
        onChange={onWidthChange}
        idle={book === null}
        market={market}
      />
      {book ? (
        <BookTable
          book={book}
          market={market}
          lastTrade={lastTrade}
          width={width}
          slots={slots}
          bestBid={best.bid}
          bestAsk={best.ask}
        />
      ) : (
        <p className={styles.waiting}>Waiting for the feed</p>
      )}
    </section>
  );
}

type TableProps = {
  book: BookState;
  market: MarketInfo;
  lastTrade: LastTrade | null;
  width: number;
  slots: number;
  bestBid: number | undefined;
  bestAsk: number | undefined;
};

// A separate component so that what it remembers, the previous book for the
// flash, starts over when the feed resyncs and the book comes back.
function BookTable({
  book,
  market,
  lastTrade,
  width,
  slots,
  bestBid,
  bestAsk,
}: TableProps) {
  const [base = "", quote = ""] = market.symbol.split("-");

  const grouped = useMemo(
    () => ({
      asks: groupLevels(book.asks, "ask", width),
      bids: groupLevels(book.bids, "bid", width),
    }),
    [book.asks, book.bids, width],
  );
  const { asks, bids, largest } = useMemo(() => {
    const asks = topRows(grouped.asks, "ask", slots);
    const bids = topRows(grouped.bids, "bid", slots);
    const largest = Math.max(asks.at(-1)?.total ?? 0, bids.at(-1)?.total ?? 0);
    return { asks, bids, largest };
  }, [grouped, slots]);

  // Every price of the grouped book, visible or not. A row flashes only if its
  // price was not in the previous book, so a taller panel or a vanished better
  // level never lights up a price that was already there. The previous book is
  // kept as state adjusted during render, which a repeated render sees the
  // same way. A new width makes every bucket a different price, so the book
  // after a width change has no previous book and flashes nothing.
  const prices = useMemo(
    () => new Set([...grouped.asks.keys(), ...grouped.bids.keys()]),
    [grouped],
  );
  const [seen, setSeen] = useState<{
    prices: ReadonlySet<number>;
    width: number;
    previous: ReadonlySet<number> | null;
  }>({ prices, width, previous: null });
  if (seen.prices !== prices || seen.width !== width) {
    setSeen({
      prices,
      width,
      previous: seen.width === width ? seen.prices : null,
    });
  }

  return (
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
        bestBid={bestBid}
        bestAsk={bestAsk}
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
  );
}
