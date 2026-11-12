/**
 * Custom Next.js server for both `npm run dev` (`--dev`) and
 * `npm start`. It proxies `/api/*` (HTTP) and `/ws/*` (WebSocket) to
 * the gateway at runtime, so `HAVN_GATEWAY_URL` takes effect without
 * a rebuild and dev and production share one code path.
 *
 * The dashboard has no login of its own, so it listens on 127.0.0.1 by
 * default. In multi-user mode an authenticating proxy in front of it
 * sets X-User-ID; that header is forwarded to the gateway only from the
 * peers listed in HAVN_DASHBOARD_TRUSTED_PROXIES and dropped otherwise.
 *
 * WS path: browser opens `ws://<dashboard-host>:3000/ws/chat/<id>?token=…`,
 * we open a raw TCP socket to the gateway (default `127.0.0.1:8080`),
 * replay the original Upgrade request bytes, then pipe the two sockets
 * together. No npm dependency — pure Node `http` + `net`.
 */

import { createServer, request as httpRequest } from "node:http";
import { connect as netConnect } from "node:net";
import { pipeline } from "node:stream";
import next from "next";

const DEV = process.argv.includes("--dev");
const PORT = Number(process.env.PORT ?? 3000);
// Not HOSTNAME: shells and containers often export it as the machine
// name, which would bind the dashboard to a network interface.
// `||`, not `??`: an empty value (e.g. `Environment=HAVN_DASHBOARD_HOST=`)
// would otherwise make Node listen on every interface.
const HOST = process.env.HAVN_DASHBOARD_HOST || "127.0.0.1";
const TRUSTED_PROXIES = (process.env.HAVN_DASHBOARD_TRUSTED_PROXIES ?? "")
  .split(",")
  .map((entry) => entry.trim())
  .filter(Boolean);
const IDENTITY_HEADER = "x-user-id";
const MAX_UPGRADE_RESPONSE_HEAD = 16 * 1024;
const GATEWAY = process.env.HAVN_GATEWAY_URL ?? "http://127.0.0.1:8080";
const WS_PREFIX = "/ws/";
const API_PREFIX = "/api/";
const HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "proxy-connection",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
]);

function parseGatewayUrl(raw) {
  let url;
  try {
    url = new URL(raw);
  } catch {
    url = null;
  }
  const plain =
    url !== null &&
    url.protocol === "http:" &&
    url.pathname === "/" &&
    !url.search &&
    !url.hash &&
    !url.username &&
    !url.password;
  if (!plain) {
    console.error(`HAVN_GATEWAY_URL must be http://host:port with nothing else (got ${raw})`);
    process.exit(1);
  }
  return url;
}

const gatewayUrl = parseGatewayUrl(GATEWAY);
const GATEWAY_HOST = gatewayUrl.hostname.replace(/^\[|\]$/g, "");
const GATEWAY_PORT = Number(gatewayUrl.port || 80);

const app = next({ dev: DEV, hostname: HOST, port: PORT });
await app.prepare();
const handle = app.getRequestHandler();

function peerAddress(req) {
  const addr = req.socket.remoteAddress ?? "";
  return addr.startsWith("::ffff:") ? addr.slice("::ffff:".length) : addr;
}

// Only an authenticating proxy listed in HAVN_DASHBOARD_TRUSTED_PROXIES
// may tell the gateway who the user is.
function mayAssertIdentity(req) {
  return TRUSTED_PROXIES.includes(peerAddress(req));
}

function endToEndHeaders(headers) {
  const out = {};
  for (const [name, value] of Object.entries(headers)) {
    if (value !== undefined && !HOP_BY_HOP.has(name.toLowerCase())) {
      out[name] = value;
    }
  }
  return out;
}

function forwardedHeaders(req) {
  const headers = { ...endToEndHeaders(req.headers), host: gatewayUrl.host };
  if (!mayAssertIdentity(req)) delete headers[IDENTITY_HEADER];
  // A chunked request body must stay chunked upstream. Without framing,
  // Node writes the decoded body of a GET/DELETE/OPTIONS as raw bytes on
  // a keep-alive connection, and the gateway parses them as a second
  // request (which would carry whatever X-User-ID the client chose).
  if (req.headers["transfer-encoding"] !== undefined) {
    headers["transfer-encoding"] = "chunked";
  }
  return headers;
}

function proxyApi(req, res) {
  const upstream = httpRequest(
    {
      host: GATEWAY_HOST,
      port: GATEWAY_PORT,
      method: req.method,
      path: req.url.slice(API_PREFIX.length - 1),
      headers: forwardedHeaders(req),
    },
    (upstreamRes) => {
      res.writeHead(upstreamRes.statusCode ?? 502, endToEndHeaders(upstreamRes.headers));
      // A response cut off by the gateway must not leave the browser
      // waiting on a half-sent body.
      pipeline(upstreamRes, res, (err) => {
        if (err) res.destroy();
      });
    },
  );
  upstream.on("error", (err) => {
    // The client went away and we cancelled the request ourselves.
    if (res.destroyed) return;
    console.warn(`[gateway proxy] ${err.code ?? err.message}: ${req.method} ${req.url}`);
    if (res.headersSent) {
      res.destroy();
      return;
    }
    res.writeHead(502, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: "gateway unreachable" }));
  });
  res.on("close", () => {
    if (!res.writableFinished) upstream.destroy();
  });
  req.on("error", () => upstream.destroy());
  req.pipe(upstream);
}

const server = createServer((req, res) => {
  if (req.url?.startsWith(API_PREFIX)) {
    proxyApi(req, res);
    return;
  }
  handle(req, res);
});

server.on("upgrade", (req, clientSocket, head) => {
  if (!req.url?.startsWith(WS_PREFIX)) {
    // In dev, Next registers its own upgrade listener on this server for
    // its hot-reload socket; leave those to it. Nothing else is served
    // over WebSocket.
    // If the dev server restarts while a tab is open, reload the tab: Next
    // only attaches that listener after its first page request.
    if (DEV && req.url?.startsWith("/_next/")) return;
    clientSocket.destroy();
    return;
  }
  const upstream = netConnect({ host: GATEWAY_HOST, port: GATEWAY_PORT }, () => {
    // Replay the Upgrade request with Origin set to the dashboard's own
    // loopback origin, which the gateway's allowed_origins lists by
    // default; browsers reach the dashboard under many hostnames (SSH
    // tunnel, proxy domain), so their Origin cannot be listed upfront.
    const canonicalOrigin = `http://127.0.0.1:${PORT}`;
    const trusted = mayAssertIdentity(req);
    const headerLines = Object.entries(req.headers)
      .filter(([k, v]) => v !== undefined && k.toLowerCase() !== "origin")
      .filter(([k]) => trusted || k.toLowerCase() !== IDENTITY_HEADER)
      .map(([k, v]) => `${k}: ${Array.isArray(v) ? v.join(", ") : v}`);
    headerLines.push(`origin: ${canonicalOrigin}`);
    const requestLine = `${req.method} ${req.url} HTTP/${req.httpVersion}`;
    upstream.write(`${requestLine}\r\n${headerLines.join("\r\n")}\r\n\r\n`);

    // Forward nothing else until the gateway has switched protocols.
    // Bytes the client sent after its Upgrade request (`head`, or later)
    // would otherwise reach the gateway as further HTTP requests on this
    // loopback connection if it refused the upgrade.
    let response = Buffer.alloc(0);
    const onResponse = (chunk) => {
      response = Buffer.concat([response, chunk]);
      if (response.indexOf("\r\n\r\n") === -1) {
        if (response.length > MAX_UPGRADE_RESPONSE_HEAD) tearDown();
        return;
      }
      upstream.off("data", onResponse);
      clientSocket.write(response);
      const statusLine = response.subarray(0, response.indexOf("\r\n")).toString("latin1");
      if (!/^HTTP\/1\.1 101 /.test(statusLine)) {
        tearDown();
        return;
      }
      if (head?.length) upstream.write(head);
      // The WebSocket framing is opaque to us from here on.
      upstream.pipe(clientSocket);
      clientSocket.pipe(upstream);
    };
    upstream.on("data", onResponse);
  });
  const tearDown = () => {
    upstream.destroy();
    clientSocket.destroy();
  };
  upstream.on("error", tearDown);
  upstream.on("close", () => clientSocket.destroy());
  clientSocket.on("error", tearDown);
  clientSocket.on("close", () => upstream.destroy());
});

server.listen(PORT, HOST, () => {
  console.log(`havn dashboard listening on http://${HOST}:${PORT}`);
  console.log(
    `  X-User-ID accepted from: ${TRUSTED_PROXIES.length ? TRUSTED_PROXIES.join(", ") : "nobody"}`,
  );
  console.log(`  /api and /ws proxy → ${GATEWAY_HOST}:${GATEWAY_PORT}${DEV ? " (dev)" : ""}`);
});

process.on("unhandledRejection", (reason) => {
  console.warn("[unhandledRejection]", reason);
});
