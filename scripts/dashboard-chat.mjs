// scripts/dashboard-chat.mjs - open a chat through the dashboard's own
// server in a headless browser and wait for the agent's reply.
//
// For the length of the run it also serves a mock of the Anthropic
// Messages API, which the gateway reaches through `[llm.base_url]`
// (scripts/readme-deploy.sh sets that up). Checks that the reply shows up
// in the page, that it came over the dashboard's `/ws` proxy, that `/api`
// answers through the dashboard, and that the gateway called the mock with
// the credential that was added.
//
// Env: PLAYWRIGHT_DIR (a directory with node_modules/playwright),
// HAVN_DASHBOARD_URL, HAVN_MOCK_LLM_PORT, HAVN_MOCK_LLM_KEY.

import { createServer } from "node:http";
import { createRequire } from "node:module";

const { chromium } = createRequire(`${process.env.PLAYWRIGHT_DIR}/`)("playwright");

const DASHBOARD = process.env.HAVN_DASHBOARD_URL ?? "http://127.0.0.1:3000";
const MOCK_PORT = Number(process.env.HAVN_MOCK_LLM_PORT ?? 18090);
const MOCK_KEY = process.env.HAVN_MOCK_LLM_KEY ?? "";
const REPLY = "Hello from the mock provider.";
const TIMEOUT_MS = 120_000;

const calls = [];
const mock = createServer((req, res) => {
  let body = "";
  req.setEncoding("utf8");
  req.on("data", (chunk) => {
    body += chunk;
  });
  req.on("end", () => {
    if (req.method !== "POST" || req.url !== "/v1/messages") {
      res.writeHead(404).end();
      return;
    }
    let request;
    try {
      request = JSON.parse(body);
    } catch {
      res.writeHead(400).end();
      return;
    }
    calls.push({ key: req.headers["x-api-key"], model: request.model });
    res.writeHead(200, { "content-type": "application/json" });
    res.end(
      JSON.stringify({
        id: `msg_mock_${calls.length}`,
        type: "message",
        role: "assistant",
        model: request.model,
        content: [{ type: "text", text: REPLY }],
        stop_reason: "end_turn",
        stop_sequence: null,
        usage: { input_tokens: 12, output_tokens: 7 },
      }),
    );
  });
});
await new Promise((resolve) => mock.listen(MOCK_PORT, "127.0.0.1", resolve));

function check(condition, message) {
  if (!condition) throw new Error(message);
}

const browser = await chromium.launch();
let failure = null;
try {
  const page = await browser.newPage();
  page.setDefaultTimeout(TIMEOUT_MS);
  const sockets = [];
  page.on("websocket", (ws) => {
    const entry = { url: ws.url(), received: [] };
    sockets.push(entry);
    ws.on("framereceived", (frame) => entry.received.push(String(frame.payload)));
  });
  page.on("console", (msg) => {
    if (msg.type() === "error") console.error(`[browser] ${msg.text()}`);
  });

  await page.goto(`${DASHBOARD}/agents/new`);
  await page.fill("#name", "readme-agent");
  const created = page.waitForResponse(
    (r) => new URL(r.url()).pathname === "/api/agents" && r.request().method() === "POST",
  );
  await page.getByRole("button", { name: "create agent" }).click();
  const createdStatus = (await created).status();
  check(createdStatus === 200 || createdStatus === 201, `POST /api/agents returned ${createdStatus}`);
  await page.waitForURL(/\/agents\/[0-9a-f-]{36}$/);

  const composer = page.locator("textarea:not([disabled])");
  await composer.waitFor();
  await composer.fill("Hello, are you there?");
  await page.getByRole("button", { name: "send", exact: true }).click();
  await page.getByText(REPLY).waitFor();

  const chat = sockets.find((s) => new URL(s.url).pathname.startsWith("/ws/chat/"));
  check(chat, `no chat WebSocket opened; saw ${JSON.stringify(sockets.map((s) => s.url))}`);
  check(
    new URL(chat.url).origin === new URL(DASHBOARD).origin.replace(/^http/, "ws"),
    `the chat WebSocket bypassed the dashboard: ${chat.url}`,
  );
  check(
    chat.received.some((payload) => payload.includes(REPLY)),
    "the reply did not arrive over the dashboard's /ws proxy",
  );

  const me = await page.evaluate(async () => {
    const res = await fetch("/api/me");
    return { status: res.status, body: await res.json().catch(() => null) };
  });
  check(me.status === 200 && typeof me.body?.ws_token === "string", `GET /api/me: ${JSON.stringify(me)}`);

  check(calls.length > 0, "the gateway never called the mock provider");
  check(
    calls.every((c) => c.key === MOCK_KEY),
    "the gateway called the mock without the credential that was added",
  );
  console.log(`chat reply received; mock provider served ${calls.length} call(s) for ${calls[0].model}`);
} catch (e) {
  failure = e;
  const dir = process.env.RUNNER_TEMP ?? "/tmp";
  for (const page of browser.contexts().flatMap((c) => c.pages())) {
    await page.screenshot({ path: `${dir}/dashboard-chat.png`, fullPage: true }).catch(() => {});
  }
} finally {
  await browser.close();
  mock.close();
}
if (failure) {
  console.error(failure);
  process.exit(1);
}
