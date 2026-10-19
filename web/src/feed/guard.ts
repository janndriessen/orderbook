import type { FeedMessage } from "../protocol/FeedMessage";
import { isRejectReason } from "./rejectReasons";

type Dict = Record<string, unknown>;

const MAX_DECIMALS = 18;

function isDict(value: unknown): value is Dict {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Every key holds a safe integer of at least `min`; JSON numbers beyond 2^53 lose digits. */
function ints(
  item: Dict,
  keys: readonly string[],
  min = Number.MIN_SAFE_INTEGER,
) {
  return keys.every((key) => {
    const value = item[key];
    return Number.isSafeInteger(value) && (value as number) >= min;
  });
}

function all(value: unknown, check: (item: Dict) => boolean): boolean {
  return Array.isArray(value) && value.every((v) => isDict(v) && check(v));
}

const isSide = (value: unknown) => value === "buy" || value === "sell";

/** Per event type: its side field, its signed integer fields, its unsigned ones. */
const EVENTS: Record<string, [string | null, string[], string[]]> = {
  accepted: ["side", [], ["order_id", "qty"]],
  rejected: [null, [], ["order_id"]],
  trade: [
    "aggressor_side",
    ["price"],
    ["maker_order_id", "taker_order_id", "qty"],
  ],
  replaced: [null, ["price"], ["order_id", "qty"]],
  canceled: [null, [], ["order_id", "leaves_qty"]],
  l2_update: ["side", ["price"], ["qty"]],
};

function isOrderType(value: unknown): boolean {
  if (!isDict(value)) return false;
  if (value.type === "market") return true;
  return (
    value.type === "limit" &&
    ints(value, ["price"]) &&
    (value.time_in_force === "gtc" ||
      value.time_in_force === "ioc" ||
      value.time_in_force === "post_only")
  );
}

function isEvent(item: Dict): boolean {
  const body = item.body;
  if (!ints(item, ["seq", "ts"], 0) || !isDict(body)) return false;
  const spec =
    typeof body.type === "string" && Object.hasOwn(EVENTS, body.type)
      ? EVENTS[body.type]
      : undefined;
  if (!spec) return false;
  const [side, signed, unsigned] = spec;
  return (
    (side === null || isSide(body[side])) &&
    // Extra keys are ignored: the server is strict about what it accepts, the
    // client lenient about what it reads.
    (body.type !== "accepted" || isOrderType(body.order_type)) &&
    (body.type !== "rejected" || isRejectReason(body.reason)) &&
    ints(body, signed) &&
    ints(body, unsigned, 0)
  );
}

const isLevel = (item: Dict) => ints(item, ["price"]) && ints(item, ["qty"], 0);

function isMarket(value: unknown): boolean {
  return (
    isDict(value) &&
    typeof value.symbol === "string" &&
    ints(value, ["price_decimals", "qty_decimals"], 0) &&
    (value.price_decimals as number) <= MAX_DECIMALS &&
    (value.qty_decimals as number) <= MAX_DECIMALS
  );
}

/**
 * Parses a raw WebSocket payload into a feed message, or `null` when it does
 * not have the shape the generated protocol types promise. The types are
 * erased at runtime, so the check is written out by hand.
 */
export function parseFeedMessage(raw: unknown): FeedMessage | null {
  if (typeof raw !== "string") return null;
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (isDict(value) && value.type === "heartbeat") {
    return ints(value, ["seq"], 0) ? (value as FeedMessage) : null;
  }
  if (
    !isDict(value) ||
    !ints(value, ["seq"], 0) ||
    !all(value.bids, isLevel) ||
    !all(value.asks, isLevel) ||
    !all(value.events, isEvent)
  ) {
    return null;
  }
  if (value.type === "update") return value as FeedMessage;
  if (value.type === "snapshot" && isMarket(value.market)) {
    return value as FeedMessage;
  }
  return null;
}
