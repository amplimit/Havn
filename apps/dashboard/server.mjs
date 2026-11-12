/**
 * Custom Next.js server for both `npm run dev` (`--dev`) and
 * `npm start`. It proxies `/api/*` (HTTP) and `/ws/*` (WebSocket) to
 * the gateway at runtime, so `HAVN_GATEWAY_URL` takes effect without
 * a rebuild and dev and production share one code path.
 *
 * The dashboard has no login of its own, so it listens on 127.0.0.1 by
 * default. In multi-user mode an authenticating proxy in front of it
 * sets the identity header. Its name comes from the gateway
 * (`GET /auth/info`, the gateway's `trust_header.header`), so the two can
 * never disagree; HAVN_DASHBOARD_IDENTITY_HEADER, when set, must match it.
 * That header is forwarded only from the peers listed in
 * HAVN_DASHBOARD_TRUSTED_PROXIES; other common identity headers are
 * dropped from every peer, as is every name the gateway reported before.
 * Each forwarded request carries `x-havn-identity-header` naming the
 * header it was filtered for; the gateway refuses it when that is not
 * its own (after a rename, until the next refresh). Until the gateway has
 * reported the name, /api and /ws requests are refused.
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
const HEADER_NAME = /^[!#$%&'*+.^_`|~0-9a-z-]+$/;
const EXPECTED_IDENTITY_HEADER =
  process.env.HAVN_DASHBOARD_IDENTITY_HEADER?.trim().toLowerCase() || null;
if (EXPECTED_IDENTITY_HEADER !== null && !HEADER_NAME.test(EXPECTED_IDENTITY_HEADER)) {
  console.error(
    `HAVN_DASHBOARD_IDENTITY_HEADER is not a valid header name (got ${EXPECTED_IDENTITY_HEADER})`,
  );
  process.exit(1);
}
const IDENTITY_REFRESH_MS = 10_000;
const FILTERED_FOR_HEADER = "x-havn-identity-header";
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

// Headers common SSO proxies use for the user's identity. Only the one
// the gateway reads is ever forwarded, so a trusted proxy that passes a
// client's other identity header through cannot reach the gateway with it.
const KNOWN_IDENTITY_HEADERS = new Set([
  "x-user-id",
  "remote-user",
  "remote-email",
  "x-remote-user",
  "x-forwarded-user",
  "x-forwarded-email",
  "x-forwarded-preferred-username",
  "x-auth-request-user",
  "x-auth-request-email",
  "cf-access-authenticated-user-email",
]);
if (EXPECTED_IDENTITY_HEADER !== null) KNOWN_IDENTITY_HEADERS.add(EXPECTED_IDENTITY_HEADER);
// Every name the gateway ever reported joins the set, so a rename never
// lets the previous name through from untrusted clients.

// The header the gateway reads, or null while it has not told us.
let identityHeader = null;
let learning = null;
let lastIdentityError = null;

async function fetchIdentityHeader() {
  const res = await fetch(new URL("/auth/info", gatewayUrl), {
    signal: AbortSignal.timeout(5_000),
  });
  // Gateways up to v0.1.2 have no /auth/info and only ever read X-User-ID.
  // With a different expected header a 404 is an error, not that gateway.
  if (res.status === 404 && (EXPECTED_IDENTITY_HEADER ?? "x-user-id") === "x-user-id") {
    await res.body?.cancel();
    return "x-user-id";
  }
  if (!res.ok) throw new Error(`GET /auth/info returned ${res.status}`);
  const body = await res.json();
  const name = typeof body?.identity_header === "string" ? body.identity_header.toLowerCase() : "";
  if (!HEADER_NAME.test(name)) throw new Error("GET /auth/info has no identity_header");
  return name;
}

function learnIdentityHeader() {
  learning ??= fetchIdentityHeader()
    .then((name) => {
      if (EXPECTED_IDENTITY_HEADER !== null && name !== EXPECTED_IDENTITY_HEADER) {
        console.error(
          `HAVN_DASHBOARD_IDENTITY_HEADER is ${EXPECTED_IDENTITY_HEADER} but the gateway reads ` +
            `${name} (trust_header.header); make them equal or unset HAVN_DASHBOARD_IDENTITY_HEADER`,
        );
        process.exit(1);
      }
      if (name !== identityHeader) console.log(`  identity header (from the gateway): ${name}`);
      KNOWN_IDENTITY_HEADERS.add(name);
      identityHeader = name;
      lastIdentityError = null;
    })
    .catch((err) => {
      identityHeader = null;
      if (err.message !== lastIdentityError) {
        console.warn(
          `[identity] cannot learn the gateway's identity header (${err.message}); ` +
            "refusing /api and /ws until it answers",
        );
        lastIdentityError = err.message;
      }
    })
    .finally(() => {
      learning = null;
    });
  return learning;
}

async function identityKnown() {
  if (identityHeader === null) await learnIdentityHeader();
  return identityHeader !== null;
}

// The gateway's identity header passes only from a trusted peer; every
// other known identity header is dropped from every peer.
function dropsIdentity(name, trusted) {
  const lower = name.toLowerCase();
  if (lower === FILTERED_FOR_HEADER) return true;
  if (lower === identityHeader) return !trusted;
  return KNOWN_IDENTITY_HEADERS.has(lower);
}

// Node joins a repeated header into one value; refuse rather than forward
// a client's value glued to the proxy's.
function repeatsIdentity(req) {
  return (req.headersDistinct[identityHeader]?.length ?? 0) > 1;
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
  const trusted = mayAssertIdentity(req);
  for (const name of Object.keys(headers)) {
    if (dropsIdentity(name, trusted)) delete headers[name];
  }
  // "none" if an upstream error cleared the name meanwhile; the gateway
  // then refuses the request.
  headers[FILTERED_FOR_HEADER] = identityHeader ?? "none";
  // A chunked request body must stay chunked upstream. Without framing,
  // Node writes the decoded body of a GET/DELETE/OPTIONS as raw bytes on
  // a keep-alive connection, and the gateway parses them as a second
  // request (which would carry whatever identity the client chose).
  if (req.headers["transfer-encoding"] !== undefined) {
    headers["transfer-encoding"] = "chunked";
  }
  return headers;
}

function proxyApi(req, res) {
  const te = req.headers["transfer-encoding"];
  if (te !== undefined && te.trim().toLowerCase() !== "chunked") {
    res.writeHead(400, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: "unsupported transfer-encoding" }));
    return;
  }
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
    // The gateway may come back with another identity header.
    identityHeader = null;
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

function refuseApi(res, status, error) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify({ error }));
}

function refuseUpgrade(socket, status, reason) {
  socket.end(`HTTP/1.1 ${status} ${reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n`);
}

const server = createServer((req, res) => {
  if (!req.url?.startsWith(API_PREFIX)) {
    handle(req, res);
    return;
  }
  req.on("error", () => {});
  identityKnown().then((known) => {
    if (!known) refuseApi(res, 503, "gateway identity header unknown");
    else if (repeatsIdentity(req)) refuseApi(res, 400, "identity header sent more than once");
    else proxyApi(req, res);
  });
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
  clientSocket.on("error", () => clientSocket.destroy());
  identityKnown().then((known) => {
    if (clientSocket.destroyed) return;
    if (!known) refuseUpgrade(clientSocket, 503, "Service Unavailable");
    else if (repeatsIdentity(req)) refuseUpgrade(clientSocket, 400, "Bad Request");
    else proxyUpgrade(req, clientSocket, head);
  });
});

function proxyUpgrade(req, clientSocket, head) {
  const upstream = netConnect({ host: GATEWAY_HOST, port: GATEWAY_PORT }, () => {
    // Replay the Upgrade request with Origin set to the dashboard's own
    // loopback origin, which the gateway's allowed_origins lists by
    // default; browsers reach the dashboard under many hostnames (SSH
    // tunnel, proxy domain), so their Origin cannot be listed upfront.
    const canonicalOrigin = `http://127.0.0.1:${PORT}`;
    const trusted = mayAssertIdentity(req);
    const headerLines = Object.entries(req.headers)
      .filter(([k, v]) => v !== undefined && k.toLowerCase() !== "origin")
      .filter(([k]) => !dropsIdentity(k, trusted))
      .map(([k, v]) => `${k}: ${Array.isArray(v) ? v.join(", ") : v}`);
    headerLines.push(`origin: ${canonicalOrigin}`);
    headerLines.push(`${FILTERED_FOR_HEADER}: ${identityHeader ?? "none"}`);
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
      const statusLine = response.subarray(0, response.indexOf("\r\n")).toString("latin1");
      if (!/^HTTP\/1\.1 101 /.test(statusLine)) {
        // Relay the refusal and close; never forward further requests.
        clientSocket.end(response);
        upstream.destroy();
        return;
      }
      clientSocket.write(response);
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
  upstream.on("error", () => {
    identityHeader = null;
    tearDown();
  });
  upstream.on("close", () => clientSocket.destroy());
  clientSocket.on("error", tearDown);
  clientSocket.on("close", () => upstream.destroy());
}

await learnIdentityHeader();
// The gateway only changes the name on restart; re-check in case it did.
setInterval(learnIdentityHeader, IDENTITY_REFRESH_MS).unref();

server.listen(PORT, HOST, () => {
  console.log(`havn dashboard listening on http://${HOST}:${PORT}`);
  console.log(
    `  identity header accepted from: ${TRUSTED_PROXIES.length ? TRUSTED_PROXIES.join(", ") : "nobody"}`,
  );
  console.log(`  /api and /ws proxy → ${GATEWAY_HOST}:${GATEWAY_PORT}${DEV ? " (dev)" : ""}`);
});

process.on("unhandledRejection", (reason) => {
  console.warn("[unhandledRejection]", reason);
});
