import { type CSSProperties, useState } from "react";
import { formatUnits } from "../../format/number";
import type { MarketInfo } from "../../protocol/MarketInfo";
import styles from "./BookRow.module.css";

/**
 * Delay steps by slot, repeating every ten. The scrambled order makes a batch
 * of new levels light up in a scattered pattern rather than as a sweep. The
 * stylesheet turns a step into time.
 */
const FLASH_STEPS = [0, 8, 4, 3, 2, 5, 7, 1, 9, 6];

type Props = {
  side: "bid" | "ask";
  price: number;
  qty: number;
  total: number;
  /** Cumulative total that a full-width depth bar stands for. */
  maxTotal: number;
  /** Whether the price is new to the book; read once, when the row mounts. */
  isNew: boolean;
  /** Position counted from the best price outward; sets the flash delay. */
  slot: number;
  market: MarketInfo;
};

export function BookRow({
  side,
  price,
  qty,
  total,
  maxTotal,
  isNew,
  slot,
  market,
}: Props) {
  // Under reduced motion the duration tokens make the flash zero-length, so
  // no separate opt-out is needed here. Decided once per mount: a flash runs for its full length however many
  // updates arrive meanwhile, and a row that shifts to another slot keeps its
  // step. The row is keyed by price, so a level that disappears and comes
  // back counts as new again.
  const [flashStep] = useState(() =>
    isNew ? (FLASH_STEPS[slot % FLASH_STEPS.length] ?? 0) : null,
  );
  const depth = maxTotal > 0 ? total / maxTotal : 0;
  const className = side === "bid" ? styles.bid : styles.ask;
  return (
    <tr
      className={
        flashStep === null ? className : `${className} ${styles.flash}`
      }
      style={
        {
          "--depth": depth,
          "--flash-step-index": flashStep ?? undefined,
        } as CSSProperties
      }
    >
      <td className={styles.price}>
        {formatUnits(price, market.price_decimals)}
      </td>
      <td>{formatUnits(qty, market.qty_decimals)}</td>
      <td>{formatUnits(total, market.qty_decimals)}</td>
    </tr>
  );
}
