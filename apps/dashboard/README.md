# havn dashboard

Next.js dashboard for the havn gateway (spec §10). Includes:

- **agent list** (`/`) — start / stop / delete with live status pills
- **agent creation** (`/agents/new`) — name + model picker
- **chat view** (`/agents/[id]`) — WebSocket connection to `/ws/chat/:agent_id`
- **credentials** (`/credentials`) — provider keys with optional `max_tokens_per_day` cap

## Design

Visual system follows `../../DESIGN.md` (Stripe-inspired). Because the
proprietary `sohne-var` font is unavailable, we substitute Inter Variable with
Inter's `ss01` stylistic set toggled on — the closest open match for
"geometric, modern letterforms at light weights." All other tokens (colors,
shadows, radii, spacing) are reproduced verbatim.

## Dev

```bash
cd apps/dashboard
npm install                       # one-time
npm run dev                       # http://localhost:3000
```

## Production

```bash
npm run build
npm start                         # http://localhost:3000
```

Both commands run `server.mjs`, which serves the UI and proxies `/api/*`
(REST) and `/ws/*` (WebChat WebSocket) to the gateway, so the browser only
talks to the dashboard's origin. Settings, read at startup:

| Variable | Default | Meaning |
|---|---|---|
| `PORT` | `3000` | Dashboard port |
| `HAVN_DASHBOARD_HOST` | `127.0.0.1` | Listen address |
| `HAVN_GATEWAY_URL` | `http://127.0.0.1:8080` | Gateway address (http only) |
| `HAVN_DASHBOARD_TRUSTED_PROXIES` | empty | Comma-separated peer addresses allowed to set the identity header |
| `HAVN_DASHBOARD_IDENTITY_HEADER` | unset | Optional check: refuse to start unless the gateway's `trust_header.header` has this name |

The proxy presents WebSocket upgrades to the gateway with
`Origin: http://127.0.0.1:<PORT>`. The default port is in the gateway's
default `allowed_origins`; if you change `PORT`, add
`http://127.0.0.1:<PORT>` to `allowed_origins` in the gateway config.

## Access from another machine

The dashboard has no login of its own: anyone who can reach it acts as
the single-user operator, or as whatever identity header they are allowed
to send. Keep it on `127.0.0.1` and reach it through one of:

- **An SSH tunnel** (single user): `ssh -L 3000:127.0.0.1:3000 <server>`,
  then open `http://localhost:3000`.
- **An authenticating reverse proxy** on the same host (multi-user). The
  proxy logs the user in and sets the identity header (the gateway's
  `trust_header.header`, for example `X-Forwarded-User`) to the user's
  external id (`havn user add --external-id`) or havn user id on every
  request it forwards, replacing any value the client sent, including on
  routes it does not authenticate. Set `HAVN_DASHBOARD_TRUSTED_PROXIES`
  to the exact address the proxy connects from (for example `127.0.0.1`;
  no ranges or host names). The dashboard learns the header name from the
  gateway (`GET /auth/info`) at startup and every 10 seconds, forwards it
  only from those addresses, and drops other common identity headers
  (`X-User-ID`, `Remote-User`, `X-Forwarded-Email`, ...) from every peer.
  While the gateway has not reported the name, `/api` and `/ws` get 503.
  See the [main README](../../README.md#multi-user-mode-behind-an-sso-proxy)
  for proxy recipes.
  Trusting `127.0.0.1` trusts every process on the host, including other
  OS users and agents run with `HAVN_SPAWNER=subprocess`.

Setting `HAVN_DASHBOARD_HOST=0.0.0.0` exposes the dashboard without
authentication to every network the host is on, including the per-agent
interfaces that sandboxed agents with network access can reach. Do that
only behind a firewall that admits nothing but the proxy.
