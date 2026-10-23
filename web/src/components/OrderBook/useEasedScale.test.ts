import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  decayScale,
  HALF_LIFE_MS,
  TICK_MS,
  useEasedScale,
} from "./useEasedScale";

describe("decayScale", () => {
  it("halves the scale every half-life", () => {
    expect(decayScale(800, HALF_LIFE_MS)).toBeCloseTo(400);
    expect(decayScale(800, 2 * HALF_LIFE_MS)).toBeCloseTo(200);
  });

  it("leaves the scale alone when no time has passed", () => {
    expect(decayScale(800, 0)).toBe(800);
  });

  it("snaps a scale below one lot to zero", () => {
    expect(decayScale(1, 2 * HALF_LIFE_MS)).toBe(0);
  });
});

describe("useEasedScale", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  const advance = (ms: number) => act(() => vi.advanceTimersByTime(ms));

  it("rises to a larger total at once", () => {
    const { result, rerender } = renderHook(
      ({ largest }) => useEasedScale(largest),
      { initialProps: { largest: 100 } },
    );
    rerender({ largest: 400 });
    expect(result.current).toBe(400);
  });

  it("falls gradually, never below the current maximum, and settles on it", () => {
    const { result, rerender } = renderHook(
      ({ largest }) => useEasedScale(largest),
      { initialProps: { largest: 1_000 } },
    );
    rerender({ largest: 100 });
    expect(result.current).toBe(1_000);

    advance(HALF_LIFE_MS);
    expect(result.current).toBeGreaterThan(100);
    expect(result.current).toBeLessThan(1_000);

    advance(20 * HALF_LIFE_MS);
    expect(result.current).toBe(100);
  });

  it("keeps easing after the inputs stop changing", () => {
    const { result, rerender } = renderHook(
      ({ largest }) => useEasedScale(largest),
      { initialProps: { largest: 1_000 } },
    );
    rerender({ largest: 0 });
    advance(TICK_MS);
    const first = result.current;
    advance(TICK_MS);
    expect(result.current).toBeLessThan(first);
  });
});
