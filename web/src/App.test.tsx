import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { accepted, snapshot } from "./test/fixtures";
import { stubViewport } from "./test/layout";

class FakeWebSocket {
  static instances: FakeWebSocket[] = [];
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  close() {}
  constructor() {
    FakeWebSocket.instances.push(this);
  }
}

const socket = () => {
  const latest = FakeWebSocket.instances.at(-1);
  if (!latest) throw new Error("no socket opened");
  return latest;
};

const deliver = (message: unknown) =>
  act(() => socket().onmessage?.({ data: JSON.stringify(message) }));

describe("App", () => {
  beforeEach(() => {
    stubViewport(160);
    FakeWebSocket.instances = [];
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });

  it("shows the connection state and waits for the first snapshot", () => {
    render(<App />);
    expect(screen.getByRole("status")).toHaveTextContent("Connecting");
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });

  it("renders the book once the feed is live", () => {
    render(<App />);
    deliver(snapshot());
    expect(screen.getByRole("status")).toHaveTextContent("Live");
    expect(
      screen.getByRole("region", { name: "BTC-USD order book" }),
    ).toBeInTheDocument();
  });

  it("opens and closes the flow log from the toggle", async () => {
    const user = userEvent.setup();
    render(<App />);
    deliver(snapshot({ events: [accepted(1)] }));

    // Closed, the panel is parked off-screen and inert, so it cannot take focus.
    const panel = screen.getByRole("complementary", {
      name: "Flow log",
      hidden: true,
    });
    expect(panel).toHaveAttribute("inert");
    const toggle = screen.getByRole("button", { name: "Flow log" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");

    await user.click(toggle);
    expect(panel).not.toHaveAttribute("inert");
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(panel).getAllByRole("listitem")).toHaveLength(1);

    await user.click(toggle);
    expect(panel).toHaveAttribute("inert");
  });

  it("keeps the flow log and its filter across a reconnect", async () => {
    const user = userEvent.setup();
    render(<App />);
    deliver(snapshot({ events: [accepted(1)] }));
    await user.click(screen.getByRole("button", { name: "Flow log" }));
    await user.click(screen.getByRole("radio", { name: "Trades" }));

    act(() => socket().onclose?.());
    expect(screen.getByRole("status")).toHaveTextContent("Reconnecting");
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "Trades" })).toBeChecked();
  });
});
