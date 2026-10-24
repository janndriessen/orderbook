import type { CancelRejectReason } from "../protocol/CancelRejectReason";
import type { CancelRequest } from "../protocol/CancelRequest";
import type { RejectReason } from "../protocol/RejectReason";

/** Display wording per reason; also the list of reasons the guard accepts. */
export const REJECT_REASON_LABELS = {
  post_only_would_cross: "post-only would cross",
  no_liquidity: "no liquidity",
  trading_halted: "trading halted",
} satisfies Record<RejectReason, string>;

export const isRejectReason = (value: unknown): value is RejectReason =>
  typeof value === "string" && Object.hasOwn(REJECT_REASON_LABELS, value);

/** Same, for refused cancel and replace requests. */
export const CANCEL_REJECT_REASON_LABELS = {
  unknown_order: "unknown order",
  post_only_would_cross: "post-only would cross",
  trading_halted: "trading halted",
} satisfies Record<CancelRejectReason, string>;

export const isCancelRejectReason = (
  value: unknown,
): value is CancelRejectReason =>
  typeof value === "string" &&
  Object.hasOwn(CANCEL_REJECT_REASON_LABELS, value);

/** Row label per refused request; also the requests the guard accepts. */
export const CANCEL_REQUEST_LABELS = {
  cancel: "Cancel rejected",
  replace: "Replace rejected",
} satisfies Record<CancelRequest, string>;

export const isCancelRequest = (value: unknown): value is CancelRequest =>
  typeof value === "string" && Object.hasOwn(CANCEL_REQUEST_LABELS, value);
