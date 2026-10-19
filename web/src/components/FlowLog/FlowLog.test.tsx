import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { appendEvents, type LogEntry } from "../../feed/log";
import {
  accepted,
  canceled,
  market,
  replaced,
  trade,
} from "../../test/fixtures";
import { FlowLog } from "./FlowLog";

function logOf(...events: Parameters<typeof appendEvents>[1]) {
  return appendEvents([], events);
}

function items() {
  return within(screen.getByRole("list")).queryAllByRole("listitem");
}

describe("FlowLog", () => {
  const entries = logOf(accepted(1), trade(2), replaced(3), canceled(4));

  it("lists every flow event, newest first, with its own label", () => {
    render(<FlowLog entries={entries} market={market} />);
    const text = items().map((item) => item.textContent ?? "");
    expect(text).toHaveLength(4);
    expect(text[0]).toContain("Canceled");
    expect(text[1]).toContain("Replaced");
    expect(text[2]).toContain("Trade");
    expect(text[3]).toContain("Accepted");
  });

  it("formats quantities and prices with the market's decimals", () => {
    render(<FlowLog entries={logOf(accepted(1))} market={market} />);
    expect(items()[0]).toHaveTextContent("Buy 0.500 @ 100,000.0");
  });

  it.each([
    [
      "an immediate-or-cancel limit order with an IOC tag",
      { type: "limit", price: 1_000_000, time_in_force: "ioc" },
      "Buy 0.500 @ 100,000.0 IOC#1",
    ],
    [
      "a post-only limit order with a PO tag",
      { type: "limit", price: 1_000_000, time_in_force: "post_only" },
      "Buy 0.500 @ 100,000.0 PO#1",
    ],
    [
      "a market order as MKT instead of a price",
      { type: "market" },
      "Buy 0.500 MKT#1",
    ],
  ] as const)("shows %s", (_name, orderType, text) => {
    render(<FlowLog entries={logOf(accepted(1, orderType))} market={market} />);
    expect(items()[0]).toHaveTextContent(`Accepted${text}`);
  });

  it("explains the tags in a tooltip", () => {
    const orderType = { type: "market" } as const;
    render(<FlowLog entries={logOf(accepted(1, orderType))} market={market} />);
    expect(screen.getByText("MKT")).toHaveAttribute("title", "Market order");
  });

  it("filters to trades and back", async () => {
    const user = userEvent.setup();
    render(<FlowLog entries={entries} market={market} />);
    await user.click(screen.getByRole("radio", { name: "Trades" }));
    expect(items()).toHaveLength(1);
    expect(items()[0]).toHaveTextContent("Trade");
    await user.click(screen.getByRole("radio", { name: "Orders" }));
    expect(items()).toHaveLength(3);
    await user.click(screen.getByRole("radio", { name: "All" }));
    expect(items()).toHaveLength(4);
  });

  it("says so when the filter matches nothing", async () => {
    const user = userEvent.setup();
    render(<FlowLog entries={logOf(accepted(1))} market={market} />);
    await user.click(screen.getByRole("radio", { name: "Trades" }));
    expect(screen.getByText("No events to show.")).toBeInTheDocument();
  });

  it("freezes the list on pause, counts what it missed and catches up on resume", async () => {
    const user = userEvent.setup();
    const { rerender } = render(<FlowLog entries={entries} market={market} />);
    await user.click(screen.getByRole("button", { name: "Pause" }));

    const newer: readonly LogEntry[] = appendEvents(entries, [
      accepted(5),
      trade(6),
    ]);
    rerender(<FlowLog entries={newer} market={market} />);
    expect(items()).toHaveLength(4);
    expect(screen.getByText("2 new")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Resume" }));
    expect(items()).toHaveLength(6);
    expect(screen.queryByText(/new$/)).not.toBeInTheDocument();
  });

  it("counts only missed events that pass the filter", async () => {
    const user = userEvent.setup();
    const { rerender } = render(<FlowLog entries={entries} market={market} />);
    await user.click(screen.getByRole("radio", { name: "Trades" }));
    await user.click(screen.getByRole("button", { name: "Pause" }));
    rerender(
      <FlowLog
        entries={appendEvents(entries, [accepted(5), trade(6)])}
        market={market}
      />,
    );
    expect(screen.getByText("1 new")).toBeInTheDocument();
  });
});
