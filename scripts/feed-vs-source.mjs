// Measures how far behind the source's own feed one of our feeds delivers the same push.
//
// The script subscribes to the source's `l2` feed (BTC, no price rounding) and to one of our
// feeds. A source push carries a time `t` in milliseconds; our update events for that push
// carry `ts` = t * 1000 microseconds. For every `t` it records when the push first arrives
// from each side, and prints the distribution of (ours - source) in milliseconds.
//
// Both arrival times come from this process's monotonic clock, so no clock offset between
// the source, the service and this machine enters the result. A negative value means one of
// our seven source connections received the push before the single connection this script
// compares against. The result is the service's own delay plus the network between the
// service and this machine; the source's delay before its data reaches anyone is not in it.
//
// Usage: node scripts/feed-vs-source.mjs <seconds> <our-ws-url>
import zlib from "node:zlib";
const secs = Number(process.argv[2]);
const url = process.argv[3];
if (!(secs > 0) || !url) {
  console.error("usage: node scripts/feed-vs-source.mjs <seconds> <our-ws-url>");
  process.exit(2);
}
const fail = (what) => { console.error(`${what}`); process.exit(1); };
const first = (map, t, now) => { if (!map.has(t)) map.set(t, now); };
const source = new Map();
const ours = new Map();

const src = new WebSocket("wss://api.hyperliquid.xyz/ws");
src.onerror = () => fail("source: cannot connect");
src.onclose = () => fail("source: connection closed");
src.onopen = () => {
  src.send(JSON.stringify({ method: "subscribe", subscription: { type: "l2", c: "BTC" } }));
  setInterval(() => src.send(JSON.stringify({ method: "ping" })), 30_000);
};
src.onmessage = (m) => {
  const now = performance.now();
  const d = JSON.parse(m.data);
  if (d.channel !== "l2") return;
  // A push is a snapshot `s`, a deflated change `c` (base64, raw deflate) or a plain change `u`.
  const x = d.data;
  const body = x.c ? JSON.parse(zlib.inflateRawSync(Buffer.from(x.c, "base64"))) : (x.u ?? x.s);
  const t = body?.t ?? body?.time;
  if (t != null) first(source, t, now);
};

const feed = new WebSocket(url);
feed.onerror = () => fail(`ours: cannot connect to ${url}`);
feed.onclose = () => fail("ours: connection closed");
feed.onmessage = (m) => {
  const now = performance.now();
  const d = JSON.parse(m.data);
  if (d.type !== "update") return;
  for (const e of d.events ?? []) first(ours, Math.round(e.ts / 1000), now);
};

setTimeout(() => {
  const lags = [...source].filter(([t]) => ours.has(t)).map(([t, a]) => ours.get(t) - a).sort((a, b) => a - b);
  const q = (p) => lags[Math.floor(p * (lags.length - 1))]?.toFixed(0);
  console.log(JSON.stringify({ sourcePushes: source.size, matched: lags.length,
    behindSourceMs: { p10: q(0.1), p50: q(0.5), p90: q(0.9), min: lags[0]?.toFixed(0), max: lags.at(-1)?.toFixed(0) } }));
  process.exit(0);
}, secs * 1000);
