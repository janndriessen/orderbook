import type { Levels } from "./book";

/** Largest count of significant figures a grouping keeps. */
const MAX_FIGURES = 5;
/** Smallest count of significant figures a grouping keeps. */
const MIN_FIGURES = 2;
/** At the finest figure count, twice and five times the base width are offered too. */
const FINE_MULTIPLES = [1, 2, 5];

/**
 * Bucket widths, in ticks, offered for a book, finest first.
 *
 * The digits come from the best bid, or from the best ask when there are no
 * bids. Each width keeps a number of significant figures of the price:
 * 10^(d - e) ticks keeps `e` of the price's `d` digits. A price of six or more
 * digits also offers no grouping (one tick); at five figures the widths 2 and
 * 5 times the base are offered as well. A width below one tick is never
 * offered, so a short price gets fewer options.
 *
 * An ask on its own is counted one tick lower, so a touch exactly on a power
 * of ten (100,000) keeps the digits of the price just below it, as the replay
 * adapter does when it reads the same grouping from a book side.
 */
export function groupWidths(
  bestBid: number | undefined,
  bestAsk?: number,
): number[] {
  const price = bestBid ?? (bestAsk === undefined ? undefined : bestAsk - 1);
  if (price === undefined || price < 1) return [1];
  const digits = Math.trunc(price).toString().length;
  const widths: number[] = digits >= MAX_FIGURES + 1 ? [1] : [];
  for (let figures = MAX_FIGURES; figures >= MIN_FIGURES; figures--) {
    if (digits < figures) continue;
    const base = 10 ** (digits - figures);
    for (const multiple of figures === MAX_FIGURES ? FINE_MULTIPLES : [1]) {
      widths.push(base * multiple);
    }
  }
  // A price under two digits keeps fewer figures than any option; the book
  // then stays ungrouped.
  return widths.length > 0 ? widths : [1];
}

/**
 * Sums a side's sizes into buckets `width` ticks wide, keyed by the bucket's
 * edge nearest the other side: bids round down and asks round up, so a grouped
 * price is never better than the best real order in it.
 */
export function groupLevels(
  levels: Levels,
  side: "bid" | "ask",
  width: number,
): Levels {
  if (width <= 1) return levels;
  const round = side === "bid" ? Math.floor : Math.ceil;
  const grouped = new Map<number, number>();
  for (const [price, qty] of levels) {
    const bucket = round(price / width) * width;
    grouped.set(bucket, (grouped.get(bucket) ?? 0) + qty);
  }
  return grouped;
}
