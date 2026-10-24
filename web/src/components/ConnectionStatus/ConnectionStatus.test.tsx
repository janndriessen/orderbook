import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ConnectionStatus } from "./ConnectionStatus";

describe("ConnectionStatus", () => {
  it("reads Live when live and open", () => {
    render(<ConnectionStatus status="live" tradingStatus="open" />);
    expect(screen.getByRole("status", { name: "" })).toHaveTextContent(
      /^Live$/,
    );
  });

  it("reads Halted when live and halted", () => {
    render(<ConnectionStatus status="live" tradingStatus="halted" />);
    expect(screen.getByRole("status")).toHaveTextContent(
      "Halted · source reconnecting",
    );
  });

  it.each([
    ["connecting", "Connecting"],
    ["reconnecting", "Reconnecting"],
  ] as const)("keeps %s whatever the trading status", (status, text) => {
    render(<ConnectionStatus status={status} tradingStatus="halted" />);
    expect(screen.getByRole("status")).toHaveTextContent(
      new RegExp(`^${text}$`),
    );
  });
});
