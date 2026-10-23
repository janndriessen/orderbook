import { act, render, screen, within } from "@testing-library/react";
import { StrictMode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { applyMessage, type BookState } from "../../feed/book";
import type { LastTrade } from "../../feed/log";
import { level, market, snapshot } from "../../test/fixtures";
import { stubViewport } from "../../test/layout";
import { OrderBook } from "./OrderBook";

function bookOf(message: ReturnType<typeof snapshot>): BookState {
  const result = applyMessage(null, message);
  if (!result) throw new Error("unexpected gap");
  return result;
}

function cells(row: HTMLElement) {
  return within(row)
    .getAllByRole("cell")
    .map((cell) => cell.textContent);
}

describe("OrderBook", () => {
  beforeEach(() => {
    // Room for more than twenty 20 px rows per side.
    stubViewport(1_000);
  });

  it("labels the columns with the market's currencies", () => {
    render(
      <OrderBook book={bookOf(snapshot())} market={market} lastTrade={null} />,
    );
    const headers = screen
      .getAllByRole("columnheader")
      .map((h) => h.textContent);
    expect(headers).toEqual(["Price (USD)", "Size (BTC)", "Total (BTC)"]);
  });

  it("shows asks above the spread with the best ask closest to it", () => {
    render(
      <OrderBook book={bookOf(snapshot())} market={market} lastTrade={null} />,
    );
    const asks = within(screen.getByRole("rowgroup", { name: "Asks" }));
    expect(asks.getAllByRole("row").map((r) => cells(r))).toEqual([
      ["100,015.0", "0.500", "4.500"],
      ["100,010.0", "2.500", "4.000"],
      ["100,005.0", "1.500", "1.500"],
    ]);
  });

  it("shows bids below the spread, best first, with cumulative totals", () => {
    render(
      <OrderBook book={bookOf(snapshot())} market={market} lastTrade={null} />,
    );
    const bids = within(screen.getByRole("rowgroup", { name: "Bids" }));
    expect(bids.getAllByRole("row").map((r) => cells(r))).toEqual([
      ["100,000.0", "1.000", "1.000"],
      ["99,995.0", "2.000", "3.000"],
      ["99,990.0", "3.000", "6.000"],
    ]);
  });

  it("shows the spread in price and as a percentage of the mid", () => {
    render(
      <OrderBook book={bookOf(snapshot())} market={market} lastTrade={null} />,
    );
    const bar = within(screen.getByRole("rowgroup", { name: "Spread" }));
    expect(bar.getByText("5.0")).toBeInTheDocument();
    expect(bar.getByText("0.005%")).toBeInTheDocument();
  });

  it("shows placeholders instead of numbers when a side is empty", () => {
    render(
      <OrderBook
        book={bookOf(snapshot({ asks: [] }))}
        market={market}
        lastTrade={null}
      />,
    );
    const bar = within(screen.getByRole("rowgroup", { name: "Spread" }));
    expect(bar.getAllByText("–")).toHaveLength(2);
  });

  const deep = (start: number, step: number) =>
    Array.from({ length: 25 }, (_, i) => level(start + i * step, 1));
  const deepBook = () =>
    bookOf(snapshot({ bids: deep(1_000, -1), asks: deep(1_001, 1) }));
  const rowCount = (name: string) =>
    within(screen.getByRole("rowgroup", { name })).getAllByRole("row").length;

  it("shows twenty rows per side of a deep book on a tall viewport", () => {
    render(<OrderBook book={deepBook()} market={market} lastTrade={null} />);
    expect([rowCount("Asks"), rowCount("Bids")]).toEqual([20, 20]);
  });

  it("shows fewer rows per side when the viewport is too short for twenty", () => {
    // 20 + 20 fixed, then 6 rows of 20 px: three per side.
    stubViewport(160);
    render(<OrderBook book={deepBook()} market={market} lastTrade={null} />);
    expect([rowCount("Asks"), rowCount("Bids")]).toEqual([3, 3]);
  });

  it("keeps twenty slots per side so the spread bar does not move", () => {
    // jsdom has no layout and tests do not load stylesheets, so the blank
    // slots' height cannot be measured. The count the stylesheet turns into
    // height (--empty-rows) is checked instead.
    const slots = (name: string) => {
      const side = screen.getByRole("rowgroup", { name });
      const empty = Number(side.style.getPropertyValue("--empty-rows"));
      return rowCount(name) + empty;
    };
    const { rerender } = render(
      <OrderBook book={bookOf(snapshot())} market={market} lastTrade={null} />,
    );
    expect([slots("Asks"), slots("Bids")]).toEqual([20, 20]);

    rerender(
      <OrderBook
        book={bookOf(snapshot({ bids: [level(1_000_000, 1_000)] }))}
        market={market}
        lastTrade={null}
      />,
    );
    expect(rowCount("Bids")).toBe(1);
    expect([slots("Asks"), slots("Bids")]).toEqual([20, 20]);
  });

  describe("depth bars", () => {
    const depths = (name: string) =>
      within(screen.getByRole("rowgroup", { name }))
        .getAllByRole("row")
        .map((row) => Number(row.style.getPropertyValue("--depth")));

    it("are each row's total over the larger of the sides' last totals", () => {
      render(
        <OrderBook
          book={bookOf(snapshot())}
          market={market}
          lastTrade={null}
        />,
      );
      // Bids total 6,000 lots, asks 4,500; asks render furthest first.
      expect(depths("Bids")).toEqual([1_000 / 6_000, 3_000 / 6_000, 1]);
      expect(depths("Asks")).toEqual([
        4_500 / 6_000,
        4_000 / 6_000,
        1_500 / 6_000,
      ]);
    });

    it("measure against the visible rows only", () => {
      // Three rows per side fit; the deeper levels are cut off.
      stubViewport(160);
      render(<OrderBook book={deepBook()} market={market} lastTrade={null} />);
      expect(depths("Bids")).toEqual([1 / 3, 2 / 3, 1]);
    });

    it("change at once when the book shrinks, with no easing", () => {
      const thin = bookOf(
        snapshot({ bids: [level(1_000_000, 1_000)], asks: [] }),
      );
      const { rerender } = render(
        <OrderBook
          book={bookOf(snapshot())}
          market={market}
          lastTrade={null}
        />,
      );
      expect(depths("Bids")[0]).toBeCloseTo(1_000 / 6_000);

      rerender(<OrderBook book={thin} market={market} lastTrade={null} />);
      expect(depths("Bids")).toEqual([1]);
    });
  });

  it("gives every body row one cell per header column and nothing else", () => {
    // Stylesheets are not loaded in tests, so a generated cell from a
    // pseudo-element on a row cannot be seen here; this guards the markup and
    // that the depth lives on the row itself as a custom property. The
    // browser check covers the CSS.
    render(
      <OrderBook book={bookOf(snapshot())} market={market} lastTrade={null} />,
    );
    const columns = screen.getAllByRole("columnheader").length;
    const rows = screen
      .getAllByRole("row")
      .filter((row) => row.style.getPropertyValue("--depth") !== "");
    expect(rows).toHaveLength(6);
    for (const row of rows) {
      expect(row.children).toHaveLength(columns);
      expect(Array.from(row.children).every((c) => c.tagName === "TD")).toBe(
        true,
      );
    }
  });

  describe("flash on new levels", () => {
    const view = (book: BookState) => (
      <OrderBook book={book} market={market} lastTrade={null} />
    );
    const run = (bids: number[], asks: number[] = [], qty = 1_000) =>
      bookOf(
        snapshot({
          bids: bids.map((p) => level(p, qty)),
          asks: asks.map((p) => level(p, qty)),
        }),
      );
    // Delay step per row, best first, as the row hands it to the stylesheet;
    // null where the row does not flash.
    const steps = (name: string) =>
      within(screen.getByRole("rowgroup", { name }))
        .getAllByRole("row")
        .map((row) => row.style.getPropertyValue("--flash-step-index"))
        .map((step) => (step === "" ? null : Number(step)));
    const rowCountOf = (name: string) =>
      within(screen.getByRole("rowgroup", { name })).getAllByRole("row").length;

    it("does not flash the first book", () => {
      render(view(run([1_000, 999], [1_001, 1_002])));
      expect(steps("Bids")).toEqual([null, null]);
      expect(steps("Asks")).toEqual([null, null]);
    });

    it("flashes only the price that was not in the previous book", () => {
      const { rerender } = render(view(run([1_000, 999], [1_001, 1_002])));
      rerender(view(run([1_000, 999, 998], [1_001, 1_002, 1_003])));
      expect(steps("Bids")).toEqual([null, null, 4]);
      expect(steps("Asks")).toEqual([4, null, null]);
    });

    it("does not flash a size change", () => {
      const { rerender } = render(view(run([1_000, 999], [1_001])));
      rerender(view(run([1_000, 999], [1_001], 5_000)));
      expect(steps("Bids")).toEqual([null, null]);
      expect(steps("Asks")).toEqual([null]);
    });

    it("steps each slot by its place in the scattered order", () => {
      const { rerender } = render(view(run([1_000])));
      const prices = Array.from({ length: 12 }, (_, i) => 1_000 - i);
      rerender(view(run(prices)));
      const order = [0, 8, 4, 3, 2, 5, 7, 1, 9, 6];
      const expected = prices.map((_, slot) =>
        slot === 0 ? null : (order[slot % 10] ?? 0),
      );
      expect(steps("Bids")).toEqual(expected);
    });

    it("keeps a level's flash while later updates arrive", () => {
      const { rerender } = render(view(run([1_000])));
      rerender(view(run([1_000, 999])));
      rerender(view(run([1_000, 999], [], 2_000)));
      expect(steps("Bids")).toEqual([null, 8]);
    });

    it("keeps a row's step when its slot shifts", () => {
      const { rerender } = render(view(run([1_000])));
      rerender(view(run([1_000, 999])));
      // 999 moves from slot 1 to slot 2 but keeps the step it started with.
      rerender(view(run([1_001, 1_000, 999])));
      expect(steps("Bids")).toEqual([0, null, 8]);
    });

    it("flashes a new price exactly once under StrictMode", () => {
      const { rerender } = render(
        <StrictMode>{view(run([1_000], [1_005]))}</StrictMode>,
      );
      rerender(<StrictMode>{view(run([1_001, 1_000], [1_005]))}</StrictMode>);
      expect(steps("Bids")).toEqual([0, null]);
      expect(steps("Asks")).toEqual([null]);
    });

    it("does not flash a price that was on the other side before", () => {
      const { rerender } = render(view(run([1_001, 1_000], [1_002])));
      rerender(view(run([1_000], [1_001, 1_002])));
      expect(steps("Asks")).toEqual([null, null]);
    });

    it("flashes a price that left the book and came back", () => {
      const { rerender } = render(view(run([1_000, 999, 998])));
      rerender(view(run([1_000, 999])));
      rerender(view(run([1_000, 999, 998])));
      expect(steps("Bids")).toEqual([null, null, 4]);
    });

    it("still flashes a price that is new to the whole book", () => {
      stubViewport(80);
      const { rerender } = render(view(run([1_000, 999], [1_005])));
      rerender(view(run([1_001, 1_000, 999], [1_005])));
      expect(steps("Bids")).toEqual([0]);
    });

    it("does not flash rows revealed by a taller panel", () => {
      // 20 + 20 fixed, then 2 rows of 20 px: one per side.
      stubViewport(80);
      let resize = () => {};
      vi.stubGlobal(
        "ResizeObserver",
        class {
          constructor(callback: () => void) {
            resize = callback;
          }
          observe() {}
          unobserve() {}
          disconnect() {}
        },
      );
      render(view(run([1_000, 999, 998], [1_001, 1_002, 1_003])));
      expect(steps("Bids")).toEqual([null]);

      vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(
        1_000,
      );
      act(() => resize());
      expect(rowCountOf("Bids")).toBe(3);
      expect(steps("Bids")).toEqual([null, null, null]);
      expect(steps("Asks")).toEqual([null, null, null]);
    });

    it("does not flash a level that moves into view from deeper in the book", () => {
      stubViewport(80);
      const { rerender } = render(view(run([1_000, 999, 998], [1_001, 1_002])));
      expect(steps("Bids")).toEqual([null]);
      // The best bid goes; 999 was already in the book, just out of sight.
      rerender(view(run([999, 998], [1_001, 1_002])));
      expect(steps("Bids")).toEqual([null]);
      rerender(view(run([998], [1_001, 1_002])));
      expect(steps("Bids")).toEqual([null]);
    });
  });

  describe("last trade", () => {
    const slot = () => {
      const bar = screen.getByRole("rowgroup", { name: "Spread" });
      const found = bar.querySelector("[data-last-trade]");
      if (!found) throw new Error("no last-trade slot");
      return found;
    };
    const renderWith = (lastTrade: LastTrade | null) =>
      render(
        <OrderBook
          book={bookOf(snapshot())}
          market={market}
          lastTrade={lastTrade}
        />,
      );

    // Stylesheets are not loaded in tests, so the color rule is checked
    // through the attribute it selects on.
    it("shows a buy as an up arrow on the bid side", () => {
      renderWith({ price: 1_000_020, side: "buy" });
      expect(slot()).toHaveTextContent("Last ▲100,002.0 buyer");
      expect(slot().querySelector("[data-side='buy']")).not.toBeNull();
    });

    it("shows a sell as a down arrow on the ask side", () => {
      renderWith({ price: 999_990, side: "sell" });
      expect(slot()).toHaveTextContent("Last ▼99,999.0 seller");
      expect(slot().querySelector("[data-side='sell']")).not.toBeNull();
    });

    it("keeps its slot, empty, before the first trade", () => {
      renderWith(null);
      expect(slot()).toBeEmptyDOMElement();
    });
  });
});
