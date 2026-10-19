import type { RejectReason } from "../protocol/RejectReason";

/** Display wording per reason; also the list of reasons the guard accepts. */
export const REJECT_REASON_LABELS = {
  post_only_would_cross: "post-only would cross",
  no_liquidity: "no liquidity",
} satisfies Record<RejectReason, string>;

export const isRejectReason = (value: unknown): value is RejectReason =>
  typeof value === "string" && Object.hasOwn(REJECT_REASON_LABELS, value);
