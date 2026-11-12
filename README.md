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

For a dev box without isolation, `HAVN_SPAWNER=subprocess havn start` runs agents as plain unsandboxed processes; do not use it for anything untrusted.

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

Then open `http://localhost:3000`, create an agent, and start chatting. For
development, `npm run dev` serves the same app with hot reload. See
[apps/dashboard/README.md](apps/dashboard/README.md) for settings.

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
- Agents with network egress reach the public internet only: traffic to the gateway host, other agents, private, link-local (including cloud metadata), CGNAT and multicast addresses is dropped, and the agent link has no IPv6. The configured `agent_dns` servers stay reachable; list other internal addresses agents may use under `[network] agent_private_access = ["10.0.5.20", "192.168.1.0/24"]`
- API keys encrypted at rest with [age](https://age-encryption.org/) — agent processes never see them
- Per-credential token-per-day and requests-per-minute caps enforced gateway-side
- WebChat WebSocket connections are checked against an Origin allowlist (`allowed_origins`) and must present the caller's `ws_token` from `GET /me`; the token identifies the user rather than being a secret, so the Origin check is what blocks cross-site connections
- No marketplace — skills are local files; sharing via git

## Configuration

Gateway config lives at `~/.config/havn/config.toml`:

```toml
listen = "127.0.0.1:8080"
db_path = "~/.local/share/havn/havn.db"
```

Config edits are picked up live: hot-reloadable fields (`allowed_origins`, `[[bindings]]`, `[[channels]]`) apply in place, while deploy-time fields (`listen`, `db_path`, paths) are logged as needing a `havn gateway restart` to apply — the gateway never restarts itself. Per-agent resource limits come from the agent's role policy, not a global config block.

For non-loopback exposure, configure a reverse proxy and enable trust-header mode:

```toml
listen = "0.0.0.0:8080"

[trust_header]
enabled = true
# Peers allowed to set X-User-ID; everyone else gets 403.
# Empty means loopback only (a proxy on the same host).
allowed_proxies = ["127.0.0.1/32"]
```

In trust-header (multi-user) mode, team roles define what each member's agents may do; a per-agent policy override can only narrow the member's role policy, and the gateway-wide embedding configuration can only be changed in `config.toml`.

Cron jobs may carry a `wake_check` script. It runs on the host as the dedicated `havn-wakecheck` user (created by `install.sh`; the gateway must run as root to switch to it) with a minimal environment. Without that user the script is skipped and the job always wakes.

## Channel Adapters

Havn ships a stable HTTP + WebSocket protocol at `/api/v1/channel` for external adapter daemons. Adapters run outside havn's trust zone and translate between chat platforms and havn's wire format.

| Adapter | Repository | Status |
|---------|-----------|--------|
| Telegram | [havn-channel-telegram](https://github.com/amplimit/havn-channel-telegram) | Available |
| Slack | — | Planned |
| Discord | — | Planned |

## Capability Packs

Some agent workloads need capabilities havn doesn't build into core (browser, code interpreter, vendor CLI). Havn provides generic primitives — bind mounts, tmpfs mounts, seccomp allowances — and external capability packs wire them together. Packs live in separate repositories and install under `/opt/havn/<pack>/`.

| Pack | Repository | Description |
|------|-----------|-------------|
| Browser | [havn-pack-browser](https://github.com/amplimit/havn-pack-browser) | Playwright CLI + Chromium for web browsing |

## License

Copyright 2025-2026 Amplimit

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
