import { useState } from "react";
import { formatUnits } from "../../format/number";
import type { MarketInfo } from "../../protocol/MarketInfo";
import styles from "./GroupSelect.module.css";

type Props = {
  /** Offered bucket widths in ticks, finest first; one leaves nothing to pick. */
  widths: number[];
  /** The width in use; one of `widths`. */
  value: number;
  onChange: (width: number) => void;
  /** True while there is no book to group. */
  idle: boolean;
  market: MarketInfo;
};

/** Picks how many ticks wide each price row of the book is. */
export function GroupSelect({ widths, value, onChange, idle, market }: Props) {
  // The offered widths can change with every book update. While the menu has
  // focus the list is held still, so an open native popup never changes which
  // width a position means; it is released on blur and after a pick.
  const [held, setHeld] = useState<number[] | null>(null);
  const shown = held ?? widths;
  return (
    <span className={styles.menu}>
      <select
        className={styles.select}
        aria-label="Group prices"
        // While idle the menu stays focusable, which `disabled` would not
        // allow, so keyboard focus survives a resync; it just ignores changes.
        disabled={!idle && shown.length < 2}
        aria-disabled={idle || undefined}
        value={value}
        onFocus={() => setHeld(widths)}
        onBlur={() => setHeld(null)}
        onChange={(event) => {
          if (idle) return;
          setHeld(null);
          onChange(Number(event.target.value));
        }}
      >
        {shown.map((width) => (
          <option key={width} value={width}>
            {formatUnits(width, market.price_decimals)}
          </option>
        ))}
      </select>
    </span>
  );
}
