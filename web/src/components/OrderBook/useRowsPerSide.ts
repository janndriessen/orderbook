import { type RefObject, useLayoutEffect, useState } from "react";

/** Price levels shown on each side of the spread. */
export type RowsPerSide = { asks: number; bids: number };

const NONE: RowsPerSide = { asks: 0, bids: 0 };

function pixels(style: CSSStyleDeclaration, name: string): number {
  return Number.parseFloat(style.getPropertyValue(name));
}

/**
 * How many price levels fit on each side of the spread in the element's
 * height. The row, header and spread heights come from the CSS tokens, so the
 * stylesheet stays the single place that defines them.
 *
 * Rows are counted over the whole height and split between the sides, which
 * may differ by one, so less than a row is left over. Those whole pixels go to
 * the spread bar through `--spread-extra`: rows keep an exact height and the
 * book still reaches the bottom edge.
 */
export function useRowsPerSide(
  ref: RefObject<HTMLElement | null>,
): RowsPerSide {
  const [rows, setRows] = useState(NONE);

  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    const measure = () => {
      const style = getComputedStyle(element);
      const fixed =
        pixels(style, "--header-height") +
        pixels(style, "--spread-height") +
        pixels(style, "padding-top") +
        pixels(style, "padding-bottom");
      const row = pixels(style, "--row-height");
      const free = element.clientHeight - fixed;
      const total = Math.max(0, Math.floor(free / row));
      if (!Number.isFinite(total)) {
        setRows(NONE);
        return;
      }
      const extra = Math.max(0, Math.floor(free - total * row));
      element.style.setProperty("--spread-extra", `${extra}px`);
      setRows((current) => {
        const asks = Math.ceil(total / 2);
        const bids = total - asks;
        return current.asks === asks && current.bids === bids
          ? current
          : { asks, bids };
      });
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, [ref]);

  return rows;
}
