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
# from repo root
cd apps/dashboard
npm install                       # one-time
npm run dev                       # http://localhost:3000

# in another shell, with the gateway already running
HAVN_GATEWAY_URL=http://127.0.0.1:8080 npm run dev
```

The dashboard's dev origin (`http://localhost:3000`) is in the gateway's
default `allowed_origins` allowlist (spec §11 ClawJacked-class mitigation).

REST hits the gateway via Next's `/api/:path*` proxy (see `next.config.mjs`).
WebSocket frames bypass that proxy because Next can't transparently upgrade WS
across origins; the browser opens a direct connection to
`ws://127.0.0.1:8080/ws/chat/:agent_id?token=...` and the gateway validates the
`Origin` header.

## Production

```bash
npm run build
npm start                         # http://localhost:3000
```

Or build a static export and serve via the gateway / a CDN — `next.config.mjs`
takes a single env var (`HAVN_GATEWAY_URL`) for the API hop, plus
`NEXT_PUBLIC_HAVN_GATEWAY_WS` for the WebSocket origin.
