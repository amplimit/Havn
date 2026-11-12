# havn dashboard

Next.js dashboard for the havn gateway (spec §10). Includes:

- **agent list** (`/`) — start / stop / delete with live status pills
- **agent creation** (`/agents/new`) — name + model picker
- **chat view** (`/agents/[id]`) — WebSocket connection to `/ws/chat/:agent_id`
- **credentials** (`/credentials`) — provider keys with optional `max_usd_per_day` cap

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
| `HAVN_GATEWAY_URL` | `http://127.0.0.1:8080` | Gateway address (http only) |

The proxy presents WebSocket upgrades to the gateway with
`Origin: http://127.0.0.1:<PORT>`. The default port is in the gateway's
default `allowed_origins`; if you change `PORT`, add
`http://127.0.0.1:<PORT>` to `allowed_origins` in the gateway config.
