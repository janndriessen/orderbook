import { describe, expect, it } from "vitest";
import { level, snapshot, update } from "../test/fixtures";
import { applyMessage, type BookState, topRows } from "./book";

function apply(
  book: BookState | null,
  message: Parameters<typeof applyMessage>[1],
) {
  const result = applyMessage(book, message);
  if (!result) throw new Error("unexpected gap");
  return result;
}

function seeded(): BookState {
  return apply(null, snapshot());
}

describe("applyMessage", () => {
  it("builds the book from a snapshot", () => {
    const book = seeded();
    expect(book.seq).toBe(1);
    expect(book.bids.get(1_000_000)).toBe(1_000);
    expect(book.asks.get(1_000_050)).toBe(1_500);
  });

  it("replaces held state when a new snapshot arrives", () => {
    const second = apply(
      seeded(),
      snapshot({ seq: 50, bids: [level(5, 5)], asks: [] }),
    );
    expect([...second.bids]).toEqual([[5, 5]]);
    expect(second.asks.size).toBe(0);
    expect(second.seq).toBe(50);
  });

  it("changes an existing level and inserts a new one", () => {
    const book = seeded();
    const next = apply(
      book,
      update({
        bids: [level(1_000_000, 700), level(999_000, 9)],
        asks: [level(1_000_200, 4)],
      }),
    );
    expect(next.bids.get(1_000_000)).toBe(700);
    expect(next.bids.get(999_000)).toBe(9);
    expect(next.asks.get(1_000_200)).toBe(4);
    expect(next.seq).toBe(2);
  });

  it("removes a level at quantity zero", () => {
    const book = seeded();
    const next = apply(
      book,
      update({ bids: [level(1_000_000, 0)], asks: [level(1_000_050, 0)] }),
    );
    expect(next.bids.has(1_000_000)).toBe(false);
    expect(next.asks.has(1_000_050)).toBe(false);
    expect(next.bids.size).toBe(2);
  });

  it("ignores removal of a level that is not held", () => {
    const book = seeded();
    const next = apply(book, update({ bids: [level(1, 0)] }));
    expect(next.bids.size).toBe(3);
  });

  it("does not mutate the previous state", () => {
    const book = seeded();
    apply(book, update({ bids: [level(1_000_000, 0)] }));
    expect(book.bids.get(1_000_000)).toBe(1_000);
  });

  it("drops zero-quantity levels from a snapshot", () => {
    const book = apply(null, snapshot({ bids: [level(1, 0)], asks: [] }));
    expect(book.bids.size).toBe(0);
  });

  it("advances on an update that carries no changes", () => {
    const book = seeded();
    const next = apply(book, update());
    expect(next.seq).toBe(2);
    expect(next.bids).toBe(book.bids);
  });

  it.each([
    ["skips a sequence number", 3],
    ["repeats a sequence number", 1],
    ["goes backwards", 0],
  ])("reports a gap when an update %s", (_name, seq) => {
    const book = seeded();
    expect(applyMessage(book, update({ seq }))).toBeNull();
  });

  it("reports a gap for an update before any snapshot", () => {
    expect(applyMessage(null, update())).toBeNull();
  });
});

describe("topRows", () => {
  const book = seeded();

  it("sorts bids highest first with running totals", () => {
    expect(topRows(book.bids, "bid", 10)).toEqual([
      { price: 1_000_000, qty: 1_000, total: 1_000 },
      { price: 999_950, qty: 2_000, total: 3_000 },
      { price: 999_900, qty: 3_000, total: 6_000 },
    ]);
  });

  it("sorts asks lowest first with running totals", () => {
    expect(
      topRows(book.asks, "ask", 10).map((r) => [r.price, r.total]),
    ).toEqual([
      [1_000_050, 1_500],
      [1_000_100, 4_000],
      [1_000_150, 4_500],
    ]);
  });

  it("sorts levels that arrived out of order", () => {
    const levels = new Map([
      [3, 1],
      [1, 1],
      [2, 1],
    ]);
    expect(topRows(levels, "ask", 3).map((r) => r.price)).toEqual([1, 2, 3]);
    expect(topRows(levels, "bid", 3).map((r) => r.price)).toEqual([3, 2, 1]);
  });

  it("keeps only the best levels and totals them correctly", () => {
    expect(topRows(book.bids, "bid", 2).map((r) => r.total)).toEqual([
      1_000, 3_000,
    ]);
  });

  it("handles an empty side and a zero count", () => {
    expect(topRows(new Map(), "bid", 5)).toEqual([]);
    expect(topRows(book.bids, "bid", 0)).toEqual([]);
  });
});
