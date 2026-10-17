import type { Envelope } from "../protocol/Envelope";
import type { Event } from "../protocol/Event";
import type { FeedMessage } from "../protocol/FeedMessage";
import type { Level } from "../protocol/Level";
import type { MarketInfo } from "../protocol/MarketInfo";

export const market: MarketInfo = {
  symbol: "BTC-USD",
  price_decimals: 1,
  qty_decimals: 3,
};

export function level(price: number, qty: number): Level {
  return { price, qty };
}

export function envelope<T extends Event>(seq: number, body: T): Envelope<T> {
  return { seq, ts: 1_700_000_000_000_000 + seq * 1_000, body };
}

export const accepted = (seq: number) =>
  envelope(seq, {
    type: "accepted",
    order_id: seq,
    side: "buy",
    price: 1_000_000,
    qty: 500,
  });

export const trade = (seq: number) =>
  envelope(seq, {
    type: "trade",
    maker_order_id: 1,
    taker_order_id: seq,
    aggressor_side: "sell",
    price: 1_000_000,
    qty: 250,
  });

export const replaced = (seq: number) =>
  envelope(seq, { type: "replaced", order_id: seq, price: 999_990, qty: 100 });

export const canceled = (seq: number) =>
  envelope(seq, { type: "canceled", order_id: seq, leaves_qty: 75 });

export const l2 = (seq: number) =>
  envelope(seq, { type: "l2_update", side: "buy", price: 1_000_000, qty: 1 });

type Parts = {
  seq?: number;
  bids?: Level[];
  asks?: Level[];
  events?: Envelope<Event>[];
};

/** Bids 100,000.0 down and asks 100,000.5 up, in 0.5 steps (1 tick = 0.1). */
export function snapshot(parts: Parts = {}): FeedMessage {
  return {
    type: "snapshot",
    seq: parts.seq ?? 1,
    market,
    bids: parts.bids ?? [
      level(1_000_000, 1_000),
      level(999_950, 2_000),
      level(999_900, 3_000),
    ],
    asks: parts.asks ?? [
      level(1_000_050, 1_500),
      level(1_000_100, 2_500),
      level(1_000_150, 500),
    ],
    events: parts.events ?? [],
  };
}

export function update(parts: Parts = {}): FeedMessage {
  return {
    type: "update",
    seq: parts.seq ?? 2,
    bids: parts.bids ?? [],
    asks: parts.asks ?? [],
    events: parts.events ?? [],
  };
}
