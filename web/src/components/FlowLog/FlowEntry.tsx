import { memo } from "react";
import type { LogEntry } from "../../feed/log";
import { formatClock, formatUnits, isoTime } from "../../format/number";
import type { MarketInfo } from "../../protocol/MarketInfo";
import styles from "./FlowEntry.module.css";

type Props = {
  entry: LogEntry;
  market: MarketInfo;
};

const LABELS = {
  accepted: "Accepted",
  trade: "Trade",
  replaced: "Replaced",
  canceled: "Canceled",
} as const;

function Detail({ entry, market }: Props) {
  const body = entry.body;
  const price = (ticks: number) => formatUnits(ticks, market.price_decimals);
  const qty = (lots: number) => formatUnits(lots, market.qty_decimals);
  switch (body.type) {
    case "accepted":
      return (
        <>
          <span className={styles[body.side]}>
            {body.side === "buy" ? "Buy" : "Sell"}
          </span>{" "}
          {qty(body.qty)} @ {price(body.price)}
          <span className={styles.ref}>#{body.order_id}</span>
        </>
      );
    case "trade":
      return (
        <>
          <span className={styles[body.aggressor_side]}>
            {body.aggressor_side === "buy" ? "Buy" : "Sell"}
          </span>{" "}
          {qty(body.qty)} @ {price(body.price)}
          <span className={styles.ref}>
            #{body.taker_order_id} x #{body.maker_order_id}
          </span>
        </>
      );
    case "replaced":
      return (
        <>
          {qty(body.qty)} @ {price(body.price)}
          <span className={styles.ref}>#{body.order_id}</span>
        </>
      );
    case "canceled":
      return (
        <>
          {qty(body.leaves_qty)} left
          <span className={styles.ref}>#{body.order_id}</span>
        </>
      );
  }
}

/** Entries never change once logged, so a row re-renders only for a new market. */
export const FlowEntry = memo(function FlowEntry({ entry, market }: Props) {
  const type = entry.body.type;
  return (
    <li className={styles.entry} data-type={type}>
      <time className={styles.time} dateTime={isoTime(entry.ts)}>
        {formatClock(entry.ts)}
      </time>
      <span className={styles.type}>{LABELS[type]}</span>
      <span className={styles.detail}>
        <Detail entry={entry} market={market} />
      </span>
    </li>
  );
});
