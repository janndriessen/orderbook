// Measures what a client of one or more feed servers sees: update rate, data rate and the lag
// of each update, in milliseconds, as one JSON line per server.
//
// Lag = wall clock at receipt minus the newest event `ts` in the update (microseconds since
// the epoch); updates without events are skipped for lag. Limits: the lag includes the
// source's own delay and any clock offset between the source and this machine, so compare
// builds side by side (same source, same time) instead of reading the absolute number as ours.
// A replay at any speed other than the recorded one makes the lag meaningless.
//
// Usage: node scripts/feed-latency.mjs <seconds> <name>=<ws-url> [<name>=<ws-url> ...]
const [secArg, ...targets] = process.argv.slice(2);
const secs = Number(secArg);
const servers = targets.map((t) => t.split(/=(.*)/s));
if (!(secs > 0) || servers.length === 0 || servers.some(([n, u]) => !n || !u)) {
  console.error("usage: node scripts/feed-latency.mjs <seconds> <name>=<ws-url> [<name>=<ws-url> ...]");
  process.exit(2);
}
const stats = {};
for (const [name, url] of servers) {
  const s = (stats[name] = { lags: [], updates: 0, bytes: 0 });
  const ws = new WebSocket(url);
  ws.onerror = () => { console.error(`${name}: cannot connect to ${url}`); process.exit(1); };
  ws.onclose = () => { console.error(`${name}: connection closed`); process.exit(1); };
  ws.onmessage = (m) => {
    const now = Date.now() * 1000;
    const d = JSON.parse(m.data);
    if (d.type !== "update") return;
    s.updates++;
    s.bytes += m.data.length;
    const ts = Math.max(0, ...(d.events ?? []).map((e) => e.ts ?? 0));
    if (ts > 0) s.lags.push((now - ts) / 1000);
  };
}
setTimeout(() => {
  for (const [name, s] of Object.entries(stats)) {
    const l = s.lags.sort((a, b) => a - b);
    const q = (p) => l[Math.floor(p * (l.length - 1))]?.toFixed(0);
    const lagMs = { p10: q(0.1), p50: q(0.5), p90: q(0.9) };
    console.log(JSON.stringify({ name, updates: s.updates, perSec: +(s.updates / secs).toFixed(2), kbPerSec: +(s.bytes / 1024 / secs).toFixed(1), lagMs }));
  }
  process.exit(0);
}, secs * 1000);
