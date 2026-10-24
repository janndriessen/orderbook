import type { LastTrade } from "../../feed/log";
import { formatSpreadPercent, formatUnits } from "../../format/number";
import type { MarketInfo } from "../../protocol/MarketInfo";
import hidden from "../../styles/a11y.module.css";
import styles from "./SpreadBar.module.css";

type Props = {
  bestBid: number | undefined;
  bestAsk: number | undefined;
  lastTrade: LastTrade | null;
  market: MarketInfo;
};

const EMPTY = "–";

export function SpreadBar({ bestBid, bestAsk, lastTrade, market }: Props) {
  const both = bestBid !== undefined && bestAsk !== undefined;
  const decimals = market.price_decimals;
  return (
    <tbody aria-label="Spread">
      <tr className={styles.bar}>
        <td colSpan={3}>
          <div className={styles.content}>
            <div className={styles.spread}>
              <span className={styles.label}>Spread</span>
              <span className={styles.value}>
                {both ? formatUnits(bestAsk - bestBid, decimals) : EMPTY}
              </span>
              <span className={styles.value}>
                {both ? formatSpreadPercent(bestBid, bestAsk) : EMPTY}
              </span>
            </div>
            {/* Always rendered, so the spread stays put when the first trade
                arrives. The taker side is shown by arrow and color and also
                said in words. */}
            <span className={styles.last} data-last-trade>
              {lastTrade && (
                <span data-side={lastTrade.side}>
                  <span className={styles.label}>Last </span>
                  <span className={styles.arrow} aria-hidden="true">
                    {lastTrade.side === "buy" ? "▲" : "▼"}
                  </span>
                  {formatUnits(lastTrade.price, decimals)}
                  <span className={hidden.visuallyHidden}>
                    {lastTrade.side === "buy" ? " buyer" : " seller"}
                  </span>
                </span>
              )}
            </span>
          </div>
        </td>
      </tr>
    </tbody>
  );
}
