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
- **Network tools** — `iptables` (nft or legacy backend), `ip` (iproute2) and `nsenter` (util-linux) on the gateway host, used to give agents outbound network. Without them the gateway still starts, but every agent runs with no network access. `install.sh` installs them via apt when they are missing; `havn doctor` checks for them. Minimal and container images (e.g. Docker `ubuntu:24.04`) often lack `iptables` and `iproute2`.
- **Rust** — 1.94+ — only for building from source (plus a C compiler and libc headers, e.g. `build-essential`). The installer downloads a prebuilt static binary by default, so no toolchain is needed.
- **Node.js** — 22+ (for the dashboard only)

### Install (prebuilt — no toolchain, no compile)

```bash
curl -fsSL https://raw.githubusercontent.com/amplimit/havn/main/install.sh | bash
```

This fetches a statically-linked `havn` binary from [Releases](https://github.com/amplimit/havn/releases) (verified by SHA-256), creates the `havn-wakecheck` sandbox user, and runs `havn setup`. Pin a version with `HAVN_VERSION=v0.1.1`, or force a source build with `HAVN_FROM_SOURCE=1`.

Then configure and start. Agent isolation (namespaces + cgroup v2) needs the gateway to run as root, either from a root shell or under the [systemd unit](#run-under-systemd); without it the gateway refuses to start rather than run agents unisolated. `havn doctor` reports what is missing (cgroup slice, network tools, Landlock).

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

### Run under systemd

On hosts running systemd, `install.sh` installs a reference unit at `/usr/local/lib/systemd/system/havn.service` (source: [contrib/systemd/havn.service](contrib/systemd/havn.service), also in the release tarball). It is not enabled. It runs the gateway as root with `HOME=/root`, so it uses the config and data under `/root` (install.sh runs `havn setup` for root when it installs the unit), reads `HAVN_AGE_KEY` from `/etc/havn/havn.env`, and restarts on failure:

```bash
sudo -i
HOME=/root havn setup --non-interactive         # once; install.sh already did this
install -d -m 700 -o root -g root /etc/havn
install -m 600 -o root -g root /dev/null /etc/havn/havn.env
$EDITOR /etc/havn/havn.env                      # add: HAVN_AGE_KEY='your-secret-passphrase'
systemctl enable --now havn
journalctl -u havn -f
```

Keep `/etc/havn/havn.env` mode 0600 and owned by root, and write the key with an editor rather than `echo`, which leaves it in the shell history. systemd takes the value inside single quotes literally; it must not contain `'` or `\`.

Agent cgroups are `/sys/fs/cgroup/havn.slice/agent-<id>`. The gateway creates that top-level cgroup itself and moves agents into it, outside the unit's own cgroup, so this is not systemd delegation; `Delegate=yes` only keeps the cpu, memory and pids controllers enabled from the root cgroup down. `/sys/fs/cgroup/havn.slice` is also where systemd would put a slice unit called `havn.slice`: never create such a unit, set `Slice=havn.slice`, or run anything with `--slice=havn`, or systemd adopts the cgroup, rewrites its controllers (which can drop the agents' memory limits) and may remove it. Put local changes in a drop-in (`systemctl edit havn`) rather than editing the installed file, which a re-install overwrites.

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
- Per-credential token-per-day and requests-per-minute caps enforced gateway-side
- A policy's `allowed_models` is enforced by the gateway; entries are exact model names, or `*` for any model. For agents without network egress, the gateway also refuses LLM requests that would make the provider fetch a URL (URL-sourced images and documents, provider-side tools, models with built-in web search under common names); restrict `allowed_models` to be sure an agent cannot pick a model with its own web access
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

In trust-header (multi-user) mode:

- The operator creates teams (`havn team add --name <name> --admin <user-id>`). Team roles define what each member's agents may do.
- Team admins can set any policy on their team's roles, including MCP servers that mount host paths. Appoint them as you would an operator.
- A per-agent policy override can only narrow the member's role policy.
- The gateway-wide embedding configuration can only be changed in `config.toml`.

Cron jobs may carry a `wake_check` script. It runs on the host as the dedicated `havn-wakecheck` user (created by `install.sh`; the gateway must run as root to switch to it) with a minimal environment. Without that user the script is skipped and the job always wakes.

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
