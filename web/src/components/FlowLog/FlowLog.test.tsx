import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { appendEvents, type LogEntry, MAX_TRADE_ENTRIES } from "../../feed/log";
import {
  accepted,
  canceled,
  cancelRejected,
  market,
  rejected,
  replaced,
  statusChanged,
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

  it.each([
    ["halted", "Trading halted"],
    ["open", "Trading open"],
  ] as const)("words a status change to %s as %s", (status, text) => {
    render(
      <FlowLog entries={logOf(statusChanged(1, status))} market={market} />,
    );
    expect(items()[0]).toHaveTextContent(`Status${text}`);
  });

  it("explains the tags in a tooltip", () => {
    const orderType = { type: "market" } as const;
    render(<FlowLog entries={logOf(accepted(1, orderType))} market={market} />);
    expect(screen.getByText("MKT")).toHaveAttribute("title", "Market order");
  });

  it.each([
    ["post_only_would_cross", "post-only would cross"],
    ["no_liquidity", "no liquidity"],
    ["trading_halted", "trading halted"],
  ] as const)("shows a rejected order's reason %s in words", (reason, text) => {
    render(<FlowLog entries={logOf(rejected(1, reason))} market={market} />);
    expect(items()[0]).toHaveTextContent(`Rejected${text}#1`);
  });

  it("shows a refused cancel with its reason, under Orders but not Trades", async () => {
    const user = userEvent.setup();
    const log = logOf(cancelRejected(1, "unknown_order"), trade(2));
    render(<FlowLog entries={log} market={market} />);
    await user.click(screen.getByRole("radio", { name: "Orders" }));
    expect(items()).toHaveLength(1);
    expect(items()[0]).toHaveTextContent("Cancel rejectedunknown order#1");
    await user.click(screen.getByRole("radio", { name: "Trades" }));
    expect(items()).toHaveLength(1);
    expect(items()[0]).toHaveTextContent("Trade");
  });

  it("labels a refused replace as such", () => {
    const entry = cancelRejected(1, "post_only_would_cross", "replace");
    render(<FlowLog entries={logOf(entry)} market={market} />);
    expect(items()[0]).toHaveTextContent(
      "Replace rejectedpost-only would cross#1",
    );
  });

  it("lists rejected orders under Orders and hides them under Trades", async () => {
    const user = userEvent.setup();
    const log = logOf(rejected(1, "post_only_would_cross"), trade(2));
    render(<FlowLog entries={log} market={market} />);
    await user.click(screen.getByRole("radio", { name: "Orders" }));
    expect(items()).toHaveLength(1);
    expect(items()[0]).toHaveTextContent("Rejected");
    await user.click(screen.getByRole("radio", { name: "Trades" }));
    expect(items()).toHaveLength(1);
    expect(items()[0]).toHaveTextContent("Trade");
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

  it("goes live again when the server restarts", async () => {
    const user = userEvent.setup();
    const high = logOf(accepted(100), trade(101));
    const { rerender } = render(<FlowLog entries={high} market={market} />);
    await user.click(screen.getByRole("button", { name: "Pause" }));

    // A restarted server numbers its events from 1 again.
    const restarted = logOf(accepted(1), trade(2), canceled(3));
    rerender(<FlowLog entries={restarted} market={market} restarts={1} />);
    expect(screen.getByRole("button", { name: "Pause" })).toBeInTheDocument();
    expect(items()).toHaveLength(3);
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

  it("marks the count as a lower bound once one kind has been trimmed", async () => {
    const user = userEvent.setup();
    const frozen = logOf(trade(1));
    const { rerender } = render(<FlowLog entries={frozen} market={market} />);
    await user.click(screen.getByRole("button", { name: "Pause" }));
    // Far below the combined size of both caps, yet the trade list is full and
    // has dropped the frozen trade.
    const flood = Array.from({ length: MAX_TRADE_ENTRIES + 1 }, (_, i) =>
      trade(2 + i),
    );
    rerender(<FlowLog entries={appendEvents(frozen, flood)} market={market} />);
    expect(screen.getByText(`${MAX_TRADE_ENTRIES}+ new`)).toBeInTheDocument();
  });
});
