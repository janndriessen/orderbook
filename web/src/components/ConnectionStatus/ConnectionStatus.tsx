import type { FeedStatus } from "../../feed/FeedHandler";
import type { TradingStatus } from "../../protocol/TradingStatus";
import styles from "./ConnectionStatus.module.css";

const LABELS: Record<FeedStatus | "halted", string> = {
  connecting: "Connecting",
  live: "Live",
  reconnecting: "Reconnecting",
  halted: "Halted \u00b7 source reconnecting",
};

type Props = { status: FeedStatus; tradingStatus: TradingStatus | null };

export function ConnectionStatus({ status, tradingStatus }: Props) {
  // A halt is only news on a live feed; otherwise the connection state says
  // more, and the last known trading status may be stale.
  const shown =
    status === "live" && tradingStatus === "halted" ? "halted" : status;
  return (
    <output className={styles.status} data-status={shown}>
      <span className={styles.dot} aria-hidden="true" />
      {LABELS[shown]}
    </output>
  );
}
