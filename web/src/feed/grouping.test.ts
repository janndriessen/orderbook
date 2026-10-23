import { describe, expect, it } from "vitest";
import { groupLevels, groupWidths } from "./grouping";

describe("groupWidths", () => {
  it("offers 1, 2, 5, 10, 100 and 1000 ticks for a five-digit price", () => {
    expect(groupWidths(99_995)).toEqual([1, 2, 5, 10, 100, 1_000]);
  });

  it("starts at no grouping for a six-digit price and goes coarser", () => {
    expect(groupWidths(100_000)).toEqual([1, 10, 20, 50, 100, 1_000, 10_000]);
  });

  it("never offers a width below one tick for a short price", () => {
    expect(groupWidths(9_999)).toEqual([1, 10, 100]);
    expect(groupWidths(5)).toEqual([1]);
  });

  it("offers no grouping when there is no price yet", () => {
    expect(groupWidths(undefined)).toEqual([1]);
  });

  it("reads an ask on its own one tick lower", () => {
    // A touch at exactly 100,000 keeps the five-digit options of 99,999.
    expect(groupWidths(undefined, 100_000)).toEqual(groupWidths(99_999));
    expect(groupWidths(undefined, 100_001)).toEqual(groupWidths(100_000));
  });

  it("prefers the best bid when both sides exist", () => {
    expect(groupWidths(99_999, 100_000)).toEqual(groupWidths(99_999));
  });
});

describe("groupLevels", () => {
  const levels = new Map([
    [1_007, 1],
    [1_003, 2],
    [1_000, 4],
    [995, 8],
  ]);

  it("rounds bids down to the bucket and sums their sizes", () => {
    expect(groupLevels(levels, "bid", 10)).toEqual(
      new Map([
        [1_000, 7],
        [990, 8],
      ]),
    );
  });

  it("rounds asks up to the bucket and sums their sizes", () => {
    expect(groupLevels(levels, "ask", 10)).toEqual(
      new Map([
        [1_010, 3],
        [1_000, 12],
      ]),
    );
  });

  it("rounds to widths that are not powers of ten", () => {
    expect(groupLevels(new Map([[1_007, 1]]), "bid", 5)).toEqual(
      new Map([[1_005, 1]]),
    );
    expect(groupLevels(new Map([[1_003, 1]]), "ask", 2)).toEqual(
      new Map([[1_004, 1]]),
    );
    expect(groupLevels(new Map([[1_004, 1]]), "ask", 2)).toEqual(
      new Map([[1_004, 1]]),
    );
  });

  it("returns the levels untouched for a one-tick width", () => {
    expect(groupLevels(levels, "bid", 1)).toBe(levels);
  });
});
