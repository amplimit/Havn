/**
 * Custom Next.js server that ALSO proxies WebSocket upgrades to the
 * gateway. The default `next.config.mjs::rewrites` only proxies HTTP;
 * WS connections to /ws/* would otherwise die at the Next layer.
 *
 * This is the cleanest fit for spec §1.7: the gateway stays loopback-
 * only (no `trust_header` + no public bind), and the dashboard plays
 * the role of the "upstream proxy" — auth happens in the dashboard
 * (single-user mode here means bootstrap user is implicit; multi-user
 * deployments would put a real auth proxy between the browser and
 * this dashboard, then forward X-User-ID to gateway via fetch headers).
 *
 * WS path: browser opens `ws://<public-host>:8101/ws/chat/<id>?token=…`,
 * we open a raw TCP socket to `127.0.0.1:8100`, replay the original
 * Upgrade request bytes, then pipe the two sockets together. No npm
 * dependency — pure Node `http` + `net`.
 */

import { createServer } from "node:http";
import { connect as netConnect } from "node:net";
import next from "next";

const PORT = Number(process.env.PORT ?? 8101);
const HOSTNAME = process.env.HOSTNAME ?? "0.0.0.0";
const GATEWAY = process.env.HAVN_GATEWAY_URL ?? "http://127.0.0.1:8100";
const WS_PREFIX = "/ws/";

const gatewayUrl = new URL(GATEWAY);
const GATEWAY_HOST = gatewayUrl.hostname;
const GATEWAY_PORT = Number(gatewayUrl.port || (gatewayUrl.protocol === "https:" ? 443 : 80));

const app = next({ dev: false, hostname: HOSTNAME, port: PORT });
await app.prepare();
const handle = app.getRequestHandler();

const server = createServer((req, res) => handle(req, res));

server.on("upgrade", (req, clientSocket, head) => {
  // Only proxy /ws/* — anything else is a misuse, refuse cleanly.
  if (!req.url?.startsWith(WS_PREFIX)) {
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
  console.log(`  WS upgrades proxy → ${GATEWAY_HOST}:${GATEWAY_PORT}`);
});

// Next.js's internal HTTP proxy bubbles up ECONNREFUSED as an
// uncaughtException when the gateway is briefly down (e.g. during a
// restart). Without this guard the whole dashboard process exits and
// any open browser tabs go dead. We log + swallow these specific
// proxy errors and let the in-flight request return its 502; any
// genuinely-unexpected error still crashes (we don't want to swallow
// real bugs).
process.on("uncaughtException", (err) => {
  if (
    err && typeof err === "object" &&
    ("code" in err && (err.code === "ECONNREFUSED" || err.code === "ECONNRESET" ||
                       err.code === "ETIMEDOUT" || err.code === "EPIPE"))
  ) {
    console.warn(`[gateway proxy] ${err.code}: gateway unreachable — request returns 5xx`);
    return;
  }
  console.error("uncaughtException — exiting:", err);
  process.exit(1);
});
process.on("unhandledRejection", (reason) => {
  console.warn("[unhandledRejection]", reason);
});
