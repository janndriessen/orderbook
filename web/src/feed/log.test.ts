import { describe, expect, it } from "vitest";
import { accepted, canceled, l2, replaced, trade } from "../test/fixtures";
import {
  appendEvents,
  MAX_LOG_ENTRIES,
  matchesFilter,
  newestTrade,
} from "./log";

describe("appendEvents", () => {
  it("puts the newest event first", () => {
    const log = appendEvents([], [accepted(1), trade(2)]);
    expect(log.map((e) => e.seq)).toEqual([2, 1]);
    expect(appendEvents(log, [canceled(3)]).map((e) => e.seq)).toEqual([
      3, 2, 1,
    ]);
  });

  it("drops book diffs", () => {
    const log = appendEvents([], [accepted(1), l2(2), trade(3)]);
    expect(log.map((e) => e.seq)).toEqual([3, 1]);
  });

  it("returns the same array when nothing is added", () => {
    const log = appendEvents([], [accepted(1)]);
    expect(appendEvents(log, [])).toBe(log);
    expect(appendEvents(log, [l2(2)])).toBe(log);
  });

  it("keeps at most the newest entries", () => {
    const events = Array.from({ length: MAX_LOG_ENTRIES + 20 }, (_, i) =>
      accepted(i + 1),
    );
    const log = appendEvents([], events);
    expect(log).toHaveLength(MAX_LOG_ENTRIES);
    expect(log[0]?.seq).toBe(MAX_LOG_ENTRIES + 20);
    expect(log.at(-1)?.seq).toBe(21);
    const more = appendEvents(log, [accepted(9_999)]);
    expect(more).toHaveLength(MAX_LOG_ENTRIES);
    expect(more[0]?.seq).toBe(9_999);
  });
});

describe("matchesFilter", () => {
  const entries = [accepted(1), trade(2), replaced(3), canceled(4)];
  const pick = (filter: Parameters<typeof matchesFilter>[1]) =>
    entries.filter((e) => matchesFilter(e, filter)).map((e) => e.seq);

  it("keeps trades, or everything else, or all", () => {
    expect(pick("all")).toEqual([1, 2, 3, 4]);
    expect(pick("trades")).toEqual([2]);
    expect(pick("orders")).toEqual([1, 3, 4]);
  });
});

describe("newestTrade", () => {
  it("is the newest trade of events given oldest first, with its taker side", () => {
    const events = [
      trade(1, "sell", 999_000),
      accepted(2),
      trade(3, "buy", 1_001_000),
      accepted(4),
    ];
    expect(newestTrade(events)).toEqual({ price: 1_001_000, side: "buy" });
  });

  it("is null without a trade", () => {
    expect(newestTrade([accepted(1)])).toBeNull();
  });
});
