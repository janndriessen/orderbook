import { describe, expect, it } from "vitest";
import {
  accepted,
  l2,
  rejected,
  snapshot,
  trade,
  update,
} from "../test/fixtures";
import { parseFeedMessage } from "./guard";

const json = (value: unknown) => JSON.stringify(value);

describe("parseFeedMessage", () => {
  it("accepts a snapshot and an update", () => {
    const snap = snapshot({ events: [accepted(1), trade(2), l2(3)] });
    expect(parseFeedMessage(json(snap))).toEqual(snap);
    expect(parseFeedMessage(json(update()))).toEqual(update());
  });

  it("accepts a heartbeat", () => {
    const beat = { type: "heartbeat", seq: 42 };
    expect(parseFeedMessage(json(beat))).toEqual(beat);
  });

  it.each([
    { type: "limit", price: 5, time_in_force: "ioc" },
    { type: "limit", price: -5, time_in_force: "post_only" },
    { type: "market" },
  ] as const)("accepts an accepted order of type %j", (orderType) => {
    const msg = update({ events: [accepted(1, orderType)] });
    expect(parseFeedMessage(json(msg))).toEqual(msg);
  });

  it.each(["post_only_would_cross", "no_liquidity"] as const)(
    "accepts a rejected order with reason %s",
    (reason) => {
      const msg = update({ events: [rejected(1, reason)] });
      expect(parseFeedMessage(json(msg))).toEqual(msg);
    },
  );

  const withEvent = (body: unknown) =>
    json({ ...update(), events: [{ seq: 1, ts: 1, body }] });

  it.each([
    ["invalid JSON", "{"],
    ["a non-string payload", new ArrayBuffer(2)],
    ["an unknown message type", json({ ...update(), type: "nonsense" })],
    ["a heartbeat with a negative seq", json({ type: "heartbeat", seq: -1 })],
    ["a heartbeat without a seq", json({ type: "heartbeat" })],
    ["a fractional seq", json({ ...update(), seq: 2.5 })],
    [
      "a seq beyond 2^53",
      '{"type":"update","seq":9007199254740993,"bids":[],"asks":[],"events":[]}',
    ],
    [
      "a negative quantity",
      json({ ...update(), bids: [{ price: 1, qty: -1 }] }),
    ],
    ["an unknown event", withEvent({ type: "x" })],
    [
      "a rejection with an unknown reason",
      withEvent({ ...rejected(1, "post_only_would_cross").body, reason: "x" }),
    ],
    [
      "a rejection with a fractional order id",
      withEvent({
        ...rejected(1, "post_only_would_cross").body,
        order_id: 1.5,
      }),
    ],
    [
      "an accepted order without an order type",
      withEvent({ ...accepted(1).body, order_type: undefined }),
    ],
    [
      "an unknown order type",
      withEvent({
        ...accepted(1).body,
        order_type: { type: "stop", price: 1, time_in_force: "gtc" },
      }),
    ],
    [
      "a limit order without a price",
      withEvent({
        ...accepted(1).body,
        order_type: { type: "limit", time_in_force: "gtc" },
      }),
    ],
    [
      "a limit order with a fractional price",
      withEvent({
        ...accepted(1).body,
        order_type: { type: "limit", price: 1.5, time_in_force: "gtc" },
      }),
    ],
    [
      "a limit order with an unknown time in force",
      withEvent({
        ...accepted(1).body,
        order_type: { type: "limit", price: 1, time_in_force: "day" },
      }),
    ],
    ["a bad trade side", withEvent({ ...trade(1).body, aggressor_side: "up" })],
    ["a snapshot without a market", json({ ...snapshot(), market: undefined })],
    [
      "snapshot decimals out of range",
      json({
        ...snapshot(),
        market: { symbol: "X", price_decimals: 19, qty_decimals: 1 },
      }),
    ],
  ])("rejects %s", (_name, raw) => {
    expect(parseFeedMessage(raw)).toBeNull();
  });
});
