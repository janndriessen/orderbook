import type { CSSProperties } from "react";
import type { Row } from "../../feed/book";
import type { MarketInfo } from "../../protocol/MarketInfo";
import { BookRow } from "./BookRow";
import styles from "./BookSide.module.css";

type Props = {
  side: "bid" | "ask";
  /** Best level first, at most `slots` long. */
  rows: Row[];
  /** Rows the side occupies; unused ones stay empty so the spread stays put. */
  slots: number;
  maxTotal: number;
  market: MarketInfo;
};

export function BookSide({ side, rows, slots, maxTotal, market }: Props) {
  const rendered = rows.map((row) => (
    <BookRow
      key={row.price}
      side={side}
      price={row.price}
      qty={row.qty}
      total={row.total}
      maxTotal={maxTotal}
      market={market}
    />
  ));
  const style = { "--empty-rows": slots - rows.length } as CSSProperties;

  // Asks sit above the spread with the best one at the bottom.
  return side === "ask" ? (
    <tbody aria-label="Asks" className={styles.asks} style={style}>
      {rendered.reverse()}
    </tbody>
  ) : (
    <tbody aria-label="Bids" className={styles.bids} style={style}>
      {rendered}
    </tbody>
  );
}
