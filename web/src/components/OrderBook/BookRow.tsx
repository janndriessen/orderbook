import { type CSSProperties, useState } from "react";
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
  // Count size changes while this row is mounted. Adjusting state during
  // render, rather than in an effect, avoids painting a frame with the new
  // size and the old count. The row is keyed by price, so a level that
  // disappears and comes back starts again at zero.
  const [seen, setSeen] = useState({ qty, changes: 0 });
  let changes = seen.changes;
  if (seen.qty !== qty) {
    changes += 1;
    setSeen({ qty, changes });
  }
  const depth = maxTotal > 0 ? total / maxTotal : 0;
  return (
    <tr
      className={side === "bid" ? styles.bid : styles.ask}
      style={{ "--depth": depth } as CSSProperties}
    >
      <td className={styles.price}>
        {formatUnits(price, market.price_decimals)}
      </td>
      <td>
        {/* A new key remounts the span, which restarts its flash animation. */}
        <span
          key={changes}
          className={changes > 0 ? styles.flash : undefined}
          data-changes={changes}
        >
          {formatUnits(qty, market.qty_decimals)}
        </span>
      </td>
      <td>{formatUnits(total, market.qty_decimals)}</td>
    </tr>
  );
}
