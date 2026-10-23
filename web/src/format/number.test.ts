import { describe, expect, it } from "vitest";
import {
  formatClock,
  formatSpreadPercent,
  formatUnits,
  isoTime,
} from "./number";

describe("formatUnits", () => {
  it.each([
    [1_234_567, 1, "123,456.7"],
    [5, 1, "0.5"],
    [0, 1, "0.0"],
    [0, 0, "0"],
    [1_000, 0, "1,000"],
    [7, 3, "0.007"],
    [12, 8, "0.00000012"],
    [100, 2, "1.00"],
    [-1_050, 2, "-10.50"],
    [-5, 3, "-0.005"],
  ])("formats %i with %i decimals as %s", (units, decimals, expected) => {
    expect(formatUnits(units, decimals)).toBe(expected);
  });

  it("stays exact at the largest safe integer", () => {
    expect(formatUnits(Number.MAX_SAFE_INTEGER, 2)).toBe(
      "90,071,992,547,409.91",
    );
  });
});

describe("formatSpreadPercent", () => {
  it("shows the spread relative to the mid price", () => {
    expect(formatSpreadPercent(99_999, 100_000)).toBe("0.001%");
    expect(formatSpreadPercent(1_000_000, 1_000_050)).toBe("0.005%");
  });

  it("never shows less than 0.001%", () => {
    expect(formatSpreadPercent(1_000_000, 1_000_001)).toBe("0.001%");
  });

  it("shows a locked book as zero and a crossed one as negative", () => {
    expect(formatSpreadPercent(100_000, 100_000)).toBe("0.000%");
    expect(formatSpreadPercent(100_010, 100_000)).toBe("-0.010%");
  });
});

describe("formatClock", () => {
  it("renders UTC time with milliseconds from microseconds", () => {
    expect(formatClock(1_700_000_000_123_456)).toBe("22:13:20.123");
  });

  it("pads small values", () => {
    expect(formatClock(5_000)).toBe("00:00:00.005");
  });
});

describe("isoTime", () => {
  it("converts microseconds to an ISO timestamp", () => {
    expect(isoTime(1_700_000_000_123_456)).toBe("2023-11-14T22:13:20.123Z");
  });
});
