# Havn

Open infrastructure for running autonomous AI agents. Single binary, single SQLite file, Linux-native process isolation.

havn is a self-hosted server that runs a gateway and per-user agents. Identity, network exposure, encryption keys, and external chat-platform integrations come from the operator's own stack. havn focuses on what nobody else does well: kernel-level agent isolation, typed memory, skill curation, credential proxying, and a usable single-binary deployment.

## Key Features

- **Linux-native process isolation** — namespace + Landlock LSM + seccomp-bpf + cgroup v2 per agent. No Docker daemon, no per-user VMs, near-zero overhead.
- **Single gateway, many agents** — one process handles message routing, channel adapters, credential resolution, LLM proxy, and per-agent lifecycle.
- **Gateway-proxied LLM calls** — agent processes never see API keys.
- **Typed memory with audit trail** — facts categorized by kind (identity / preference / project / event), aged by recall freshness, with a supersedes chain for full history.
- **Skill curation** — agents create skills from successful workflows; an idle-triggered curator consolidates and ages the library.
- **Channel adapter protocol** — stable HTTP + WebSocket API for external adapter daemons (Telegram, Slack, etc.) without any platform-specific code in core.
- **Policy-as-data** — roles, quotas, and constraints are JSON, enforced at registry build time and every tool dispatch.
- **Single binary, single directory** — one `havn` CLI, one SQLite gateway DB, one SQLite per agent. No Postgres, no Redis.

## Architecture

```
                    Reverse proxy (operator's auth stack)
                                  │
                                  ▼
┌──────────────────────────────────────────────────────────┐
│                        Gateway                           │
│  Channel Adapters · Message Router · LLM Proxy           │
│  Credential Resolver · Cron Scheduler · Config Reload    │
│  Agent Spawner (namespace + Landlock + seccomp + cgroup)  │
└────────────────────────┬─────────────────────────────────┘
                         │ Unix socket
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
     ┌─────────┐   ┌─────────┐   ┌─────────┐
     │ Agent 1 │   │ Agent 2 │   │ Agent 3 │
     │ PID ns  │   │ PID ns  │   │ PID ns  │
     │ LLM loop│   │ LLM loop│   │ LLM loop│
     │ Tools   │   │ Tools   │   │ Tools   │
     │ Memory  │   │ Memory  │   │ Memory  │
     └─────────┘   └─────────┘   └─────────┘
```

## Quick Start

**Requirements:**

- **OS** — Linux only. Ubuntu 24.04 LTS recommended. Agent isolation relies on kernel features (user namespaces, Landlock ≥ 5.13, seccomp, cgroup v2) without macOS/Windows equivalents.
- **Rust** — 1.94+ — only for building from source. The installer downloads a prebuilt static binary by default, so no toolchain is needed.
- **Node.js** — 22+ (for the dashboard only)

### Install (prebuilt — no toolchain, no compile)

```bash
curl -fsSL https://raw.githubusercontent.com/amplimit/havn/main/install.sh | bash
```

This fetches a statically-linked `havn` binary from [Releases](https://github.com/amplimit/havn/releases) (verified by SHA-256), creates the `havn-wakecheck` sandbox user, and runs `havn setup`. Pin a version with `HAVN_VERSION=v0.1.1`, or force a source build with `HAVN_FROM_SOURCE=1`.

Then configure and start. Agent isolation (namespaces + cgroup v2) needs the gateway to run as root, or inside a systemd unit with `Delegate=yes`; without it the gateway refuses to start rather than run agents unisolated.

```bash
sudo -i                      # the gateway runs as root
havn setup --non-interactive # once, if install.sh ran setup as another user

# Set the encryption key for credentials (any passphrase you choose)
export HAVN_AGE_KEY="your-secret-passphrase"

# Start the gateway and keep it running
havn start

# In another shell: add your LLM credential (encrypted at rest).
# `credential add` talks to the running gateway.
havn credential add anthropic sk-ant-...
```

For a dev box without isolation, `HAVN_SPAWNER=subprocess havn start` runs agents as plain unsandboxed processes, and the gateway cannot tell which agent a connection comes from; do not use it for anything untrusted.

### Build from source

```bash
cargo build --release
./target/release/havn setup
# …then the same credential / start steps as above.
```

### Dashboard

The dashboard is not part of the release binaries. Run it on the gateway
host with Node.js 22+, from your source checkout or a fresh clone:

```bash
git clone https://github.com/amplimit/havn   # skip if you built from source
cd havn/apps/dashboard
npm install
npm run build
npm start                    # http://localhost:3000
```

Then open `http://localhost:3000`, create an agent, and start chatting. The
dashboard listens on 127.0.0.1 only and has no login of its own; from
another machine use an SSH tunnel (`ssh -L 3000:127.0.0.1:3000 <server>`) or
an authenticating reverse proxy. For development, `npm run dev` serves the
same app with hot reload. See
[apps/dashboard/README.md](apps/dashboard/README.md) for settings and
multi-user setup.

## Project Structure

```
crates/
  havn-core/       Shared types — IDs, messages, policy, errors
  havn-proto/      Wire protocol (agent↔gateway + channel adapter)
  havn-db/         SQLite pool, migrations, repositories
  havn-gateway/    Gateway server — routing, auth, LLM proxy, cron
  havn-runtime/    Agent runtime — LLM loop, tools, memory, curator
  havn-spawner/    Agent spawner — namespaces, cgroups, Landlock, seccomp
  havn-init/       PID-1 shim for agent namespace setup
  havn-cli/        CLI — setup, agent/credential/user/team management
apps/
  dashboard/       Next.js dashboard + WebChat
```

## Design Philosophy

havn is **infrastructure, not SaaS**. The test: does it require kernel primitives, the LLM proxy, or the per-agent SQLite? If yes, havn does it. If no, the operator wires it up.

| Concern | Where it lives |
|---------|----------------|
| User authentication | Operator's reverse proxy (Authelia, Keycloak, Cloudflare Access, Tailscale, …) |
| TLS termination | Reverse proxy |
| Outbound delivery to Slack/Telegram | External channel adapter daemons |
| Billing / cost in USD | Operator's analytics (havn logs token counts) |
| Skill sharing | GitHub / git clone |

## Target Deployment

- **Individual** — one $5 VPS, gateway + agents co-located, managed via CLI or dashboard
- **Small team (5–50)** — same single server, cgroup resource partitioning per agent, reverse proxy for auth

## Security Model

- Agents run in isolated Linux namespaces (PID, mount, network, user) with Landlock LSM and seccomp-bpf syscall filtering
- Agents do not start if Landlock is not available (kernel 5.13+ with `landlock` in the `lsm=` list; `havn doctor` checks it). To run agents without filesystem restriction on such a kernel anyway, start the gateway with `HAVN_ALLOW_NO_LANDLOCK=1`
- Agents with network egress reach the public internet only: traffic to the gateway host (including Docker-published ports), other agents, private, link-local (including cloud metadata), CGNAT and reserved addresses is dropped in `mangle PREROUTING` before any other firewall rule, and the host end of the agent link has no IPv6. The configured `agent_dns` servers stay reachable on port 53, also when they run on the gateway host; list other internal addresses agents may use under `[network] agent_private_access = ["10.0.5.20", "192.168.1.0/24"]`. If these rules cannot be installed (no `iptables`), agents get no network access at all. A `firewall-cmd --reload` or similar flush removes them; agents started afterwards get no network until the gateway restarts. A host-wide rule that redirects port 53 to a local resolver also breaks agent DNS, since the redirected destination is no longer an `agent_dns` server
- API keys encrypted at rest with [age](https://age-encryption.org/) — agent processes never see them
- Token caps enforced gateway-side: per credential (`max_tokens_per_day`), per user on a shared credential (`per_user.max_tokens_per_day`, `per_user.max_requests_per_minute`) and per agent across all credentials (policy `budget.max_tokens_per_day`, 1,000,000 for the seeded member role; `on_exhaust = "warn_only"` only logs, other values refuse the call). Each call reserves its worst case (estimated input plus `max_tokens`) before it is sent and is settled to the provider's reported usage afterwards, so parallel requests cannot overspend a cap; near a cap, `max_tokens` is lowered to what remains. Usage counts prompt-cache reads and writes and reasoning tokens; a response without usage keeps its reservation as the recorded usage. `max_tokens` is also clamped to the model's output limit for known Anthropic, OpenAI and Gemini models and to `[llm] max_tokens_ceiling`
- A policy's `allowed_models` is enforced by the gateway; entries are exact model names, or `*` for any model. For agents without network egress, the gateway also refuses LLM requests that would make the provider fetch a URL (URL-sourced images and documents, provider-side tools, models with built-in web search under common names); restrict `allowed_models` to be sure an agent cannot pick a model with its own web access
- WebChat WebSocket connections are checked against an Origin allowlist (`allowed_origins`) and must present the caller's `ws_token` from `GET /me`; the token identifies the user rather than being a secret, so the Origin check is what blocks cross-site connections
- No marketplace — skills are local files; sharing via git

## Configuration

Gateway config lives at `~/.config/havn/config.toml`:

```toml
listen = "127.0.0.1:8080"
db_path = "~/.local/share/havn/havn.db"
```

`max_tokens` on every LLM call is capped by `[llm] max_tokens_ceiling` (default 128000, the largest output limit of a known model; applied on `havn gateway restart`), whatever agents ask for. A running call holds a reservation of up to that many output tokens against the caps it is billed to, so with small caps on a shared credential, lower it to keep one user's large `max_tokens` from tying up everyone else's remaining budget:

```toml
[llm]
max_tokens_ceiling = 16000   # 0 is rejected
```

Config edits are picked up live: hot-reloadable fields (`allowed_origins`, `[[bindings]]`, `[[channels]]`) apply in place, while deploy-time fields (`listen`, `db_path`, paths) are logged as needing a `havn gateway restart` to apply — the gateway never restarts itself. Per-agent resource limits come from the agent's role policy, not a global config block.

For non-loopback exposure, configure a reverse proxy and enable trust-header mode:

```toml
listen = "0.0.0.0:8080"

[trust_header]
enabled = true
# Peers allowed to set the identity header (the address your proxy
# connects from); everyone else gets 403. Empty means loopback only.
allowed_proxies = ["192.0.2.10/32"]
# Header carrying the user's identity (default "X-User-ID").
header = "X-Forwarded-Email"
```

In trust-header (multi-user) mode:

- The operator creates teams (`havn team add --name <name> --admin <user-id>`). Team roles define what each member's agents may do.
- Team admins can set any policy on their team's roles, including MCP servers that mount host paths. Appoint them as you would an operator.
- A per-agent policy override can only narrow the member's role policy.
- The gateway-wide embedding configuration can only be changed in `config.toml`.

Cron jobs may carry a `wake_check` script. It runs on the host as the dedicated `havn-wakecheck` user (created by `install.sh`; the gateway must run as root to switch to it) with a minimal environment. Without that user the script is skipped and the job always wakes.

### Multi-user mode behind an SSO proxy

The gateway reads the user's identity from one header, `trust_header.header` (default `X-User-ID`), and only on connections from an `allowed_proxies` peer. It resolves the value to a havn user in this order:

1. When `match_user_id` is on and the value is an internal user id in canonical form (lowercase, hyphenated): that user. `match_user_id` defaults to on for `X-User-ID` and off for any other header. Turn it on for another header only if users cannot choose their own names at the identity provider, otherwise a user could take another user's internal id as their name.
2. The user whose external id equals the value. Matching is exact and case-sensitive: `Alice@example.com` and `alice@example.com` are different identities, so provision the value exactly as your proxy emits it. External ids are visible ASCII (inner spaces allowed), without commas, and cannot be UUIDs.

Anything else gets 403; users are never created on the fly. A request carrying the header more than once is refused. The header name cannot be one HTTP already uses (`Host`, `Cookie`, `Authorization`, `Connection`, ...).

Provision users with the value the proxy will send:

```sh
havn user add --external-id alice@example.com --display-name "Alice"   # generates the internal id
havn user add 0190a3c4-... --display-name "Bob"                         # internal id only (X-User-ID setups)
havn user list                                                          # id, name, external id
havn user set-external-id <existing-id> ops@example.com                 # e.g. the single-user bootstrap user
havn user set-external-id <existing-id> --clear
```

The dashboard asks the gateway for the header name at startup (`GET /auth/info`) and forwards that header only from the addresses in `HAVN_DASHBOARD_TRUSTED_PROXIES`. It drops other common identity headers such as `X-User-ID`, `Remote-User` and `X-Forwarded-Email` from every peer, and refuses `/api` and `/ws` requests while it does not know the name. It marks each forwarded request with `X-Havn-Identity-Header` naming the header it filtered for, and the gateway refuses a request marked for another name, so renaming `trust_header.header` cannot open a window before the dashboard notices. `HAVN_DASHBOARD_IDENTITY_HEADER` is optional; when set, the dashboard refuses to start if the gateway reads another header. When the dashboard runs on the gateway host, the gateway sees the dashboard as a loopback peer, so `allowed_proxies = ["127.0.0.1/32"]` and `listen = "127.0.0.1:8080"` cover it. Route both the UI and `/api` and `/ws` through the SSO proxy, without unauthenticated paths: a path the proxy skips would pass through whatever header the client sent.

#### oauth2-proxy

oauth2-proxy (a current v7 release) in front of the dashboard, all on one host, with a TLS-terminating proxy in front of oauth2-proxy. `--pass-user-headers` (on by default) makes oauth2-proxy replace `X-Forwarded-User` and `X-Forwarded-Email` on every request it forwards upstream, WebSocket upgrades included:

```sh
oauth2-proxy \
  --http-address=127.0.0.1:4180 \
  --upstream=http://127.0.0.1:3000/ \
  --provider=oidc --oidc-issuer-url=https://sso.example.com \
  --client-id=havn --client-secret-file=/etc/oauth2-proxy/client-secret \
  --cookie-secret-file=/etc/oauth2-proxy/cookie-secret \
  --email-domain=example.com \
  --pass-user-headers=true

HAVN_DASHBOARD_TRUSTED_PROXIES=127.0.0.1 \
HAVN_DASHBOARD_IDENTITY_HEADER=X-Forwarded-Email \
npm start   # in apps/dashboard
```

```toml
listen = "127.0.0.1:8080"

[trust_header]
enabled = true
allowed_proxies = ["127.0.0.1/32"]
header = "X-Forwarded-Email"
```

```sh
havn user add --external-id alice@example.com --display-name "Alice"
```

`X-Forwarded-User` carries the OIDC `sub` claim. Many providers (Keycloak, Authentik) use a UUID there, which cannot be an external id, so key users on the email. When oauth2-proxy runs as an nginx `auth_request` endpoint instead of in front of the dashboard, start it with `--set-xauthrequest` and copy its `X-Auth-Request-Email` response header into the forwarded request as in the next recipe.

#### nginx `auth_request` with Authelia

nginx terminates TLS, asks Authelia whether the request is authenticated, and copies Authelia's `Remote-User` response header into the request to the dashboard. `proxy_set_header` replaces whatever `Remote-User` the client sent:

```nginx
location /internal/authelia/authz {
    internal;
    proxy_pass http://127.0.0.1:9091/api/authz/auth-request;
    proxy_pass_request_body off;
    proxy_set_header Content-Length "";
    proxy_set_header X-Original-Method $request_method;
    proxy_set_header X-Original-URL $scheme://$http_host$request_uri;
    proxy_set_header X-Forwarded-For $remote_addr;
}

location / {
    auth_request /internal/authelia/authz;
    auth_request_set $user $upstream_http_remote_user;
    auth_request_set $redirection_url $upstream_http_location;
    error_page 401 =302 $redirection_url;

    proxy_set_header Remote-User $user;
    proxy_set_header Host $host;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection $connection_upgrade;   # map $http_upgrade $connection_upgrade { default upgrade; '' close; }
    proxy_pass http://127.0.0.1:3000;
}
```

```sh
HAVN_DASHBOARD_TRUSTED_PROXIES=127.0.0.1 HAVN_DASHBOARD_IDENTITY_HEADER=Remote-User npm start
```

```toml
listen = "127.0.0.1:8080"

[trust_header]
enabled = true
allowed_proxies = ["127.0.0.1/32"]
header = "Remote-User"
```

```sh
havn user add --external-id alice --display-name "Alice"   # the Authelia username
```

The same pattern works for any other proxy that replaces a header with the authenticated user's name. With Cloudflare Access, use `header = "Cf-Access-Authenticated-User-Email"` and set `allowed_proxies` to the address `cloudflared` connects from. havn does not check the `Cf-Access-Jwt-Assertion` token, so the origin must be reachable only through `cloudflared`, and an Access bypass policy forwards requests nobody has authenticated.

## Channel Adapters

Havn ships a stable HTTP + WebSocket protocol at `/api/v1/channel` for external adapter daemons. Adapters run outside havn's trust zone and translate between chat platforms and havn's wire format.

| Adapter | Repository | Status |
|---------|-----------|--------|
| Telegram | [havn-channel-telegram](https://github.com/amplimit/havn-channel-telegram) | Available |
| Slack | — | Planned |
| Discord | — | Planned |

An agent can only send through the channel accounts that `[[bindings]]` binds to it. With an account's `dm_policy = "allowlist"` (the default) it can only message senders in `allow_from`; with `dm_policy = "open"` it can message anyone the bot can reach on that platform, so use `open` only for bots meant to be public.

## Capability Packs

Some agent workloads need capabilities havn doesn't build into core (browser, code interpreter, vendor CLI). Havn provides generic primitives — bind mounts, tmpfs mounts, seccomp allowances — and external capability packs wire them together. Packs live in separate repositories and install under `/opt/havn/<pack>/`.

| Pack | Repository | Description |
|------|-----------|-------------|
| Browser | [havn-pack-browser](https://github.com/amplimit/havn-pack-browser) | Playwright CLI + Chromium for web browsing |

## License

Copyright 2025-2026 Amplimit

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
