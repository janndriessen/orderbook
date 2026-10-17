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
    // 20 + 20 fixed, then 6 rows of 20 px: three per side.
    stubViewport(160);
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

  it("splits the rows that fit between the sides, asks taking any odd one", () => {
    stubViewport(100);
    render(
      <OrderBook
        book={bookOf(
          snapshot({
            bids: [level(10, 1), level(9, 1), level(8, 1)],
            asks: [level(11, 1), level(12, 1), level(13, 1)],
          }),
        )}
        market={market}
      />,
    );
    const asks = within(screen.getByRole("rowgroup", { name: "Asks" }));
    const bids = within(screen.getByRole("rowgroup", { name: "Bids" }));
    expect(asks.getAllByRole("row")).toHaveLength(2);
    expect(bids.getAllByRole("row")).toHaveLength(1);
  });
});
