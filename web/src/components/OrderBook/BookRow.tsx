import type { CSSProperties } from "react";
import { formatUnits } from "../../format/number";
import type { MarketInfo } from "../../protocol/MarketInfo";
import styles from "./BookRow.module.css";

type Props = {
  side: "bid" | "ask";
  price: number;
  qty: number;
  total: number;
  /** Cumulative total that a full-width depth bar stands for. */
  maxTotal: number;
  market: MarketInfo;
};

export function BookRow({ side, price, qty, total, maxTotal, market }: Props) {
  const depth = maxTotal > 0 ? total / maxTotal : 0;
  return (
    <tr
      className={side === "bid" ? styles.bid : styles.ask}
      style={{ "--depth": depth } as CSSProperties}
    >
      <td className={styles.price}>
        {formatUnits(price, market.price_decimals)}
      </td>
      <td>{formatUnits(qty, market.qty_decimals)}</td>
      <td>{formatUnits(total, market.qty_decimals)}</td>
    </tr>
  );
}
