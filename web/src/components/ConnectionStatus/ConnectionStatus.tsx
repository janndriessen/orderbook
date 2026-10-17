import type { FeedStatus } from "../../feed/FeedHandler";
import styles from "./ConnectionStatus.module.css";

const LABELS: Record<FeedStatus, string> = {
  connecting: "Connecting",
  live: "Live",
  reconnecting: "Reconnecting",
};

export function ConnectionStatus({ status }: { status: FeedStatus }) {
  return (
    <output className={styles.status} data-status={status}>
      <span className={styles.dot} aria-hidden="true" />
      {LABELS[status]}
    </output>
  );
}
