import { render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { applyMessage, type BookState } from "../../feed/book";
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
    // Room for far more than twelve 20 px rows per side.
    stubViewport(1_000);
  });

  it("labels the columns with the market's currencies", () => {
    render(<OrderBook book={bookOf(snapshot())} market={market} />);
    const headers = screen
      .getAllByRole("columnheader")
      .map((h) => h.textContent);
    expect(headers).toEqual(["Price (USD)", "Size (BTC)", "Total (BTC)"]);
  });

  it("shows asks above the spread with the best ask closest to it", () => {
    render(<OrderBook book={bookOf(snapshot())} market={market} />);
    const asks = within(screen.getByRole("rowgroup", { name: "Asks" }));
    expect(asks.getAllByRole("row").map((r) => cells(r))).toEqual([
      ["100,015.0", "0.500", "4.500"],
      ["100,010.0", "2.500", "4.000"],
      ["100,005.0", "1.500", "1.500"],
    ]);
  });

  it("shows bids below the spread, best first, with cumulative totals", () => {
    render(<OrderBook book={bookOf(snapshot())} market={market} />);
    const bids = within(screen.getByRole("rowgroup", { name: "Bids" }));
    expect(bids.getAllByRole("row").map((r) => cells(r))).toEqual([
      ["100,000.0", "1.000", "1.000"],
      ["99,995.0", "2.000", "3.000"],
      ["99,990.0", "3.000", "6.000"],
    ]);
  });

  it("shows the spread in price and as a percentage of the mid", () => {
    render(<OrderBook book={bookOf(snapshot())} market={market} />);
    const bar = within(screen.getByRole("rowgroup", { name: "Spread" }));
    expect(bar.getByText("5.0")).toBeInTheDocument();
    expect(bar.getByText("0.005%")).toBeInTheDocument();
  });

  it("shows placeholders instead of numbers when a side is empty", () => {
    render(<OrderBook book={bookOf(snapshot({ asks: [] }))} market={market} />);
    const bar = within(screen.getByRole("rowgroup", { name: "Spread" }));
    expect(bar.getAllByText("–")).toHaveLength(2);
  });

  const deep = (start: number, step: number) =>
    Array.from({ length: 20 }, (_, i) => level(start + i * step, 1));
  const deepBook = () =>
    bookOf(snapshot({ bids: deep(1_000, -1), asks: deep(1_001, 1) }));
  const rowCount = (name: string) =>
    within(screen.getByRole("rowgroup", { name })).getAllByRole("row").length;

  it("shows twelve rows per side of a deep book on a tall viewport", () => {
    render(<OrderBook book={deepBook()} market={market} />);
    expect([rowCount("Asks"), rowCount("Bids")]).toEqual([12, 12]);
  });

  it("shows fewer rows per side when the viewport is too short for twelve", () => {
    // 20 + 20 fixed, then 6 rows of 20 px: three per side.
    stubViewport(160);
    render(<OrderBook book={deepBook()} market={market} />);
    expect([rowCount("Asks"), rowCount("Bids")]).toEqual([3, 3]);
  });

  it("keeps twelve slots per side so the spread bar does not move", () => {
    // jsdom has no layout and tests do not load stylesheets, so the blank
    // slots' height cannot be measured. The count the stylesheet turns into
    // height (--empty-rows) is checked instead.
    const slots = (name: string) => {
      const side = screen.getByRole("rowgroup", { name });
      const empty = Number(side.style.getPropertyValue("--empty-rows"));
      return rowCount(name) + empty;
    };
    const { rerender } = render(
      <OrderBook book={bookOf(snapshot())} market={market} />,
    );
    expect([slots("Asks"), slots("Bids")]).toEqual([12, 12]);

    rerender(
      <OrderBook
        book={bookOf(snapshot({ bids: [level(1_000_000, 1_000)] }))}
        market={market}
      />,
    );
    expect(rowCount("Bids")).toBe(1);
    expect([slots("Asks"), slots("Bids")]).toEqual([12, 12]);
  });
});
