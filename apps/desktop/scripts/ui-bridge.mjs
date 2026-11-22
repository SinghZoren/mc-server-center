#!/usr/bin/env node
// Development tool: serves the built desktop UI (`dist/`) in an ordinary browser, connected to a
// running local `msmd`. Used to take the README screenshots and to preview UI changes without
// the Tauri shell. Not shipped; uses only Node built-ins.
//
//   pnpm build
//   node scripts/ui-bridge.mjs --data-dir <msmd data dir> [--port 1430]
//   open http://127.0.0.1:1430
//
// It stands in for the Tauri side (src-tauri/src/main.rs + daemon_link.rs): it speaks the
// daemon's framed-JSON protocol over the same local pipe/socket and exposes the same four
// commands the webview uses. Only "This PC" is available (no remote hosts). It listens on
// 127.0.0.1 only, and anything that can reach it can control your servers, so don't expose it.

import { createServer } from "node:http";
import { connect } from "node:net";
import { readFileSync, existsSync, statSync } from "node:fs";
import { join, extname, resolve, dirname } from "node:path";
import { randomBytes } from "node:crypto";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : dflt;
};
const dataDir = opt("--data-dir", null);
const port = Number(opt("--port", "1430"));
if (!dataDir) {
  console.error("usage: node scripts/ui-bridge.mjs --data-dir <dir> [--port 1430]");
  process.exit(2);
}
const dist = resolve(dirname(fileURLToPath(import.meta.url)), "..", "dist");
if (!existsSync(join(dist, "index.html"))) {
  console.error("dist/ not found; run `pnpm build` first");
  process.exit(1);
}

// ---- daemon connection -------------------------------------------------------------------
const endpoint = readFileSync(join(dataDir, "run", "endpoint"), "utf8").trim();
const sock = connect(endpoint);
const pending = new Map();
const sseClients = new Set();
let buf = Buffer.alloc(0);

function send(msg) {
  const body = Buffer.from(JSON.stringify(msg));
  const head = Buffer.alloc(4);
  head.writeUInt32BE(body.length);
  sock.write(Buffer.concat([head, body]));
}

// Request IDs are ULIDs on the daemon side; 26 Crockford base32 characters.
const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
function ulid() {
  let t = Date.now();
  let time = "";
  for (let i = 0; i < 10; i++) {
    time = CROCKFORD[t % 32] + time;
    t = Math.floor(t / 32);
  }
  const r = randomBytes(16);
  let rand = "";
  for (let i = 0; i < 16; i++) rand += CROCKFORD[r[i] % 32];
  return time + rand;
}

function broadcast(event, payload) {
  const line = `data: ${JSON.stringify({ event, payload })}\n\n`;
  for (const res of sseClients) res.write(line);
}

sock.on("data", (chunk) => {
  buf = Buffer.concat([buf, chunk]);
  while (buf.length >= 4) {
    const len = buf.readUInt32BE(0);
    if (buf.length < 4 + len) break;
    const msg = JSON.parse(buf.subarray(4, 4 + len).toString("utf8"));
    buf = buf.subarray(4 + len);
    if (msg.t === "res") {
      const p = pending.get(msg.id);
      pending.delete(msg.id);
      p?.(msg);
    } else if (msg.t === "event") {
      broadcast("msm-event", { host: "local", topic: msg.topic, seq: msg.seq, data: msg.data, gap: null });
    } else if (msg.t === "gap") {
      broadcast("msm-event", { host: "local", topic: msg.topic, seq: msg.to, data: null, gap: [msg.from, msg.to] });
    } else if (msg.t === "bye") {
      console.error("daemon said bye:", msg.error?.detail);
      process.exit(1);
    }
  }
});
sock.on("error", (e) => {
  console.error("daemon connection failed:", e.message);
  process.exit(1);
});
sock.on("connect", () =>
  send({ t: "hello", proto: [1, 1], client: { name: "msm-ui-bridge", version: "0.1.0" } }),
);

function request(cmd, params) {
  const id = ulid();
  return new Promise((resolveP) => {
    pending.set(id, (msg) => resolveP("ok" in msg ? { ok: msg.ok } : { err: msg.err }));
    send({ t: "req", id, cmd, params: params ?? null });
  });
}

// ---- the Tauri stand-in injected into the page ---------------------------------------------
const shim = `
(() => {
  const callbacks = new Map();
  const listeners = new Map(); // eventId -> {event, cb}
  let next = 1;
  const post = (body) => fetch("/__invoke", { method: "POST", body: JSON.stringify(body) }).then((r) => r.json());
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: (_e, id) => listeners.delete(id) };
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
    transformCallback(cb) { const id = next++; callbacks.set(id, cb); return id; },
    unregisterCallback(id) { callbacks.delete(id); },
    convertFileSrc: (p) => p,
    async invoke(cmd, args) {
      if (cmd === "plugin:event|listen") {
        const id = next++;
        listeners.set(id, { event: args.event, cb: callbacks.get(args.handler) });
        if (args.event === "msm-connection") setTimeout(() => callbacks.get(args.handler)?.({ event: args.event, id, payload: { host: "local", state: "connected", detail: null } }), 0);
        return id;
      }
      if (cmd === "plugin:event|unlisten") { listeners.delete(args.eventId); return null; }
      if (cmd === "list_hosts") return [{ id: "local", name: "This PC", address: null }];
      if (cmd === "daemon_request" || cmd === "daemon_subscribe") {
        const r = await post({ cmd, args });
        if (r.err) throw r.err;
        return r.ok;
      }
      return null;
    },
  };
  const es = new EventSource("/__events");
  es.onmessage = (m) => {
    const { event, payload } = JSON.parse(m.data);
    for (const [id, l] of listeners) if (l.event === event) l.cb?.({ event, id, payload });
  };
})();
`;

const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".png": "image/png", ".woff2": "font/woff2", ".woff": "font/woff", ".ico": "image/x-icon" };

createServer(async (req, res) => {
  if (req.url === "/__events") {
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
    sseClients.add(res);
    req.on("close", () => sseClients.delete(res));
    return;
  }
  if (req.url === "/__invoke" && req.method === "POST") {
    let body = "";
    for await (const c of req) body += c;
    const { cmd, args: a } = JSON.parse(body);
    let out = { ok: null };
    if (cmd === "daemon_request") out = await request(a.cmd, a.params);
    else if (cmd === "daemon_subscribe") {
      // A reloaded page needs the history again: re-subscribe from scratch.
      send({ t: "unsub", topic: a.topic });
      send({ t: "sub", topic: a.topic, from_seq: a.fromSeq ?? 0 });
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(out));
    return;
  }
  const path = decodeURIComponent((req.url ?? "/").split("?")[0]);
  let file = resolve(dist, "." + (path === "/" ? "/index.html" : path));
  if (!file.startsWith(dist) || !existsSync(file) || statSync(file).isDirectory()) file = join(dist, "index.html");
  let data = readFileSync(file);
  if (file.endsWith("index.html")) {
    data = Buffer.from(data.toString("utf8").replace("<head>", `<head><script>${shim}</script>`));
  }
  res.writeHead(200, { "content-type": TYPES[extname(file)] ?? "application/octet-stream" });
  res.end(data);
}).listen(port, "127.0.0.1", () => console.log(`UI bridge on http://127.0.0.1:${port} (daemon: ${endpoint})`));
