// Holds many feed clients open for a long time and reports what the server does under them:
// message and data rate, dropped clients and, with --pid, the server's memory and CPU.
//
// One extra "stalled" client does the WebSocket handshake over a raw socket and then never
// reads, so the server's slow-consumer handling must close it; the report says when it did. A
// paused socket cannot see the server's close by itself, so a masked ping is written every few
// seconds; the write fails once the server has gone. Clients that close or error are counted
// and never reconnected: a drop is a finding. The stalled client needs a plain ws:// URL.
//
// RSS: the fit covers samples taken after the first 10 minutes only, so start-up growth (book
// load, buffer pools) does not read as a leak. "rssMbPerHour" near zero means steady state; a
// steady positive slope over hours is a leak. CPU % is of one core, from the server's
// cumulative CPU time. The clients run on this machine, so their CPU is not the server's:
// point the URL at another host to keep them apart (--pid then needs a local server).
//
// Usage: node scripts/feed-load.mjs <seconds> <clients> <ws-url> [--pid <server-pid>] [--every <seconds>]
import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import net from "node:net";

const WARMUP_S = 600;
const args = process.argv.slice(2);
const flag = (name, dflt) => {
  const i = args.indexOf(name);
  return i < 0 ? dflt : args.splice(i, 2)[1];
};
const pid = flag("--pid", null);
const every = Number(flag("--every", 30));
const [secArg, clientArg, url] = args;
const secs = Number(secArg);
const nClients = Number(clientArg);
let target;
try { target = new URL(url); } catch { /* reported below */ }
if (!(secs > 0) || !(nClients >= 1) || !(every > 0) || !target || (pid !== null && !(Number(pid) > 0))) {
  console.error("usage: node scripts/feed-load.mjs <seconds> <clients> <ws-url> [--pid <server-pid>] [--every <seconds>]");
  process.exit(1);
}

const t0 = Date.now();
const elapsed = () => +((Date.now() - t0) / 1000).toFixed(1);
let opened = 0, failed = 0, dropped = 0, updates = 0, bytes = 0;
const sockets = [];

const connect = (i) => {
  const ws = new WebSocket(url);
  sockets.push(ws);
  let up = false;
  const end = () => {
    if (up) { up = false; dropped++; } else if (!ws.seen) { failed++; }
    ws.seen = true;
    if (opened + failed === nClients && opened === 0) {
      console.error(`no client could connect to ${url}`);
      process.exit(1);
    }
  };
  ws.onopen = () => { up = true; ws.seen = true; opened++; };
  ws.onmessage = (m) => { updates++; bytes += m.data.length; };
  ws.onerror = end;
  ws.onclose = end;
};
// Spread the connects so the server's accept path is not hit by one burst.
for (let i = 0; i < nClients; i++) setTimeout(connect, i * 20, i);

// Stalled client: handshake by hand, then stop reading.
const stalled = { state: "connecting", closedAtS: null };
if (target.protocol === "ws:") {
  const sock = net.connect(Number(target.port || 80), target.hostname, () => {
    sock.write(`GET ${target.pathname}${target.search} HTTP/1.1\r\nHost: ${target.host}\r\nUpgrade: websocket\r\n` +
      `Connection: Upgrade\r\nSec-WebSocket-Key: ${randomBytes(16).toString("base64")}\r\nSec-WebSocket-Version: 13\r\n\r\n`);
    sock.pause();
    stalled.state = "open";
  });
  const gone = () => {
    if (stalled.state === "open") { stalled.state = "closed_by_server"; stalled.closedAtS = elapsed(); }
    else if (stalled.state === "connecting") stalled.state = "connect_failed";
  };
  sock.on("error", gone);
  sock.on("close", gone);
  // Empty masked ping: the server answers or has already closed, and either way the write tells us.
  setInterval(() => { if (stalled.state === "open") sock.write(Buffer.from([0x89, 0x80, 0, 0, 0, 0])); }, 5000).unref();
} else {
  stalled.state = "skipped_not_ws";
}

// "M:SS.ss", "H:MM:SS" or "D-HH:MM:SS" to seconds.
const cpuSeconds = (t) => {
  const [d, rest] = t.includes("-") ? t.split("-") : ["0", t];
  return rest.split(":").reduce((acc, p) => acc * 60 + Number(p), 0) + Number(d) * 86400;
};
const sampleServer = () => {
  if (pid === null) return null;
  try {
    const [rss, time] = execFileSync("ps", ["-o", "rss=,time=", "-p", pid], { encoding: "utf8" }).trim().split(/\s+/);
    return { t: (Date.now() - t0) / 1000, rssMb: Number(rss) / 1024, cpuS: cpuSeconds(time) };
  } catch { console.error(`cannot read process ${pid}`); return null; }
};

const samples = [];
const first = sampleServer();
if (first) samples.push(first);
let lastUpdates = 0, lastBytes = 0;
const report = () => {
  const s = sampleServer();
  if (s) samples.push(s);
  console.log(JSON.stringify({
    elapsedS: elapsed(), open: opened - dropped, closed: dropped + failed,
    updates: updates - lastUpdates, mb: +((bytes - lastBytes) / 1048576).toFixed(2), stalled: stalled.state,
    ...(s && { rssMb: +s.rssMb.toFixed(1), cpuS: +s.cpuS.toFixed(1) }),
  }));
  lastUpdates = updates; lastBytes = bytes;
};
setInterval(report, every * 1000);

setTimeout(() => {
  report();
  const w = samples.filter((s) => s.t >= WARMUP_S);
  const summary = { summary: true, elapsedS: elapsed(), clients: nClients, opened, dropped, failed, stalled, totalUpdates: updates };
  if (samples.length > 1) {
    const a = samples[0], z = samples[samples.length - 1];
    summary.cpuPctOfOneCore = +((100 * (z.cpuS - a.cpuS)) / (z.t - a.t)).toFixed(1);
  }
  if (w.length >= 2) {
    const n = w.length, mx = w.reduce((p, s) => p + s.t, 0) / n, my = w.reduce((p, s) => p + s.rssMb, 0) / n;
    const slope = w.reduce((p, s) => p + (s.t - mx) * (s.rssMb - my), 0) / w.reduce((p, s) => p + (s.t - mx) ** 2, 0);
    Object.assign(summary, { rssMbMin: +Math.min(...w.map((s) => s.rssMb)).toFixed(1), rssMbMax: +Math.max(...w.map((s) => s.rssMb)).toFixed(1), rssMbPerHour: +(slope * 3600).toFixed(2), rssSamples: n });
  }
  console.log(JSON.stringify(summary));
  process.exit(0);
}, secs * 1000);
