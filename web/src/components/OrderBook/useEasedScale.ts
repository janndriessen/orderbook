import { useEffect, useRef, useState } from "react";

/** Time for the depth scale to fall halfway toward a smaller total. */
export const HALF_LIFE_MS = 1_300;
/** Interval between decay steps; the bars' own transition fills the gaps. */
export const TICK_MS = 50;
/** Below one lot a depth bar is invisible, so the scale snaps to zero. */
const SETTLED = 1;

/** The scale after `elapsedMs` of exponential decay with `HALF_LIFE_MS`. */
export function decayScale(scale: number, elapsedMs: number): number {
  const decayed = scale * 0.5 ** (elapsedMs / HALF_LIFE_MS);
  return decayed < SETTLED ? 0 : decayed;
}

/**
 * The total that depth bars are measured against. It rises with the largest
 * visible total at once, so no bar overflows, but falls over time rather than
 * per update: every bar eases to its new width, and keeps easing after updates
 * stop (a finished replay, a paused feed) until the scale has settled on the
 * true maximum. The timer runs only while the scale is above it.
 */
export function useEasedScale(largest: number): number {
  const [scale, setScale] = useState(largest);
  const current = useRef(largest);

  useEffect(() => {
    current.current = Math.max(current.current, largest);
    setScale(current.current);
    if (current.current <= largest) return;
    const timer = setInterval(() => {
      current.current = Math.max(largest, decayScale(current.current, TICK_MS));
      setScale(current.current);
      if (current.current <= largest) clearInterval(timer);
    }, TICK_MS);
    return () => clearInterval(timer);
  }, [largest]);

  return Math.max(largest, scale);
}
