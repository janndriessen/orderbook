import { type RefObject, useLayoutEffect, useState } from "react";

function pixels(style: CSSStyleDeclaration, name: string): number {
  return Number.parseFloat(style.getPropertyValue(name));
}

/**
 * How many price levels per side fit in the element's height, at most `max`.
 * On a normal screen that is `max`; on a short window the panel shows fewer
 * rows rather than being clipped or scrolled. The row, header and spread
 * heights come from the CSS tokens, so the stylesheet stays the single place
 * that defines them. Both sides get the same count so the spread bar stays
 * centred.
 */
export function useRowsPerSide(
  ref: RefObject<HTMLElement | null>,
  max: number,
): number {
  const [rows, setRows] = useState(max);

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
      const free = element.clientHeight - fixed;
      const fit = Math.floor(free / pixels(style, "--row-height") / 2);
      setRows(Number.isFinite(fit) ? Math.max(0, Math.min(max, fit)) : 0);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, [ref, max]);

  return rows;
}
