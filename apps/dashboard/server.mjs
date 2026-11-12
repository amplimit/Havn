/**
 * Custom Next.js server for both `npm run dev` (`--dev`) and
 * `npm start`. It proxies `/api/*` (HTTP) and `/ws/*` (WebSocket) to
 * the gateway at runtime, so `HAVN_GATEWAY_URL` takes effect without
 * a rebuild and dev and production share one code path.
 *
 * This is the cleanest fit for spec §1.7: the gateway stays loopback-
 * only (no `trust_header` + no public bind), and the dashboard plays
 * the role of the "upstream proxy" — auth happens in the dashboard
 * (single-user mode here means bootstrap user is implicit; multi-user
 * deployments would put a real auth proxy between the browser and
 * this dashboard, then forward X-User-ID to gateway via fetch headers).
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
const HOSTNAME = process.env.HOSTNAME ?? "0.0.0.0";
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

const app = next({ dev: DEV, hostname: HOSTNAME, port: PORT });
await app.prepare();
const handle = app.getRequestHandler();

function endToEndHeaders(headers) {
  const out = {};
  for (const [name, value] of Object.entries(headers)) {
    if (value !== undefined && !HOP_BY_HOP.has(name.toLowerCase())) {
      out[name] = value;
    }
  }
  return out;
}

function proxyApi(req, res) {
  const upstream = httpRequest(
    {
      host: GATEWAY_HOST,
      port: GATEWAY_PORT,
      method: req.method,
      path: req.url.slice(API_PREFIX.length - 1),
      headers: { ...endToEndHeaders(req.headers), host: gatewayUrl.host },
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
    // Replay the original HTTP/1.1 Upgrade request bytes so the
    // gateway sees the same Sec-WebSocket-* headers + path. We
    // OVERWRITE the `Origin` header to a canonical loopback value
    // — the gateway's allowlist (spec §3.2 ClawJacked defense) is
    // configured for `http://127.0.0.1:<dashboard-port>` and we
    // don't want to expand that allowlist every time the operator
    // accesses the dashboard via a different hostname (SSH tunnel
    // localhost / public IP / DNS / VPN). The dashboard IS the
    // upstream auth boundary in havn's deploy model (spec §1.7);
    // the gateway trusts whatever Origin the dashboard forwards.
    const canonicalOrigin = `http://127.0.0.1:${PORT}`;
    const headerLines = Object.entries(req.headers)
      .filter(([k, v]) => v !== undefined && k.toLowerCase() !== "origin")
      .map(([k, v]) => `${k}: ${Array.isArray(v) ? v.join(", ") : v}`);
    headerLines.push(`origin: ${canonicalOrigin}`);
    const requestLine = `${req.method} ${req.url} HTTP/${req.httpVersion}`;
    upstream.write(`${requestLine}\r\n${headerLines.join("\r\n")}\r\n\r\n`);
    if (head?.length) upstream.write(head);
    // Bidirectional byte pipe — the WS framing is opaque to us.
    upstream.pipe(clientSocket);
    clientSocket.pipe(upstream);
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

server.listen(PORT, HOSTNAME, () => {
  console.log(`havn dashboard listening on http://${HOSTNAME}:${PORT}`);
  console.log(`  /api and /ws proxy → ${GATEWAY_HOST}:${GATEWAY_PORT}${DEV ? " (dev)" : ""}`);
});

process.on("unhandledRejection", (reason) => {
  console.warn("[unhandledRejection]", reason);
});
