import { formatSpreadPercent, formatUnits } from "../../format/number";
import type { MarketInfo } from "../../protocol/MarketInfo";
import styles from "./SpreadBar.module.css";

type Props = {
  bestBid: number | undefined;
  bestAsk: number | undefined;
  market: MarketInfo;
};

const EMPTY = "–";

export function SpreadBar({ bestBid, bestAsk, market }: Props) {
  const both = bestBid !== undefined && bestAsk !== undefined;
  const decimals = market.price_decimals;
  return (
    <tbody aria-label="Spread">
      <tr className={styles.bar}>
        <td colSpan={3}>
          <div className={styles.content}>
            <span className={styles.label}>Spread</span>
            <span className={styles.value}>
              {both ? formatUnits(bestAsk - bestBid, decimals) : EMPTY}
            </span>
            <span className={styles.value}>
              {both ? formatSpreadPercent(bestBid, bestAsk) : EMPTY}
            </span>
          </div>
        </td>
      </tr>
    </tbody>
  );
}
