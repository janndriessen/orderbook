import { useEffect, useRef } from "react";
import styles from "./FlowToggle.module.css";

type Props = {
  open: boolean;
  onToggle: () => void;
};

function milliseconds(element: Element, token: string): number {
  return Number.parseFloat(getComputedStyle(element).getPropertyValue(token));
}

/**
 * Fades the button out while it jumps between the page corner and the log
 * toolbar, and back in once the panes have settled. Reduced motion sets the
 * durations to zero, which skips the fade.
 */
function hop(button: HTMLButtonElement) {
  const total = milliseconds(button, "--duration-pane");
  const fade = milliseconds(button, "--duration-fast");
  if (!(total > 0 && fade > 0)) return;
  button.animate(
    [
      { opacity: 1 },
      { opacity: 0, offset: fade / total },
      { opacity: 0, offset: 1 - fade / total },
      { opacity: 1 },
    ],
    { duration: total },
  );
}

export function FlowToggle({ open, onToggle }: Props) {
  const ref = useRef<HTMLButtonElement>(null);
  const shown = useRef(open);

  useEffect(() => {
    if (shown.current === open || !ref.current) return;
    shown.current = open;
    hop(ref.current);
  }, [open]);

  return (
    <button
      ref={ref}
      type="button"
      className={styles.toggle}
      aria-label="Flow log"
      aria-expanded={open}
      onClick={onToggle}
    >
      <svg className={styles.icon} viewBox="0 0 16 16" aria-hidden="true">
        <path d="M3 4h10M3 8h10M3 12h6" />
      </svg>
    </button>
  );
}
