# Havn

Open infrastructure for running autonomous AI agents. Single binary, single SQLite file, Linux-native process isolation.

havn is a self-hosted server that runs a gateway and per-user agents. Identity, network exposure, encryption keys, and external chat-platform integrations come from the operator's own stack. havn focuses on what nobody else does well: kernel-level agent isolation, typed memory, skill curation, credential proxying, and a usable single-binary deployment.

## Key Features

- **Linux-native process isolation** — namespace + Landlock LSM + seccomp-bpf + cgroup v2 per agent. No Docker daemon, no per-user VMs, near-zero overhead.
- **Single gateway, many agents** — one process handles message routing, channel adapters, credential resolution, LLM proxy, and per-agent lifecycle.
- **Gateway-proxied LLM calls** — agent processes never see API keys.
- **Typed memory with audit trail** — facts categorized by kind (identity / preference / project / event), aged by recall freshness, with a supersedes chain for full history.
- **Skill curation** — agents create skills from successful workflows; an idle-triggered curator consolidates and ages the library.
- **Channel adapter protocol** — stable HTTP + WebSocket API for external adapter daemons (Telegram, Lark, Discord, ...) without any platform-specific code in core.
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
- **Network tools** — `iptables` (nft or legacy backend), `ip` (iproute2) and `nsenter` (util-linux) on the gateway host, used to give agents outbound network. Without `iptables` or `nsenter` the gateway still starts, but every agent runs with no network access; without `ip` it refuses to start (it checks the host routes for the agent address pool). `install.sh` installs them via apt when they are missing; `havn doctor` checks for them. Minimal and container images (e.g. Docker `ubuntu:24.04`) often lack `iptables` and `iproute2`.
- **Rust** — 1.94+ — only for building from source (plus a C compiler and libc headers, e.g. `build-essential`). The installer downloads a prebuilt static binary by default, so no toolchain is needed.
- **Node.js** — 22.13+ (for the dashboard only)

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

# The models agents may use, as provider:model; the first one added is the
# default. With an OpenRouter key: havn model add openrouter:<vendor>/<model>
havn model add anthropic:claude-sonnet-4-6
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

One gateway runs per database and agent socket, and one gateway that isolates agents per host (with `HAVN_SPAWNER=subprocess` only the first applies). A second one, such as `havn start` while the unit runs, exits with an error naming the running one's pid instead of starting; the locks are `<db_path>.lock`, `<agent_socket_path>.lock` and `/run/havn-gateway.lock`. A gateway from release v0.1.3 or earlier holds none of them, so restart the unit after upgrading before starting any other gateway.

Keep `/etc/havn/havn.env` mode 0600 and owned by root, and write the key with an editor rather than `echo`, which leaves it in the shell history. systemd takes the value inside single quotes literally; it must not contain `'` or `\`.

Agent cgroups are `/sys/fs/cgroup/havn/agent-<id>`. The gateway creates that top-level cgroup itself and moves agents into it, outside the unit's own cgroup, so this is not systemd delegation; `Delegate=yes` only keeps the cpu, memory and pids controllers enabled from the root cgroup down. `havn` is not a unit name, so systemd leaves the cgroup alone. Older gateways used `/sys/fs/cgroup/havn.slice`: at start-up the gateway kills the agents still running there, removes their cgroups, and removes `havn.slice` itself unless systemd has an active `havn.slice` unit or something else lives in it. Put local changes in a drop-in (`systemctl edit havn`) rather than editing the installed file, which a re-install overwrites.

### Upgrading

From v0.2.9, `sudo havn update` upgrades a gateway that runs as the systemd unit `havn` (as install.sh sets it up). It reads that gateway's `HAVN_CONFIG` and `HOME` to find its config, data and `listen` address. Then it: downloads the latest release (or `--version v0.2.10`) for this machine and checks the archive's sha256 against the release's checksum file (which shows the download is whole, not who made it: both come from the same release, as for install.sh); unpacks its binaries; backs up the installation (to `~/havn-backup-before-<version>-<time>.tar.gz`, root's home under `sudo`; `--no-backup` skips it); puts each binary in place of the one beside this `havn` (the previous ones kept as `<name>.old`, the gateway last); restarts the unit; and waits for the gateway to answer as the new release, for as long as systemd keeps it running (a large database takes a while to migrate), up to 10 minutes. A gateway that fails, or that systemd restarts, is given back its previous binaries and restarted; if it had migrated its databases by then, restore the backup with `havn restore`. A release older than the gateway's needs `--allow-downgrade`, as its gateway cannot open databases a newer one migrated. A gateway that runs otherwise (in a shell, another unit) is not touched: stop it, run `havn update`, which then only puts the binaries in place, and start it. `havn update --check` only says whether a newer release is out. Channel adapters (Telegram, Lark, Discord) are updated on their own, and the release's `havn.service` is not installed over yours. The gateway also looks for a newer release once a day (from GitHub's API, a minute after it starts) and the dashboard shows one beside its version to everyone who uses it; `update_check = false` in config.toml turns that off.

By hand, or up to v0.2.8:

```bash
sudo -i
# First a backup, by the havn that made the data (see Backup and restore);
# a failed backup stops the rest
havn backup -o ~/havn-backup-$(date +%Y%m%d-%H%M%S).tar.gz &&
# Replace the binaries with the release you move to; the running gateway is not touched yet
curl -fsSL https://raw.githubusercontent.com/amplimit/havn/main/install.sh | HAVN_VERSION=<version> bash &&
systemctl restart havn
```

A stopping gateway takes no new messages and gives the agents up to 30 seconds to finish what they are doing. Open chats still get those replies, and are told when one was cut off. Gateways up to v0.1.4 stop their agents at once, so the stop before leaving one of them drops work in progress; choose a quiet moment for it. Agents that were running start again with the gateway, and the dashboard reconnects on its own.

Up to v0.2.0 havn has no `havn backup`: there, stop the gateway and copy its data instead (`systemctl stop havn && cp -a ~/.local/share/havn ~/havn-backup-$(date +%Y%m%d-%H%M%S)`, and a `db_path` or `workspace_root` elsewhere too), then install and start it.

The new gateway and agents migrate their databases when they start, and there is no way back, so keep the backup until the new version works. The dashboard comes with the gateway and is upgraded with it. Up to v0.2.7 it ran as a Node process of its own: when moving from one of those, stop and remove that process before restarting the gateway, which serves the dashboard on `dashboard_listen` (`127.0.0.1:3000` by default); set `dashboard_listen` to the address the old one used to keep your tunnels and proxy as they are. An open dashboard tab still runs the old page, and says so beside the version: reload it.

### Backup and restore

```bash
sudo -i
havn backup                          # havn-backup-<time>.tar.gz in this directory; -o names the file
systemctl stop havn
havn restore havn-backup-<time>.tar.gz [--force]
systemctl start havn
```

`havn backup` writes one archive (mode 0600, under a temporary name until it is whole) holding:

- a consistent copy of the gateway's database and of every agent's (SQLite `VACUUM INTO`), so the gateway and its agents may keep running;
- every agent's directory: workspace, inbox, outbox and home, with owners, modes and times;
- `config.toml`.

Links in workspaces are kept as links and never followed, and sparse files (with holes) of 64 MB or more are left out. An agent's database that keeps changing while it is copied, or cannot be read, is left out of that backup with a warning (the archive's manifest lists it), and the rest is backed up; `havn backup` then exits with an error, so a timer shows it. Write the archive outside `workspace_root`. A backup does not hold `HAVN_AGE_KEY`: the credentials in it stay encrypted, and restoring them needs the same key, so keep the key apart from the archives.

`havn restore` reads the archive through before it changes anything. It needs the gateway stopped (and holds the gateway's lock while it runs), and a havn that knows the backup's database migrations, so a backup made just before an upgrade restores with the version it came from; the gateway migrates the databases when it starts. Onto a host that already has an installation it refuses unless `--force`, which moves the gateway's database files and `workspace_root` aside to `<path>.pre-restore-<time>` first. A `config.toml` already in place is kept, and the archive's is written beside it as `config.toml.restored`; on a host with none, the archive's goes in place, and the database and agents go where it says.

Moving to a new host:

```bash
sudo -i
curl -fsSL https://raw.githubusercontent.com/amplimit/havn/main/install.sh | HAVN_SKIP_SETUP=1 bash
install -d -m 700 /etc/havn && install -m 600 /dev/null /etc/havn/havn.env
sudoedit /etc/havn/havn.env          # HAVN_AGE_KEY='<the old host's key>'
havn restore havn-backup-<time>.tar.gz
systemctl enable --now havn
```

A daily backup with systemd, keeping two weeks:

```ini
# /etc/systemd/system/havn-backup.service
[Service]
Type=oneshot
# Where the gateway's unit finds its config and data
Environment=HOME=/root
EnvironmentFile=-/etc/havn/havn.env
WorkingDirectory=/var/backups/havn
ExecStart=/usr/local/bin/havn backup
# After a failed backup too, so old archives never pile up
ExecStopPost=/usr/bin/find /var/backups/havn -name 'havn-backup-*.tar.gz' -mtime +14 -delete

# /etc/systemd/system/havn-backup.timer
[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

Create the directory (`install -d -m 700 /var/backups/havn`), then `systemctl enable --now havn-backup.timer`. Copy the archives off the host as well; a backup on the same disk does not survive the disk.

### Build from source

```bash
cargo build --release
./target/release/havn setup
# …then the same credential / start steps as above.
```

### Dashboard

The gateway serves the dashboard of its own release, with nothing else to
install or start: open `http://localhost:3000`, create an agent, and start
chatting. It is served on `dashboard_listen` in config.toml
(`"127.0.0.1:3000"` by default, `""` for none), with the management API under
`/api` and the web chat under `/ws` on the same origin.

It has no login of its own. Without `trust_header` it answers only requests
addressed to `localhost`, `127.0.0.1` or `[::1]` (or to an origin listed in
`allowed_origins`), so a web page whose host name leads to 127.0.0.1 cannot
use it; the management API on `listen` checks the same. From another machine use an SSH tunnel
(`ssh -L 3000:127.0.0.1:3000 <server>`) or an authenticating reverse proxy
(see [Multi-user mode behind an SSO proxy](#multi-user-mode-behind-an-sso-proxy)).

A gateway built from source embeds the dashboard when it is built first
(`npm ci && npm run build` in `apps/dashboard`, with Node.js 22.13+;
install.sh does this when it finds npm); without it, the port says how. See
[apps/dashboard/README.md](apps/dashboard/README.md) for the development
server (`npm run dev`).

### Usage

The dashboard's **usage** page (`GET /usage?days=30`) shows what the caller's model calls used, for 7 days to a year: calls and tokens, and their cost in US dollars for the models whose `[[llm.models]]` entry has a `price` (see Configuration), in all and by day, agent and model. Days are whole UTC days. Each call is counted under the model asked for, as `[[llm.models]]` names it, and input tokens read from and written to a provider's prompt cache are priced apart; both are recorded from v0.2.9, so calls from before are counted under the name the provider answered with (often not a configured one, so unpriced) and their cache tokens at the input price. A call still running, or whose provider reported no usage, counts at what was reserved for it, its most, and the page says how many there are. Searches through a search backend (Brave, Tavily, Exa) are counted apart, not as model calls. Usage on a credential goes with it when it is deleted. Without `trust_header`, the gateway's owner (its first user, made when it first starts: you, on a single-user gateway) can look at any user's, or everyone's (`?user=<id>` or `?user=all`), with a total by user; with `trust_header`, everyone sees only their own. A team's admins keep the team's usage page.

### Metrics

Set `metrics_listen` in config.toml (for example `"127.0.0.1:9464"`; empty, the default, serves none) for the gateway to serve Prometheus metrics at `/metrics` there, in the text format, with no login: keep it on loopback or a network only your Prometheus reaches (the gateway warns when it is not loopback, but serves it, unlike `listen`). It must be a port of its own, and applies on `havn gateway restart`; if the port is taken, the gateway says so and runs without metrics. The counters start at zero when the gateway starts:

- `havn_build_info{version}`, `havn_uptime_seconds`;
- `havn_agents_connected` and `havn_channel_adapters_connected`, now;
- `havn_llm_calls_total{provider, model, outcome}`, the LLM calls made for agents, each call to a provider counted once whatever credentials it tried (a request the gateway sends again, shortened, counts again; so does each model `web_fetch` tries). Web search through OpenRouter is not counted here, and the gateway's own calls, such as checking a model, are not counted. `outcome` is `ok`, `no_credential`, `unavailable`, `upstream_error`, `transport_error`, `budget_exhausted` or `other_error`;
- `havn_llm_tokens_total{provider, model, direction}`, the input (cache reads and writes included) and output tokens of the calls that answered;
- `havn_channel_messages_total{channel, direction}`, the messages adapters passed in (all of them, those turned away too) and the replies written to adapters (each part of a reply sent in parts counts; one written again after an adapter reconnects counts once);
- `havn_cron_runs_total{outcome}`, the runs of scheduled tasks: `success`, `failure` (also one that could not be sent to its agent, or whose output could not be kept), or `skipped` by their wake check.

A counter keeps at most 1000 models per provider, and 1000 channels; past that, a new one is counted as `__other__`.

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
- **Several customers per server** — one unprivileged LXD container per customer, each with its own gateway; see [docs/multi-tenant.md](docs/multi-tenant.md)

## Security Model

- Agents run in isolated Linux namespaces (PID, mount, network, user) with Landlock LSM and seccomp-bpf syscall filtering
- The agent runtime, its tools and MCP servers hold no capabilities, and cannot regain them by exec. In the agent's own network namespace they can still use `localhost` (also without egress), `ping` and bind ports below 1024, and the host directories bound into the agent are `nosuid`, so setuid bits and file capabilities there do not take effect.
- Under a root gateway each agent runs as host uid and gid of its own (from `0x71000000` up, never reused; inside a container that maps fewer ids, from the middle of the ids above 65535 it maps), with no supplementary groups: the agent's root is not the host's. It owns its workspace, which gateways before 0.1.7 left to root and is handed over on the agent's next start (files copied into a workspace as root afterwards are not: `chown` them to the workspace's owner), and only root can enter the directory above it, so no other host user reaches a file the agent made setuid. A read-write `extra_mounts` directory is writable to an agent only where its permissions or ACLs let that uid write; files an agent creates there belong to its uid
- Programs installed on the host under `/usr` are available to agents, read-only. Of the host's `/etc` and `/var`, agents see `/etc/ssl/certs`, with network egress a generated `/etc/resolv.conf`, and, read-only and where present, the directories installed programs read at run time: `/etc/alternatives` (the links behind `java`, `cc`, `awk` and others on Debian and Ubuntu), `/etc/fonts`, `/etc/groff`, `/etc/libreoffice`, `/etc/perl`, `/etc/python3`, `/etc/python3.*`, `/etc/texmf`, `/etc/java-*`, `/etc/ImageMagick-*`, `/var/lib/ghostscript` and `/var/lib/texmf`. An `extra_mounts` entry for one of these paths replaces the default. Shell commands and other tool processes get `HOME` at `.havn/home` in the agent's workspace (ignored by git). MCP servers get `HOME=/nonexistent` unless their `env` sets one, so nothing a tool leaves in its home runs in them
- Agents do not start if Landlock is not available (kernel 5.13+ with `landlock` in the `lsm=` list; `havn doctor` checks it). To run agents without filesystem restriction on such a kernel anyway, start the gateway with `HAVN_ALLOW_NO_LANDLOCK=1`
- Agents cannot create user namespaces (`unshare`/`clone` with `CLONE_NEWUSER` fail with `EPERM`; `clone3` reports `ENOSYS`, so callers fall back to `clone`), which keeps the kernel code a user namespace opens to an unprivileged process out of their reach. Capability packs whose workload nests them (Chromium's sandbox) need `[seccomp] allow_extra = ["user_namespaces"]`
- Every process a tool starts (shell commands, and git for auto-commits) runs in a Landlock domain nested under the runtime's, so it cannot read the environment or memory of MCP servers, which stay in the runtime's domain, nor their `/proc/<pid>/fd` and `cwd`. It can still see their command lines (give an MCP server secrets through `env`, not `args`), and it can read and write the files under any MCP server's `extra_paths_rw`/`extra_paths_ro`, which every agent process may use. Code an MCP server runs on the agent's behalf (a git server running hooks from the agent's repository, a server that executes commands) stays in the runtime's domain, so do not give such servers to agents alongside servers that hold secrets. Each tool call gets a domain of its own: a command cannot read those `/proc` entries of a process an earlier call left running, though `ps`, `kill` and `pkill` still work
- An agent's network access has three settings (`network_policy.egress_allowed` and `egress_proxy`, on the agent's settings page): off; allowed domains, through the gateway's egress proxy; or every public address. Through the proxy, the agent's namespace has no route past the gateway host, anything it sends to an address not of the host is dropped, and its tools get `HTTP_PROXY`/`HTTPS_PROXY` for `http://<its link's host address>:<[network] egress_proxy_port>` (913). The gateway binds that port before starting agents and does not start without it; a host firewall that filters input (ufw, firewalld) must let the agent pool reach it. The proxy tunnels HTTPS (`CONNECT`, port 443) and forwards plain HTTP (port 80, with `Content-Length` bodies only) to public addresses of allowed domains; a blocked domain is refused, and for a domain on neither list the owner is asked in the dashboard chat: allow for 10 minutes, always allow (added to the agent's allowed domains), or deny. A denial holds for 10 minutes, and an agent raises at most 5 new questions a minute. The listed hosts of common package sources (PyPI, npm, crates.io, the Go proxy, GitHub, Debian and Ubuntu archives) are asked about together, with the source named. Unanswered questions are refused after 5 minutes; answers are audited as `egress.domain`. The proxy checks the host name the tool asks for, not what a TLS tunnel carries: a service that routes requests by their inner `Host` (domain fronting on a shared CDN) can reach other sites behind it, so allow only domains you would allow everything behind. Agents created from now on get `[network] default_egress` (`allowlist` unless set to `off` or `all`) unless their config says otherwise; existing agents keep their setting, and a change takes effect at the agent's next start. Through the proxy, tools that do not honour the proxy variables (Node's built-in `fetch`, for one), or that use other protocols (git over SSH), have no network.
- Agents with network egress reach the public internet only: traffic to the gateway host (including Docker-published ports), other agents, private, link-local (including cloud metadata), CGNAT and reserved addresses is dropped in `mangle PREROUTING` before any other firewall rule, and the host end of the agent link has no IPv6. The configured `agent_dns` servers stay reachable on port 53, also when they run on the gateway host; list other internal addresses agents may use under `[network] agent_private_access = ["10.0.5.20", "192.168.1.0/24"]`. If these rules cannot be installed (no `iptables`), agents get no network access at all. A `firewall-cmd --reload` or similar flush removes them; agents started afterwards get no network until the gateway restarts. A host-wide rule that redirects port 53 to a local resolver also breaks agent DNS, since the redirected destination is no longer an `agent_dns` server
- Search queries leave the host even for agents without network egress: `web_search` sends them to the search backend, so an agent could put data from its workspace into a query. Turn off `can_search_web` for agents whose data must not leave
- `web_fetch` requests also leave the host. An agent reads only URLs users sent it, search results and links on pages it read, or its allowed domains, so it cannot put workspace data into a URL it makes up. A page it was sent to still sees the request, and a malicious page can lead it on: a site that serves pages of links can read data out of an agent that follows them, one link choice at a time, within the per-user rate. Only allow domains whose pages you trust not to echo URLs or redirect anywhere. Turn off `can_fetch_web` for agents that must not reach the web at all. Without egress, the sandbox itself still has no network: `web_fetch` is not a way for shell commands to download
- API keys encrypted at rest with [age](https://age-encryption.org/) — agent processes never see them
- Token caps enforced gateway-side: per credential (`max_tokens_per_day`), per user on a shared credential (`per_user.max_tokens_per_day`, `per_user.max_requests_per_minute`) and per agent across all credentials (policy `budget.max_tokens_per_day`, 1,000,000 for the seeded member role; `on_exhaust = "warn_only"` only logs, other values refuse the call). Each call reserves its worst case (estimated input plus `max_tokens`) before it is sent and is settled to the provider's reported usage afterwards, so parallel requests cannot overspend a cap; near a cap, `max_tokens` is lowered to what remains. Usage counts prompt-cache reads and writes and reasoning tokens. An agent's requests to Anthropic models, directly or through OpenRouter, mark the part of the system prompt every turn shares (with the tools before it) and the end of the conversation for Anthropic's prompt cache, so a turn's later rounds and the agent's next turn read them at the cache price. For that the system prompt stays the same from turn to turn: a conversation's prompt names the owner's time zone, and each of the person's messages starts with when it was sent (`[Sent Fri 2026-11-20 09:30]`), the newest being the time now. This keeps the conversation cached by providers that cache prompts on their own too. One-off calls (titles, summaries, page answers) are not marked. Cache reads reported as OpenAI's `prompt_tokens_details` or DeepSeek's `prompt_cache_hit_tokens` are counted apart; a response without usage keeps its reservation as the recorded usage. `max_tokens` is also clamped to the model's `max_output_tokens` in `[[llm.models]]` (or, without one, its known output limit for Anthropic, OpenAI and Gemini models) and to `[llm] max_tokens_ceiling`
- Agents can only use the models in `[[llm.models]]`. A policy's `allowed_models` narrows that further; entries are `provider:model` ids, or `*` for any listed model. For agents without network egress, the gateway also refuses LLM requests that would make the provider fetch a URL (URL-sourced images and documents, provider-side tools, models with built-in web search under common names); restrict `allowed_models` to be sure an agent cannot pick a model with its own web access
- WebChat WebSocket connections are checked against an Origin allowlist (`allowed_origins`) and must present the caller's `ws_token` from `GET /me`; the token identifies the user rather than being a secret, so the Origin check is what blocks cross-site connections
- No marketplace — skills are local files; sharing via git

## Policy

What an agent may do is one JSON policy (`havn_core::Policy`). Its top-level keys, with the defaults a key left out takes:

- `max_agents`: how many agents a user may create (1; a user in no team gets 50, or `HAVN_MAX_AGENTS_PER_USER`).
- `allowed_models`: `provider:model` ids the agent may use, or `*` for any of `[[llm.models]]` (`["*"]`); see [Configuration](#configuration).
- `resource_limits`: the agent's cgroup limits, `memory_mb` (512), `cpu_cores` (1.0) and `pids_max` (64).
- `budget`: `max_tokens_per_day` (0, no cap) and `on_exhaust` (`warn_only`, `warn_and_pause` or `fail`; `warn_and_pause`); see [Security Model](#security-model).
- `permissions`: switches for tools and capabilities: `can_use_shell`, `can_access_network`, `can_install_skills`, `can_schedule_cron`, `can_search_web` and `can_fetch_web` (on), `can_use_mcp`, `can_spawn_subagents` and `can_query_other_agents` (off). `can_view_own_logs`, `can_export_memory` and `can_bind_channels` are part of the schema, but nothing in Havn checks them.
- `network_policy`: `egress_allowed`, `egress_proxy`, `allowed_domains` and `blocked_domains`; see [Security Model](#security-model) and [Reading web pages](#reading-web-pages).
- `context_toolsets`: tool names turned off in one context, as `{"cron": {"disabled": ["shell"]}}`. The contexts are `webchat` (messages, from the dashboard or a chat app), `heartbeat`, `cron`, `subagent` and `agent_query`.
- `admin_visibility`: `can_view_audit_log` lets a team admin holding the role read the team's audit log; `can_view_agent_status`, `can_view_agent_config` and `can_view_conversations` are part of the schema, but nothing in Havn checks them.
- `mcp_servers`: see [MCP servers](#mcp-servers).
- `hooks`: see [Tool Hooks](#tool-hooks).
- `chat_titles`: see [Chat titles](#chat-titles).
- `tool_rounds`: `max` (100), the tool rounds one message may take, and `subagent` (30), the rounds of a subagent's task, another agent's question, a heartbeat or a cron job. At the limit the model is asked to sum up what it did and what is left.
- `shell`: `timeout_seconds` (300), how long a command runs when it does not say; `max_timeout_seconds` (1800), the most it may ask for; `background` (8), commands running in the background at once.

Policies are set in two places:

- **Team roles.** Each team member holds a role whose policy governs their agents. The operator sets it with `havn role add <team-id> <name> --policy <file>` and `havn role set-policy <role-id> <file>`, which replaces the whole policy with the file's (keys left out take the defaults above; `havn role show <role-id>` prints the current one). Team admins edit it as JSON on the dashboard's team roles page (`PATCH /teams/{id}/roles/{role_id}`). A policy is validated when it is saved. An agent created in a team gets the owner's role in that team; for their other agents, a user in several roles gets the widest of them (every role's hook rules and blocked domains still apply). A user in no team gets the defaults.
- **The agent's override**, `config.policy`. It is laid over the owner's policy key by key, so it changes only the keys it sets; `mcp_servers` and `context_toolsets` it replaces as a whole. The agent settings page writes parts of it (network and web access, MCP servers, tools that need approval, chat titles); `PATCH /agents/{id}` with `config.policy` replaces the stored override as a whole.

On a single-user gateway the override applies as it is, except that it cannot remove the owner's hook rules or domain lists, and subagents are on unless the override turns them off. In multi-user mode (`trust_header`) it is clamped to the owner's policy: a switch is on only where both have it on, a limit is the lower of the two (a 0, no limit, gives way to the other), `on_exhaust` is the stricter, `allowed_models` keeps only what both allow, domain lists and hook rules add up, egress stays on the proxy when the owner's policy has it there, `admin_visibility` is the owner's, and only MCP servers the owner's policy lists are kept, with the owner's settings.

The policy an agent's tools and sandbox get is resolved when the agent starts, so changes apply on its next start. The gateway's own checks (model calls, budgets, `web_fetch`, queries to other agents) resolve it on every request and see a change at once.

## Tool Hooks

PreToolUse hooks refuse a tool call before it runs when one of its regex patterns matches the call's main input (the shell command, the file path, the URL). The agent sees the refusal as the tool result and can adjust its plan.

Operators put hooks in the policy (a role's policy, or a per-agent override), under `hooks`:

```json
"hooks": {
  "pre_tool_use": [
    { "tool": "shell", "deny": ["^rm -rf", "^sudo "] },
    { "tool": "web_fetch", "deny": ["://internal\\.corp\\.net"] }
  ]
}
```

An agent workspace can also contain `.havn/hooks.yaml` with the same `pre_tool_use` list, so an existing file can be pasted into the policy as is. The agent can edit its own workspace, so this file can only add rules: the runtime applies the policy's rules and the file's rules together, and changing or deleting the file never removes a policy rule. Both are read when the agent starts. A per-agent policy override can add hook rules but cannot remove its owner's, and a user with several roles gets every role's rules. A policy holds at most 64 rules and 256 patterns of up to 1024 bytes each, and an invalid policy is rejected when it is saved. The workspace file is read leniently: unknown keys are ignored, and invalid patterns and rules beyond those caps are skipped with a warning. A rule for a tool the agent does not have is logged as a warning when the agent starts.

Hooks are guardrails against model mistakes, not a security boundary. A pattern matches text, and an agent that wants to can run the same action in a form no pattern matches (`rm  -rf`, `cd / && rm -rf`, a script file). To actually prevent an action, use the policy permissions (`can_use_shell`, `can_access_network`), `network_policy`, the mounts an agent gets, and the sandbox described above.


A rule can also `ask` instead of `deny`: a call whose flattened input matches an `ask` pattern (and no `deny` pattern) waits until the agent's owner approves it. Every tab of the agent's dashboard chat shows the tool and its whole input with approve, approve for this conversation (the same tool and input, until the agent restarts) and deny, also tabs opened or reloaded while it waits; a channel conversation it came from is told it waits, since a channel's sender (a customer, say) must not approve the agent's actions. A call nobody answers within 10 minutes is refused, and so is one whose input is longer than 4000 characters, and one in a scheduled run, a heartbeat or another agent's query, where nobody can be asked. An `ask` pattern in `.havn/hooks.yaml` that does not compile asks about every call of its tool. `tool` may end in `*` to cover a prefix, such as every tool of one MCP server:

```yaml
pre_tool_use:
  - tool: mcp__github__*
    ask: ["(?s).*"]
  - tool: shell
    ask: ["\\bdeploy\\b", "^git push"]
```

Agents find files with `file_find` (by name, with a glob) and `file_search` (by content), which need no `shell`. They leave out the files that rules about `file_read` (or `file_*`) deny or ask about, matched against each file's path in the workspace, so a search cannot read what `file_read` may not; they never follow links, and skip `.git`, `.havn` and the agent's database.

`view_image` shows the agent an image of its workspace (PNG, JPEG, GIF, WebP or HEIC): a screenshot it took, a chart it drew, a picture it downloaded. The gateway reads the file, without following links, and puts it in the tool's result for Anthropic. For OpenAI-compatible providers it goes in a message right after the results, because their tool results carry no images; for Gemini it goes after the function's response. It is subject to the same limits as attached images: a model without `vision` gets a line saying so; one over 3.75 MB or 8000 pixels on a side, or HEIC, goes as a copy the gateway makes for the request (up to 25 MB read; nothing is written); past the count or total size of images it is not shown. Later turns keep a line naming the image instead of the image itself, so the agent views it again when it needs it. Rules about `file_read` (or `file_*`) apply to it, and disabling `file_read` in a context disables it too.

The agent settings page keeps a list of tools whose every call needs approval (`GET` and `PUT /agents/{id}/approvals`). Requests and decisions are audited as `tool.approval.requested` and `tool.approval.decided`, with who decided. Like `deny`, `ask` guards against model mistakes on matching inputs; the sandbox and permissions stay the enforcement.

## MCP servers

Agents can use the tools of MCP (Model Context Protocol) servers the operator installed. A server is a binary in `/usr/share/havn/mcp-servers/` on the host, which agents see read-only through their `/usr`; neither agents nor users can add one. An agent runs servers only with `policy.permissions.can_use_mcp` on (off by default), and only those listed in `policy.mcp_servers`, by a name of the operator's choosing:

```json
"permissions": { "can_use_mcp": true },
"mcp_servers": {
  "github": {
    "binary": "github-mcp",
    "args": [],
    "env": { "API_TOKEN": "..." },
    "extra_paths_rw": [],
    "extra_paths_ro": [],
    "timeout_seconds": 60,
    "enabled": true
  }
}
```

- `binary` is a file name in that directory, never a path (no `/` or `..`).
- `args` go to the binary as they are, without a shell; `env` is added to its environment. Give secrets in `env`, not `args` (see [Security Model](#security-model)).
- `extra_paths_rw` and `extra_paths_ro` are added to the Landlock allowlist of every process of the agent, its own tools too, while the server is enabled; a path that does not exist is skipped.
- `timeout_seconds` (default 60) limits each tool call; a call that runs longer returns an error to the agent.
- `enabled: false` keeps the entry without running the server.

The runtime starts each enabled server when the agent starts, after its Landlock and seccomp rules are in place, as a child process speaking MCP over stdin and stdout, with `HOME=/nonexistent` unless `env` sets one and, when the agent's network goes through the egress proxy, the proxy variables. It lists the server's tools and registers each as `mcp__<server>__<tool>`, with the server's description and input schema; subagents get them too. Only tools are used, not resources or prompts, and only the text of a result: images, audio and resources in it become a placeholder line. A server that fails to start or to list its tools is left out, with a warning in the agent's log, and the agent starts without it; what servers write to stderr goes to that log too. Hook rules can name a server's tools with a prefix, such as `mcp__github__*` (see [Tool Hooks](#tool-hooks)).

The agent's settings page has an **MCP servers** section: `can_use_mcp`, the servers with an on/off switch and remove, and a form to add one (name, a binary from the ones installed, arguments separated by spaces, read-write and read-only paths). `env` and `timeout_seconds` are set through the API: `GET /agents/{id}/mcp` returns `can_use_mcp`, `servers` and the `available_binaries`, and `PATCH /agents/{id}/mcp` takes `can_use_mcp` and `servers`, which replaces the agent's list as a whole. Both write the agent's policy override, and each change is audited as `agent.mcp.updated`. Changes apply on the agent's next start. In multi-user mode an agent runs only servers its owner's role lists under the same name, with the role's settings, so team admins grant servers in the role's `mcp_servers` (see [Policy](#policy)).

## Skills

A skill is a directory in the agent's workspace, `skills/<name>/SKILL.md`: YAML front matter with a `name` and a `description` (both required, and optionally `version` and `triggers`), then a Markdown body of at most 100 KB.

```markdown
---
name: code-review
description: Reviews a diff against the team's checklist.
triggers: [review, PR, diff]
---

Look at the diff first. Check that every change has a test.
```

When the agent starts, the runtime reads every `skills/*/SKILL.md` and indexes it in the agent's database; a file that does not parse is skipped with a warning. For each message, heartbeat and cron job, the three skills that best match its text (keyword search over name, description and body, joined by vector search when `[embedding]` is configured) are added to the system prompt for that turn only, under "Skills available right now". The matches are counted as uses.

- **`skill_manage`** lets the agent create, update, delete, list, pin and unpin its skills. It writes `skills/<name>/SKILL.md` (names of lowercase letters, digits, `-` and `_`, up to 64) and indexes it at once, so a new skill can match from the next message on; delete removes the directory and its index entry. The agent gets the tool with `permissions.can_install_skills` (on by default); without it, skills in the workspace are still read.
- **Installing a skill by hand**: copy its directory into the workspace's `skills/` (by default `~/.local/share/havn/agents/<agent-id>/workspace/skills/` of the gateway's user, under `workspace_root`), give it the workspace's owner (`chown -R --reference=<workspace> <workspace>/skills/<name>`), and restart the agent (`havn agent stop <agent-id> && havn agent start <agent-id>`). A skill directory removed by hand stays in the index: have the agent remove it with `skill_manage` instead.
- **Curator.** Every 7 days, when the agent has had no message for 2 hours, a curator pass archives skills unused for 90 days and used fewer than 5 times, moving them to `.archive/<name>/` in the workspace, then asks the model for a plan to merge overlapping skills, writes each merged skill and archives the ones it absorbed. Pinned skills are left out of both. Each pass writes a report to `.curator/` in the workspace.
- The agent's **skills** page in the dashboard lists its active and archived skills with their uses and last use, pins and unpins them (`POST /agents/{id}/skills/{name}/pin` and `/unpin`, while the agent runs), and shows the curator's reports.

## Heartbeat

The gateway sends each running agent a heartbeat every `config.heartbeat.interval_seconds` of the agent's config (default 1800, 30 minutes; at least 60), the first one an interval after the agent connects. The agent's settings page sets it in minutes, as does `PATCH /agents/{id}` with `{"config": {"heartbeat": {"interval_seconds": 3600}}}`; the gateway reads it on every check, so a change needs no restart.

On a heartbeat the agent reads `HEARTBEAT.md` in its workspace (the settings page edits it), fresh each time, so an edit applies on the next heartbeat. Without the file, or with an empty one, the heartbeat does nothing and costs no model call. Otherwise the model is asked to save stable new facts about the owner it learned (`memory_remember`), then to follow `HEARTBEAT.md`, and to answer `HEARTBEAT_OK` when neither produced anything to tell.

- It runs with the agent's system prompt, the recent events and the skills that match, and the heartbeat's own earlier runs (its last 40 messages, a thread of its own, not any chat).
- It has the agent's tools except `conversation_search` and those in `context_toolsets.heartbeat.disabled`, up to `tool_rounds.subagent` rounds, and may change memory as the owner. Calls that a hook would ask about are refused, and it cannot change scheduled tasks.
- A reply that ends with `HEARTBEAT_OK` is not delivered. Any other reply is kept in a chat titled Heartbeat in the owner's web chat, and is sent where `config.heartbeat.deliver` says (the settings page sets it too):
  - `"last"`, the default: the owner's conversation with the agent they wrote in last, the web chat or a chat app account they connected (the agent's chat apps section of the settings page). The web chat when they wrote in none.
  - `"web"`: the web chat only.
  - A channel, such as `"telegram"`: the owner's connected account there, through the agent's bot. The web chat when there is none.

  A chat-app account the agent's bot may not send to (an `allowlist` account that has not admitted it) is left out.

  In the web chat it shows in the owner's open tabs, in the Heartbeat chat. In a chat app it is also kept in the chat there, so a reply there follows it. Other people's chats with the agent never get it.

## Webhooks

A webhook is an address other systems (an alert, a CI build, a form, GitHub) call to have an agent act on what they send. The agent's owner makes them on its settings page (**webhooks**; `POST /agents/{id}/webhooks` with `name`, `instructions` and `deliver`), up to 20 an agent: each gets an id and a secret, shown once. It is called at `POST /hooks/<id>` on the gateway's `listen` address, so put that path behind your reverse proxy for callers outside the host; it is not limited to the host's names as the API is. A call holds the secret in one of these ways, looked at in this order (the first present decides), else it is answered 401, as is a webhook that is not there:

- `Authorization: Bearer <secret>` (a proxy that sets its own `Authorization` hides the rest), or `X-Havn-Token: <secret>`;
- `X-Hub-Signature-256: sha256=<hex>` (as GitHub sends it, with the secret as the webhook's secret there), or `X-Havn-Signature-256`: the HMAC-SHA256 of the body under the secret.

The body is text (UTF-8, at most 256 KB, larger is answered 413; the agent gets the first 32,000 characters), whatever its type: JSON, a form, a plain line. A webhook takes 30 calls a minute; more are answered 429. While the gateway restarts, calls are answered 503. A call is answered 202 with its `run_id` once it is checked; the agent, started when it is not running, then acts on it in a fresh context, with the subagent tool-round limit and the `cron` context's tools, but no `schedule`, `set_timezone`, subagents or questions to other agents (what came from outside must not outlive the call, nor run as the owner later), told what its owner asked it to do with a call (`instructions`) and given what the call brought as a JSON string marked as from outside. What a call brings is treated as from someone other than the owner (see "Tools in other people's conversations"): what reading tools give (`memory_search` among them) and calls that run things wait for the owner, who is asked in their own chat with each of the agent's bots and in the web chat, told that "always" there allows it for every later call of that webhook; the owner's saved facts are not given to it, and nothing is saved to memory. A webhook's calls are acted on one at a time, with at most 5 waiting (more are dropped, and its chat says so), and an agent acts on at most 4 webhook calls at once; the rest wait. A call waiting for the owner's approval keeps its place meanwhile (approvals wait at most 10 minutes).

Each webhook has a chat of its own in the owner's web chat ("Webhook: <name>"), which keeps each call (its first 4,000 characters, quoted as from outside) with what the agent made of it, as the agent's message: never as the owner's. That chat is not searched by the owner's `conversation_search`, does not count as where the owner wrote last, is never taken for the owner's by a scheduled task, and its summaries save nothing to memory, also when the owner writes in it. `deliver` also sends the result, as a heartbeat's (see "Heartbeat"): `web` (the default) keeps it to that chat, `last` sends it to the conversation the owner wrote in last, and a channel's name to the owner on that channel. An answer that starts with `[SILENT]` is kept in the chat only, not sent. Making, changing (`PATCH /agents/{id}/webhooks/{hook}`, its `instructions` and `deliver`), calling and deleting a webhook are audited as `webhook.created`, `webhook.updated`, `webhook.called` and `webhook.deleted`. The secret is kept in the gateway's database as given, since a signed call is checked with it; delete the webhook and make another to change it.

## Configuration

Gateway config lives at `~/.config/havn/config.toml`:

```toml
listen = "127.0.0.1:8080"
db_path = "~/.local/share/havn/havn.db"
```

The models agents may use are listed in `[[llm.models]]`, each as `provider:model`, where the provider is `anthropic`, `openai`, `openrouter` or `gemini` and picks the API and the credentials used; the model name is everything after the first `:`. Agents without a `model` of their own use `[llm] default_model`, or the first entry when it is unset, and follow it when it changes. A model that is not listed is refused, so with an empty list agents cannot chat. `havn model add/remove/default` and the dashboard's models page edit this list through the gateway, which writes it back to this file (other keys and comments are kept); with `trust_header` on, only the file can change it:

```toml
[llm]
default_model = "openrouter:deepseek/deepseek-v4.1-flash-1017"   # optional

[[llm.models]]
id = "anthropic:claude-sonnet-4-6"
label = "Sonnet 4.6"            # optional, shown in the dashboard

[[llm.models]]
id = "openrouter:deepseek/deepseek-v4.1-flash-1017"
max_output_tokens = 32000       # optional cap on max_tokens for this model
```

A model's `price` gives what its tokens cost, in US dollars per million tokens, for the usage page: `price = { input = 3.0, output = 15.0, cache_read = 0.3, cache_write = 3.75 }` (cache reads and writes cost what input does when left out). The dashboard's models page sets, changes and clears it too (with `trust_header` on, only the file can). Nothing is fetched: a model without one is shown in tokens only. Costs are worked out with the prices set when the page is looked at, so a changed price changes past costs too. Budgets stay in tokens.

A model is asked to think only when its entry says how, since providers and models take it differently and no list of them is kept: `thinking = { style = "toggle" }` is on or off; `style = "effort"` takes `levels` (`["low", "medium", "high"]`, as the model names them); `style = "budget"` takes `budgets`, numbers of tokens (not for `openai:` models). `default` is a chat's choice when it made none (off without one), and `mandatory = true` leaves off out for a model that always thinks. The gateway asks for it as each API takes it: Anthropic adaptive thinking, with `output_config.effort` for a level (current Claude models take `effort` levels such as `low`, `medium`, `high`, `xhigh`, `max`; a `budget` style is only for Haiku 4.5 and older models, which current ones refuse), shown summarized and without temperature; OpenAI-compatible `reasoning_effort`; OpenRouter `reasoning`; Gemini `thinkingConfig`. Within a reply, the reasoning goes back with each tool call, as Anthropic and DeepSeek want it; the reasoning a model returns is shown folded in the web chat. Off sends nothing to Anthropic and OpenAI-compatible endpoints, so a model that thinks by default keeps doing so: give such a model `mandatory = true` and its lowest levels (`low`, or `minimal`/`none` where its API takes them); likewise for Gemini models that cannot stop thinking (2.5 Pro, 3 Pro), where off is `thinkingBudget = 0`. Set it with `havn model thinking <id> effort:low,medium,high --default medium` (or `toggle`, `budget:2048,8192`, `none`) or on the models page, and try it with `havn model check <id>` or the page's check button, which sends the model a very short question (on the caller's credentials, at most 4096 tokens) and says whether it answered with reasoning, without, or was refused; with `trust_header` on, only listed models are checked, as listed. The web chat's composer picks the model and the thinking of one chat (`/think` in a chat too, for the agent's owner or an account's admins); the thinking menu is greyed out for a model without `thinking`, and a choice the chat's model does not take falls back to its default. Titles, summaries and memory curation are asked not to think.

```toml
[[llm.models]]
id = "openrouter:deepseek/deepseek-v4.1-flash-1017"
thinking = { style = "effort", levels = ["low", "medium", "high"], default = "medium" }

[[llm.models]]
id = "anthropic:claude-sonnet-5-5"
thinking = { style = "effort", levels = ["low", "medium", "high"], default = "medium", mandatory = true }
```

Pictures sent in a chat (PNG, JPEG, GIF, WebP or HEIC in the agent's inbox) are shown to the model of the turn according to its `vision`. A picture over 3.75 MB or 8000 pixels on a side, or a HEIC photo, is shown as a copy the gateway makes when it arrives: at most 2048 pixels on a side, turned as the photo says, in sRGB, a JPEG (a PNG when it is transparent), kept next to the original as `.<file>.shown`. The original stays in the inbox for the agent's tools. When no copy can be made (a broken file, over 67 million pixels, HEIC that is not HEVC-coded, AVIF), the model gets a line saying the image is not shown. With `vision = true` they are always shown, and with `vision = false` never: the model gets a line saying it cannot see the image, and the agent tells the person so. Left unset, they are tried. When a provider refuses a request that shows images outright (a 4xx other than an authentication, payment or rate refusal), the request is sent once more with a note in place of each image; if that is refused too, the agent gets the first refusal. When the second try goes through and the refusal said the model takes no images (as OpenRouter answers 404 "No endpoints found that support image input", or OpenAI "image_url is only supported by certain models"), that model gets the note at once from then on, until its entry changes or the gateway restarts, and the gateway logs a warning suggesting `vision = false`. A refusal for any other reason (a broken image, a context too long) is not remembered. Set it with `havn model add <id> --vision` or `--no-vision`, or on the models page.

On the first start after an upgrade from a gateway without the list, agent and policy model names are rewritten to `provider:model` with the provider older gateways inferred from the name, and if `config.toml` has no `[llm] models` yet, the models existing agents use are added to it (led by `anthropic:claude-opus-4-6`, the model older gateways gave agents without one, if there are such agents). A new install, or an upgraded one without agents, starts with an empty list: add a model with `havn model add` before the first chat.

`max_tokens` on every LLM call is capped by `[llm] max_tokens_ceiling` (default 128000, the largest output limit of a known model; applied on `havn gateway restart`), whatever agents ask for. A running call holds a reservation of up to that many output tokens against the caps it is billed to, so with small caps on a shared credential, lower it to keep one user's large `max_tokens` from tying up everyone else's remaining budget:

```toml
[llm]
max_tokens_ceiling = 16000   # 0 is rejected
```

To send a provider's calls to another endpoint that speaks the same API (an internal LLM gateway, or a mock in tests), set its base URL under `[llm.base_url]`. The provider's credentials are sent there as well, so use `https` for anything not on the gateway host. Only the operator's config can set it, not agents or API users, and it applies on `havn gateway restart`. The keys and their defaults are `anthropic` (`https://api.anthropic.com`), `openai` (`https://api.openai.com/v1`), `openrouter` (`https://openrouter.ai/api/v1`) and `gemini` (`https://generativelanguage.googleapis.com/v1beta`):

```toml
[llm.base_url]
anthropic = "https://llm-gateway.internal.example"
```

### Web search

Agents get a `web_search` tool that the gateway runs for them, so it works for agents without network egress and the search key never enters the sandbox. The tool returns titles, URLs, excerpts and publication dates, and agents are told to cite the URLs as plain links. With the default `provider = "auto"`, each agent owner's searches use the first of these they have a key for:

1. a Brave, Tavily or Exa key, added as a credential under `search:brave`, `search:tavily` or `search:exa` (`havn credential add search:brave <key>`);
2. their OpenRouter LLM key, through OpenRouter's Exa-backed web search (about $0.007 per search on OpenRouter credits), when an `openrouter:` model is listed. The search runs through `[search] openrouter_model`, else the default model if it is an OpenRouter one, else the first listed OpenRouter model;
3. a SearXNG instance at `searxng_url`, which needs no key (enable its `json` format).

If a backend fails (quota, outage, rejected key), the next one is tried. Without any of them the tool is not offered; `havn doctor` and `GET /search` show which backend is used. Each search is recorded in the audit log as `web.search` with its query, and a user's agents may run `per_user_per_minute` searches per minute together. The policy permission `can_search_web` (on by default) removes the tool. Applies on `havn gateway restart`:

```toml
[search]
provider = "auto"            # or off, brave, tavily, exa, openrouter, searxng
searxng_url = "http://127.0.0.1:8888"
max_results = 5              # 1 to 10; the agent can ask for a number
per_user_per_minute = 20     # 0 lifts the limit

[search.base_url]            # optional: brave, tavily, exa
brave = "https://search-proxy.internal.example"
```

### Reading web pages

Agents also get a `web_fetch` tool that the gateway runs for them: the agent gives a URL and what it wants to know, the gateway reads the page and answers from it with a model call, and the agent gets the answer rather than the page. It works for agents without network egress, and a long page costs the agent's context only the answer.

- The gateway upgrades `http` to `https`, refuses URLs with a user name or password, and connects only to public addresses (checked after DNS resolution and on every redirect, with the same ranges as the agent firewall plus the gateway host's own addresses). It follows redirects on the same host; a redirect to another host is returned to the agent, which can read it with another call.
- HTML is converted to Markdown with links made absolute, PDFs to text, and text and JSON are read as they are; legacy charsets such as GBK are decoded. Pages are read up to 10 MB, and the answer covers 100,000 characters of the page at a time; the result tells the agent the offset to read further. Converted pages are cached for 15 minutes, per agent.
- Pages are parsed in a child of the gateway binary, started before its async runtime, with no network (its own network namespace), no new privileges, 1 GB of address space, 20 s of CPU time and no file writes, and killed after 30 s; under a root gateway it runs as `nobody`. A gateway running as another user needs unprivileged user namespaces for the child, and `web_fetch` fails without them; the gateway checks this at start and logs a warning. The child can still read files its user may read, so keep the gateway's files private. The page URL goes to it on stdin, not in its arguments. A page that crashes or stalls the parser fails that call only. Converted text is kept up to 8 MB per page, and the 15-minute cache holds at most 128 MB.
- The answer comes from `[fetch] model` when the agent's `allowed_models` allow it and its owner has a credential for that provider, else from the agent's own model, and is billed like the agent's own calls: to the agent owner's credentials and the agent's token budget. The whole call is limited to 75 s.
- An agent can read only URLs the gateway saw arrive from outside its model: in messages users sent it (WebChat and channels), in `web_search` results, as links on pages it read that way, or as a redirect target, for 24 hours since the gateway started. Its model's own text and other tools' output (a shell command, a file, an MCP server) never add one, so the model cannot put data from the agent into a URL of its choosing. Domains on the policy's `network_policy.allowed_domains` (a domain matches its subdomains) can be read without that, with any path and query; links and redirects on pages read only because their domain is allowed are not recorded, since such a page may echo what the model put in the URL. `blocked_domains` are never read. A refused URL is not echoed back.
- Each call that passes the policy and rate checks is recorded in the audit log as `web.fetch` with the URL, and a user's agents may read `per_user_per_minute` pages per minute together. The policy permission `can_fetch_web` (on by default) removes the tool; upgrading turns it off for roles and agent overrides that had turned off `can_search_web` or `can_access_network`. Applies on `havn gateway restart`:

```toml
[fetch]
model = "openrouter:deepseek/deepseek-v4.1-flash-1017"   # optional, one of [[llm.models]]
per_user_per_minute = 30     # 0 lifts the limit
```

Allowed and blocked domains are set at several levels and only add up: an agent gets every level's entries, and a blocked domain wins over an allowed one.

- The operator blocks domains for every agent with `[network] blocked_domains = ["example.com"]` (applies on restart).
- Team admins set `network_policy.allowed_domains` and `blocked_domains` in a role's policy; they apply to the members' agents. A member in several roles gets the blocked domains of all of them.
- Each user keeps lists for all their own agents (`GET` and `PUT /me/domains`, or the agent settings page), and each agent has its own (`PATCH /agents/{id}/web` with `allowed_domains` and `blocked_domains`). Neither can remove an entry set above it.

An entry is a domain such as `example.com` (covering its subdomains) or `*.example.com`, in ASCII (punycode for international names), with at least one dot; a list holds at most 500. Lists apply to `web_fetch` at once: the gateway resolves them on every call. If a user's own lists cannot be read, that user's agents cannot use `web_fetch` until they can.

### Time zones

Agents are told the current date, time and weekday in each turn, in their owner's time zone, so "today", "tomorrow at 9" and weekdays come out right. A user without a zone gets the gateway host's (`TZ`, `/etc/timezone` or `/etc/localtime`, else UTC). Users set theirs on an agent's settings page or with `PUT /me/timezone` (`{"timezone": "Asia/Shanghai"}`, or `null` for the host's), and an agent can set it with its `set_timezone` tool when the user says where they are. A change reaches the owner's running agents at once.

Agents whose owner's policy has `can_schedule_cron` get a `schedule` tool, so users can ask in chat for reminders and recurring tasks ("remind me at 17:00 to call Anna", "every weekday at 9, summarise the news"). The agent creates, lists, pauses, resumes and deletes its own tasks: once at a time (local in the user's zone, or with an offset) or in some minutes, or on a calendar schedule in words or as a cron expression, at most every 5 minutes, with up to 20 active tasks per agent. A task made in a channel conversation (Telegram) sends its result to that conversation, with the same allowlist checks as the agent's own messages; one made in the dashboard chat shows in its open tabs. If a run fails, its conversation (or the dashboard chat) is told once a day (UTC). Tasks change only in a user's turn, not in a scheduled run or heartbeat, and in a channel conversation the agent lists and changes only that conversation's tasks. A one-shot that ran is shown as done and cannot be resumed; done one-shots are deleted after 7 days. Each change is audited as `agent.schedule`, and the agent settings page lists the tasks with their next and last run, to run now, pause or delete. **run now** (`POST /agents/{id}/cron/{job_id}/run`, audited as `cron.run_now`) runs a task at once, paused or not, without its wake check: a recurring task keeps its schedule, and a one-shot that has not run is done with it (a reminder run now is not run again at its time). Its result goes where a scheduled run's does. It needs the agent running, the owner's policy to allow `can_schedule_cron`, and no run of the task still going (one that gave no result in 15 minutes no longer counts). Each run's output file is named with its time and run id.

Cron jobs (`POST /agents/{id}/cron`) can also run on calendar times in a time zone, with a five-field cron expression or in words: `{"cron": "0 8 * * 1-5"}`, or `{"every": "daily 08:00"}` (also `weekdays`, `weekends`, `weekly mon 10:00`, `monthly 1 09:00`, `hourly :15`). The zone is `timezone` when given, else the owner's at the time the job is created; the job keeps it. Times stay local across daylight-saving changes: a time a change skips runs right after the change, and one it repeats runs once. A job that was due while the gateway was down runs once when it is back; the runs it missed before that are skipped. An agent's `set_timezone` calls are audited as `user.timezone.updated` and limited to 3 per minute per user; subagents and cross-agent queries do not get the tool.

Config edits are picked up live: `allowed_origins`, `[llm] default_model`, `[[llm.models]]` and `[channels]` apply in place, while every other change (`listen`, `db_path`, paths, `[[bindings]]`, ...) is logged as needing a `havn gateway restart` to apply — the gateway never restarts itself. Of `[channels]`, who may reach a bot (`dm_policy`, `allow_from`, `group_policy`, `allow_groups`, `admins`, `bot_username`) applies to its next message; adding or dropping an account, or changing its `adapter_token_ref`, is logged as needing a restart, as an adapter's session was let in by its token (a changed account's policies still apply at once). `[workspace] browse`, `[channel_texts]`, `[transcription]` and `[attachments]` apply in place too. Per-agent resource limits come from the agent's role policy, not a global config block.

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

### Browsing an agent's workspace

An agent's owner can browse the files in its workspace from the dashboard (the agent's **workspace** page): folders, text and images shown in place, any file downloaded. It is read-only, and off unless the gateway allows it:

```toml
[workspace]
browse = true   # default false
```

The dashboard's **settings** page turns it on or off and writes it to config.toml (with `trust_header` on, only the file can); it applies at once. With it on, every agent's page is there unless its owner turns it off on the agent's settings page (`PUT /agents/{id}/workspace/access`); with it off, no agent's is, and an agent's settings cannot turn it on. Only the owner can browse, and not from another site's page. Links in the workspace are listed with where they point and never followed; the agent's database and `.havn` are never listed or sent (by name: a copy the agent makes elsewhere is shown like any file, as the agent can read them anyway); files over 50 MB are not sent; images are shown in place and everything else is sent as a download. Changes are audited as `workspace.browse.changed` and `agent.workspace_browse.changed`.

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

Point the SSO proxy at `dashboard_listen`. Its `/api` is the same API as `listen`, under the same rule: the identity header is read only from an `allowed_proxies` peer, so with the proxy on the gateway host `allowed_proxies = ["127.0.0.1/32"]` covers it. Route the UI, `/api` and `/ws` through the SSO proxy, without unauthenticated paths: a path the proxy skips would pass through whatever header the client sent. The web chat's WebSocket is accepted from the page's own origin (its scheme and `Host`) or from one in `allowed_origins`, so have the proxy pass the `Host` header on (oauth2-proxy does by default, nginx with `proxy_set_header Host $host`), or list the dashboard's public origin there.

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
```

```toml
listen = "127.0.0.1:8080"
dashboard_listen = "127.0.0.1:3000"

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

```toml
listen = "127.0.0.1:8080"
dashboard_listen = "127.0.0.1:3000"

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
| Lark / Feishu | [havn-channel-lark](https://github.com/amplimit/havn-channel-lark) | Available |
| Discord | [havn-channel-discord](https://github.com/amplimit/havn-channel-discord) | Available (gateway 0.2.11 or later) |
| Slack | — | Planned |

The Lark / Feishu adapter takes Lark's events on an HTTP webhook (`im.message.receive_v1`) and answers 1:1 chats only; see its README for setup and the message kinds it carries.

The Discord adapter answers private messages and server channels: a mention of the bot in a channel, or a reply to one of its messages, opens a thread, a conversation of its own, and a server can be let in as a whole or a channel at a time (see "Group chats"). See its README for the bot's setup.

An agent can only send through the channel accounts that `[[bindings]]` binds to it. With an account's `dm_policy = "allowlist"` (the default) it can only message senders in `allow_from` or admitted with an invite code (below); with `dm_policy = "open"` it can message anyone the bot can reach on that platform, so use `open` only for bots meant to be public. With `dm_policy = "allowlist"`, it can message a group only while the group may use the bot (see "Group chats"); with `open`, any chat the bot can reach, groups included.

**Admitted or connected.** These are two different things, and the owner needs the second for their own account:

- *Admitted* (`allow_from`, an invite code, or anyone on an `open` account): the bot answers the sender. Havn takes them for someone other than the agent's owner, whoever they are.
- *Connected* (linked to a Havn account; see "Connecting a chat account"): the bot answers them as that Havn user, and as the agent's owner when they own it.

An owner whose own chat-app account is only admitted is treated as someone else there: the agent's shell, file and memory tools wait for their approval, which they can give only in the web chat (see "Tools in other people's conversations"); nothing they say there changes the agent's memory; the admin commands (`/model`, ...) need `admins`, and even then the calls that wait for the owner (shell, files, memory, MCP) are approved only from the web chat; and those chats are not searched with their others nor listed in the web chat. So connect your own account from the agent's settings page (**chat apps** > **connect**), even when the bot already answers it: open the link shown, or send the bot `/start <code>` (a bare code connects only an account the bot does not answer yet; one it answers passes it on to the agent). It stays admitted as before. The **chat apps** section lists connected accounts only. `havn access list` shows the senders let in by `allow_from` or an invite, with `linked="<name>"` beside those connected (an `open` account's senders are not listed).

An agent answers one message at a time per conversation. A message sent while it is still replying waits and is answered after that reply, in order (up to 20 waiting; channel users are told it was queued). The dashboard chat has a stop button while a reply runs, and in a channel conversation `/stop` (or `/stop@botname`) stops it: the agent ends the model call or tool call in progress and kills the processes a running shell command started. The reply it keeps is the text of the model calls that had finished, followed by a blank line and `(stopped)`, or only `(stopped)` when they wrote none; the text of the model call in progress is dropped. Waiting messages are answered after it.

Each user has several chats with an agent. The dashboard chat lists them next to the conversation, most recent first: "new chat" starts another, and a chat can be renamed or deleted (with its messages). In a channel conversation, `/new` (or `/new@botname`) starts a new chat; the earlier one is kept. Chats keep their own history, queue, stop button and "approve for this conversation" approvals, and a task the agent schedules in a chat reports there and keeps its results in that chat's history; `/stop` in a channel stops the reply running in any of the sender's chats. Deleting a chat stops its reply and drops its waiting messages; the agent's memory is shared by all of them. `conversation_search` finds past messages: for the agent's owner, in every chat they wrote from (the dashboard, and chat-app accounts linked to them); for anyone else, in their own chats only; in a group, in that conversation's chats only (a thread's own, where threads are conversations of their own), for the owner too. The facts the agent saved about its owner (who they are, what they like, what they work on) are in every private chat of the owner's, on any channel, never in a group's; the owner's messages on a linked chat-app account, outside groups, are remembered as the dashboard's are. History from before chats existed became each conversation's first chat.

In the dashboard chat, a message can be edited and a reply regenerated. Either starts a branch: the messages after that point stay stored on their own branch, `‹ 2/3 ›` under a message switches between its versions, and the agent sees only the branch on screen. In a channel conversation, `/retry` (or `/retry@botname`) answers the last message again. A branch does not undo what tools already did: files written, messages sent and memories saved stay.

### Chat titles

A model names each new chat from its first message, while the agent answers it (in a channel too); until then, and if that fails, the chat is named after the start of its first message, and a chat renamed by hand keeps its name. The model that writes titles is `[llm] title_model` in config.toml, set on the dashboard's models page or with `havn model title <provider:model>` (a small, cheap model does well); without one, each agent's own model writes them. A role's policy can name another (`"chat_titles": {"model": "..."}`) or turn titles off (`"chat_titles": {"enabled": false}`; in multi-user mode an agent cannot turn them back on, and a user with several roles has them when any of the roles does), and an agent's settings page turns them off for that agent. A message of files alone keeps their names as its chat's title. Changes apply on the agent's next start; a title model removed meanwhile gives way to the agent's own model.

### Tools in other people's conversations

`SOUL.md` and `USER.md`, the files the agent's settings page edits under "bootstrap files", are in the agent's prompt in every chat: other people's and groups' too, without asking the owner. They are public to anyone who can talk to the bot, so `USER.md` holds only what anyone may know about the owner (a name, how to address them, a language); what should stay private belongs in the agent's memory, used in the owner's own chats only (see below).

The agent's memory, workspace and machine are its owner's. In a conversation with someone else (a person the owner admitted to a chat-app account, anyone on an `open` account), recent events are not put in the prompt, and the tools that reach them wait for the owner:

| Tools | The owner is asked |
|---|---|
| `shell`, `process`, `file_write`, `file_edit`, `view_image`, `skill_manage`, `send_file`, `set_timezone`, MCP tools | **before** the call runs, shown its whole input |
| `memory_search`, `file_read`, `file_find`, `file_search` | **after** it ran, shown everything it gave (exactly what the model would get; past 4000 characters in all, the rest is left out of both, and both are told) |

The person is told the agent checks with its owner. The owner sees who asked, what they wrote and the call in the web chat and, when they linked an account of that chat app, in their own chat with the same bot, and answers there:

- **allow once** / **use**: the call runs, or the model gets what it gave, this once; what a reading tool or a subagent gave is not kept in the reply's steps, so later turns there do not have it;
- **always allow there**: for a reading tool, it answers in that conversation from now on without asking; for one that runs or changes things, only that exact call does (always allowing `shell` in a group would let anyone there run anything);
- **deny**, or no answer in 10 minutes: the model is told it was not allowed and not to say what it would have given, and further calls of that tool in that reply are refused without asking again.

The owner can add what not to say (in the web chat's box, or `/approve <id> not my address` in their chat), which the model gets with the result; with always allow it stays as the rule, given to the model in every later turn there (in the owner's own turns in a group, only with the result). The agent's settings page ("tools in other people's conversations") lists what is always allowed with its rules, to edit or revoke. There the owner can also allow a reading tool (`memory_search`, `file_read`, `file_find`, `file_search`) in every conversation with the agent, with a rule for what not to say: it then answers without asking in every conversation, so `file_read` everywhere lets anyone who can reach the agent read its files without the owner looking. A conversation's own entry for the tool keeps its rule; where that rule says nothing, the one for every conversation is given. The entry is listed, edited and revoked like the others, and audited as `agent.tool_access.allowed`. Tools that run or change things are never allowed everywhere. Only the owner decides, never someone in the conversation it is for; one question waits per conversation at a time, and a reply's calls ask one after another. A rule's `ask` (see "Policy") still asks about every such call, always allowed or not. The audit log records that it was asked and decided, not the call, what it gave or the note. Scheduled tasks made in someone else's conversation, and other agents' questions, have no one to ask, and these tools are refused there; the owner's turns outside groups, heartbeats and the owner's tasks use them as before (for the owner's turns in a group, see "Who is who" under "Group chats").

### Language

An agent's settings page sets the language of what Havn writes for it: English (the default) or 中文 (`config.language`, `"en"` or `"zh"`), set by the agent's owner. What the agent writes changes on its next start; what the gateway answers itself (commands it handles, approval notices, failure notes) changes at once. It covers the parts of the system prompt Havn adds (the platform's description, who and where the conversation is, saved facts, recent events, the summary, the time, saving without being asked), the replies to chat commands (`/help`, `/status`, `/whoami`, `/model`, `/think`, `/new`, `/compact`, `/undo`, `/usage`, `/approve`, ...), approval and queue notices, failure notes (a reply that failed, a message or file that did not reach the agent, a scheduled task that failed), the headings of the skills it is given, and the refusals of the memory tools. Tool names and descriptions stay English, which models follow best; the owner's own files (`SOUL.md`, `USER.md`) stay in the language they are written in; and the texts sent before a sender is paired are set per account in `channel_texts`. The agent answers people in their own language either way. In Chinese the agent's owner is 所有者.

### Subagents and asking other agents

An agent can hand a task to a subagent (`subagent_spawn`): it runs as the same agent, and subagents asked for in one reply run at once. For each, the agent picks a model among those its policy allows (`allowed_models`), how much it thinks (as `/think` takes it, within what that model takes; by default the chat's choice on the chat's model, else the model's own), and whether it sees the conversation so far (`context: fork`, its last 10 messages, the default) or starts with nothing but the task (`isolated`). A subagent shows in the steps of the reply that started it, with the model and thinking it ran with and its own steps, not as a chat of its own. Subagents get the agent's tools except `subagent_spawn`, `agent_query`, `set_timezone` and `schedule` (and those `context_toolsets.subagent.disabled` names), and up to `tool_rounds.subagent` rounds each; at most 8 run at once in an agent. A subagent whose model is unavailable falls back on its own, without the reply's model.

An agent can also put a question to another agent of its owner's (`agent_query`), which answers with its own tools, except those that wait for the owner in someone else's conversation (see "Tools in other people's conversations").

Both are in the agent's settings page ("working with other agents"), as the policy's `permissions.can_spawn_subagents` and `permissions.can_query_other_agents`. On a single-user gateway, subagents are on unless an agent's policy turns them off, and asking other agents is off. In multi-user mode both follow the owner's role, and an agent cannot turn on one its owner's role has off. Turning either on applies on the agent's next start; asking other agents stops at once when turned off.

### Questions with buttons

An agent can ask the person it is talking to to choose among 2 to 10 options with the `ask_choice` tool (a question of at most 300 characters, options of at most 100). The message lists the options numbered. Where they can be shown, a question of one answer gets a button per option and a question of several answers a poll: in the web chat buttons and checkboxes, in Telegram buttons and a native poll, in Discord buttons and a menu that takes several choices. Elsewhere the person replies with the number(s). A press or a vote sends `/choose <question> <numbers>`, which the gateway turns into the person's next message to the agent, in the chat of the question, as `[Chosen with a button, for "<question>": "<option>"]` or `[Chosen in the poll "<question>": "<option>", ...]`. A question is answered once (once its answer reached the agent), only from its own conversation, and for 24 hours, at most 20 waiting per conversation (the oldest goes); a later press says it was already answered or expired. In a chat app the answer goes to the chat in use, which may be a newer one than the question's; the answer names the question. Questions are kept in the gateway's memory, so a restart forgets them. In the web chat a question shows in the chat it was asked in while that chat is on screen, until the page is reloaded; one asked in a chat not on screen is answered by typing. Buttons and polls are only the gateway's: those an agent puts on a message of its own are dropped.

### Voice messages

Voice notes and audio files (Ogg/Opus, MP3, M4A, WAV, WebM, FLAC; at most 20 MB) that reach an agent's inbox, from a chat app or uploaded in the web chat, are transcribed by the gateway when `[transcription]` is set. The transcript is kept next to the file (`<file>.txt`), and the agent gets it with the file, marked as a transcript of user-supplied audio; without `[transcription]`, or when transcription fails, the agent gets the file and a line saying it was not transcribed. A transcript is never taken for a command: commands are typed. The section applies without a restart.

```toml
[transcription]
kind        = "endpoint"                   # an OpenAI-compatible /audio/transcriptions
base_url    = "https://api.openai.com/v1"  # default; Groq, a local whisper.cpp server, ...
model       = "gpt-4o-mini-transcribe"     # default
api_key_env = "OPENAI_API_KEY"             # default: the variable holding the key, in the gateway's environment (/etc/havn/havn.env)

# or: a model of [[llm.models]] that takes audio, asked for a transcript on the
# agent owner's credentials, within the agent's policy and budget
# kind  = "model"
# model = "openrouter:google/gemini-3.8-flash"
```

The key of an endpoint is read from the environment, never from config.toml; no key is sent when the variable is unset or empty (a local server). With `kind = "model"`, use an OpenRouter or Gemini model that takes audio (Anthropic models take none, and OpenAI's own API only WAV and MP3, while Telegram voice notes are Ogg/Opus); the model must be listed and allowed to the agent, and its tokens count against the agent's budget. A transcription that takes longer than 90 seconds is given up; at most four run at once.

### Chat commands

A message that is only a command is answered by the gateway or the agent and never reaches the model, in a channel conversation and in the dashboard chat (where typing `/` lists them). `/name@botname` works too; an unknown command, or a command given words it does not take, gets a short answer saying so.

| Command | Does |
|---------|------|
| `/new` (`/reset`, `/clear`) | starts a new chat; the earlier one is kept |
| `/stop` | stops the reply being written |
| `/retry` | answers the last message again |
| `/undo` | takes back the last message and its reply; they stay stored as a version |
| `/status` | the agent, the chat's model and thinking, its title and length, replies waiting |
| `/model [name\|number\|default]` | lists the models the agent may use, or sets this chat's own; `/new` goes back to the agent's |
| `/think [off\|on\|level\|tokens\|default]` | shows or sets how much this chat's model thinks, within what its `thinking` offers |
| `/agent_model [name\|number\|default]` | sets the agent's model, as its settings page does, for every chat without one of its own |
| `/usage [days]` | the agent's model calls and tokens, per model (30 days by default) |
| `/approve [always] [id]`, `/deny [id]` | answer the step of this conversation waiting for approval: the one named by its request id, else the one that waited longest |
| `/allow <sender>`, `/allow group <id>` | the agent's owner, in their connected chat with the bot, lets in someone, or a group, the bot turned away (see [Who can reach a bot](#who-can-reach-a-bot)) |
| `/disconnect` | in a chat app, ends this account's link to a Havn account (see [Connecting a chat account](#connecting-a-chat-account)) |
| `/whoami` | who the gateway takes you for, and whether you may use the admin commands |
| `/help` (`/commands`) | lists them |

`/model`, `/think`, `/agent_model`, `/usage`, `/approve` and `/deny` spend the agent owner's credentials or let a step through, so only admins may use them: in the dashboard, the agent's owner; in a channel, the agent's owner once they connected that chat account to their Havn account (see [Connecting a chat account](#connecting-a-chat-account)), and the senders listed in the account's `admins` (none by default):

```toml
[[channels.telegram.accounts]]
id = "acme-bot"
allow_from = ["tg:123456789", "tg:987654321"]
admins = ["tg:123456789"]
```

In a chat app whose adapter shows buttons (Telegram, Discord), the message saying a step waits for approval carries **Approve**, **Approve for this chat** and **Deny** when the person in that chat may decide: a press sends `/approve <id>`, `/approve always <id>` or `/deny <id>` for that request, as if typed, with the same checks; the bot takes the buttons off the message once one is pressed. A request decided meanwhile (in the web chat, or by another press) answers that it was already decided. Adapters without buttons show the text, which says how to answer by command.

A Telegram adapter that knows the command list sets it as the bot's command menu (`setMyCommands`) when it connects, so typing `/` in Telegram shows them. The Discord adapter sets them as the bot's slash commands, with a `text` option for those that take something after their name.

### Invite codes

Instead of adding every team member to `allow_from` and restarting the gateway, an operator can hand out one-time invite codes for an `allowlist` account:

```sh
havn invite create --channel telegram --account acme-bot           # expires in 24h
havn invite create --channel telegram --account acme-bot --expires 2h
```

The command prints a code such as `7K3M-QX9P-2HWD-RT4B` once. Send it to the person over a channel you trust (the way you hand out the code is what vouches for them). They message the code to the bot in a private chat, or open `https://t.me/<bot>?start=<code>`, which sends `/start <code>`; case, dashes and spaces don't matter. The gateway then adds their sender id to the account's allowlist, marks the code used, records a `channel_access.granted` audit entry and replies with a short welcome. The code itself is not passed to the agent. Their later messages reach the agent like those of anyone in `allow_from`.

- A code works once, only for the channel and account it was created for, and expires after `--expires` (default 24h, at most 30d). havn stores only its SHA-256 hash.
- A wrong, used, expired or revoked code gets the same answer ("That pairing code is not valid or has expired."), so a sender can't tell which. Each sender gets 5 attempts per account every 15 minutes; after that any code, a valid one too, is answered "Too many tries" until the 15 minutes are over. Failed attempts are logged.
- On `dm_policy = "open"` accounts everyone is already admitted, so codes are passed to the agent as ordinary messages.
- An invite code admits a sender; it does not connect them to a Havn account. For the operator's or an agent owner's own chat-app account, use **chat apps** > **connect** instead (see "Connecting a chat account"), or as well, if it is already admitted.

A sender who is not paired yet is answered and never reaches the agent: `/start` gets a welcome that asks for the pairing code, `/help` explains how to pair, and any other message "Not paired yet. Send your pairing code, or /help." Every message is answered, at most 3 a minute per sender (and to at most 10,000 senders at once per account), so a stranger cannot make the bot send more. Commands are read in any case, with or without `@botname`. The texts can be set per account, for a bot that speaks another language; a text left out or blank keeps the default, and changes apply without a restart:

```toml
[channel_texts.telegram.acme-bot]
welcome      = "你好！使用前请先配对：发送你拿到的配对码。"
unpaired     = "还没有配对。请发送配对码，或发送 /help。"
invalid_code = "配对码无效或已过期。"
help         = "这个机器人只回复已配对的人。请发送管理员给你的配对码。"
paired       = "配对成功，现在可以聊天了。"
too_many_tries = "尝试次数太多，请几分钟后再发送配对码。"
```

A text is cut to 2000 characters, and the gateway warns about texts for an account it does not declare.

The agent's owner can do the same from its settings page (see [Who can reach a bot](#who-can-reach-a-bot)). The operator manages invites and admitted senders with the same direct-database commands as `havn user` (no gateway restart needed):

```sh
havn invite list [--channel telegram] [--account acme-bot]   # status of each invite; codes are never shown again
havn invite revoke <invite-id>                               # an unused invite
havn access list [--channel telegram] [--account acme-bot]   # allow_from entries and invited senders
havn access revoke --channel telegram --account acme-bot tg:123456789
havn access revoke --all --account acme-bot [--channel telegram]   # every invited sender of the account
```

`access revoke` applies to the sender's next message, and the agent can no longer message them either. Entries in `allow_from` can only be removed from the config file; `access revoke` warns when a sender it removed is still listed there.

Invited senders are stored per account and outlive config edits. If an account is removed or switched to `open`, its grants stop applying but are kept, and apply again if the account comes back as `allowlist`. The gateway warns about such grants at start-up and `access list` marks them; clear them with `havn access revoke --all`.

### Group chats

A bot can be used in the groups of its platform (Telegram groups and supergroups, not broadcast channels; Discord servers' channels and their threads), set per account:

```toml
[[channels.telegram.accounts]]
id = "acme-bot"
group_policy = "allowlist"           # "off" (the default), "allowlist" or "open"
allow_groups = ["tg:-1001234567890"] # with "allowlist"
```

- `off`: group messages are not used. A group that addresses the bot is told the bot is not used in groups yet, is listed for the owner as turned away, and the owner is told in their connected chat that groups are off for the bot and where to turn them on (no button, as letting the group in would do nothing yet).
- `allowlist`: the groups in `allow_groups`, and those the agent's owner lets in (see "Who can reach a bot"), may use it. A group not let in that addresses the bot is told it is not let in yet, is listed for the owner as turned away, and the owner is told in their connected chat with an **Allow the group** button (`/allow group <id>`). Talk in such a group that does not address the bot is not kept.
- `open`: every group the bot is added to. The agent can send to a group it was used in while the account is `open`.

A group let in lets its members use the bot there, and only there: it does not admit them to private chats.

**One conversation per group.** Everyone in a group shares one conversation with its own chats (`/new` starts another for all of them; `/retry` works in private chats only, since the last message may be someone else's). The reply goes to the group as a reply to the message it answers. Each message the agent gets says who wrote it: the name the platform shows (in quotes) and their sender id, and "your owner" when it is the agent's owner's connected account. Lines that members write that read like one of these labels are quoted.

**Servers and threads.** A platform whose groups are part of a larger one (Discord: a channel of a server) says so, and the server can be let in as a whole: the owner's notice of a channel turned away offers **Allow the whole server** and **Allow this channel**, and `allow_groups` and `/allow group` take either id. A channel may use the bot while it or its server is let in; letting in a server lets in all of it, channels made later included, for everyone in it. A thread in a channel is a conversation of its own, with its own chats, and replies go to it; the answer to a channel not let in goes to the channel, unless the thread was let in before. The agent may send only to channels and threads it has seen, while they or their server may use the bot; a server is not a place to send to (the gateway refuses one it knows, and an adapter refuses any). The agent is told the server, channel and thread a message was written in.

**When it answers.** The agent answers a message that addresses the bot: one that names it (`@acme_bot`), replies to one of its messages, or is a command. Other messages are not answered. The gateway keeps the last 30 of them per group (500 characters each, in memory only) and gives them to the agent with the next message that addresses it, as what was said since it was last addressed.

**Telegram's privacy mode.** It is on by default, and it is fixed for each group when the bot is added to it. While it is, Telegram gives the bot in a group only service messages (members joining and leaving, pins, a new title), commands meant for it (`/help@acme_bot`, or `/help` when it was the last bot to write there), replies to its messages and to messages meant for it, and messages sent via it: a mention that is not a reply never reaches it. Turn it off with BotFather (`/setprivacy`, Disable), then remove the bot from each group and add it again, as only that applies the change to a group; a group already let in stays let in. A bot that is an administrator of the group sees every message either way. The adapter says, for each group, whether the bot sees every message there, where it knows: from the mode the bot had when it was added, from its being an administrator there, or from a message only such a bot gets. Where it does not, or the gateway does not know (as after a restart, until the group writes again), the group's welcome asks members to reply to the bot or send it a command such as `/help@acme_bot`, and names a mention only where it is known to reach the bot. Where it is known not to, the owner who lets the group in is told how to turn privacy mode off, and the agent's settings page marks the group among those let in (not those in `allow_groups`, which it lists as ids).

**Who is who.** Whether a message is the owner's is decided per message. What the agent says in a group the whole group reads, and other members' lines come into the owner's turn too, so the owner's connected account there is not treated as in a private chat. Their turn is given none of what memory holds: no saved facts, no recent events, and no prompt to save what they say unasked; what they ask the agent to save is saved. The tools in the table under "Tools in other people's conversations" wait for them as for anyone there, asked in their own chat with the same bot and in the web chat, and the request says they asked in the group, where always allowing it allows it for everyone there (for a tool that runs things, that exact call only); what they ask the agent to save or forget is done without asking. What the reading tools and subagents gave is not kept in a reply's steps for later turns, and the group's summaries never save facts to memory. Everyone else is someone other than the owner (see "Tools in other people's conversations"): their calls wait for the owner, who is asked in their own chat with the same bot and in the web chat, never in the group; the group is told the agent is checking with its owner, and the owner is shown what that member wrote. "Always allow there" applies to the group. In the group itself, `/approve` and `/deny` need the step's id, so the owner decides only a step they saw. A group's chats are not the owner's own: they are not searched with the owner's chats, and a task scheduled there is not the owner's.

### Who can reach a bot

An agent's settings page manages its bots without the CLI (**who can reach the bot**, owner only; `GET /agents/{id}/channel-access`). For each account bound to the agent, it sets who may write to it in private chats (`dm_policy`: those let in, or anyone) and in groups (`group_policy`: off, those let in, or any group it is added to). A change is written to the account's `[[channels.<channel>.accounts]]` table in config.toml, keeping the rest of the file (comments included), and applies at once, without a restart (`PUT .../channel-access/policy`, audited as `channel_account.policy.changed` with what it was). What it writes must load: if config.toml holds the account otherwise (an inline table), or has a hand edit the gateway could not load, the file is left as it is, nothing changes, and the page says why. Opening a bot to anyone asks first. With `trust_header` on, only config.toml sets them. Turning groups on lets the owner be told again of the groups turned away while they were off; a group cannot be let in while they are off. It lists:

- the senders in `allow_from` (changed in config.toml only);
- the senders let in by an invite or connect code, or by the owner, and whether each is connected to a Havn account (the owner's own is marked). **remove** takes their access back from their next message (`DELETE .../channel-access/senders`); the owner's own connected account stays let in while it is connected.
- the invite codes made for it. **make an invite code** (1 hour to 30 days) shows a code once, with a `t.me` link when the bot's username is known; an unused one can be revoked. Connect codes are their users' own and are not listed.
- the groups it may be used in: those in `allow_groups`, those let in by the owner (**remove**), and with `open` those it was used in (while groups are `off`, none is used, but those let in stay for when they are on);
- the senders and groups it turned away recently: who, about how many times (counted as often as they are answered, at most 3 a minute), when last, and the start of what they last said, with anything that reads as a pairing code left out. **let in** admits them (`POST .../channel-access/senders`), and **dismiss** forgets them. Each account keeps the 100 who wrote last, for 30 days. Someone let in otherwise (`allow_from`, a code, the owner's connected account) who sends a code that does not work is not listed.

A sender can also be let in by id.

When someone the bot turns away writes, and the agent's owner connected their own account on that channel, the owner is told in their chat with the same bot (Telegram delivers it only if they have started that bot): who wrote and what, as plain text, with an **Allow** button that sends `/allow <sender>`. They are told once a day per sender, and about at most 5 senders an hour per account. The setting on the same page turns this off (`config.notify_unpaired = false`). Only the agent's owner can use `/allow`; it lets the sender in to the bot the owner sends it to. It lets in only an id that bot has seen, turned away or let in: a group's or a server's as a group, with or without the word `group`, and a sender's as a sender (`/allow group` with a sender's id is refused). An id it has not seen is answered so, and nothing is let in; the turned away are kept for 30 days after they last wrote (the last 100 per bot), so for an older notice, have them write to the bot again.

A sender let in is told they can talk now. Letting in, removing and making or revoking a code are audited as `channel_access.granted` (with `"by": "owner"`), `channel_access.revoked`, `channel_invite.created` and `channel_invite.revoked`.

### Connecting a chat account

A Havn user can connect their own Telegram account (or another chat app's) to their Havn account, without the operator. An agent's owner should connect the account they talk to its bot from: until then, the bot answers it as someone else's (see "Admitted or connected"). On an agent's settings page, **chat apps** > **connect** makes a pairing code for each channel account the agent is bound to; it works once, for 10 minutes. The person sends it to the bot, or opens the link shown beside it, which sends `/start <code>`. The gateway then admits the sender as an invite does, links the sender to the Havn user who made the code, and replies "Connected to Havn account <name>."

- A linked sender who owns the agent may use the admin commands (`/model`, `/approve`, ...) without being listed in `admins`; `admins` stays for people without a Havn account.
- A linked sender may write to the bots of every agent of the same user without another code.
- A sender is one user's, and a user has one sender per channel: linking again replaces the earlier link, and the earlier sender loses what the user's codes gave it. A code for a sender linked to another user is refused ("already connected to another Havn account"): the person sends `/disconnect` to the bot first, which ends the link whoever it is to (`havn access unlink --channel telegram tg:123` does the same for the operator). A sender who can already write to the bot links with the link (`/start <code>`) too; a plain invite code sent that way is left for its person.
- Links are listed, and removed, under **chat apps** on any agent's settings page (`GET` and `DELETE /me/channel-links`). Disconnecting takes back the access the user's codes gave the sender; an admission they had otherwise (`allow_from`, an invite) stays.
- An account bound to several agents (`[[bindings]]`) sends its messages to the first; only that agent's owner can make codes for it.
- Links are per channel, so an adapter's sender ids must be the same person on every account of the channel, as Telegram's user ids are. `havn access list` shows who each sender is linked to. Creating a code and linking and unlinking are audited as `channel_link.code_created`, `channel_link.created` and `channel_link.removed`.
- Codes made with `havn invite create` keep working as before: they admit a sender and link them to no one.

**Chat-app chats in the web chat.** Once connected, a user's chats with an agent in that chat app are listed in the agent's web chat too, under **in chat apps**, marked with the app (`GET /agents/{id}/channel-chats`); only their own, never other people's chats with the agent. One opens read-only: its messages, and the versions `/retry` and `/undo` left, which can be switched as in a web chat. In place of the message box a line says the chat lives in the app and continues there, with **continue on the web** (`POST /agents/{id}/chats/{chat}/move`). That moves it for good: it becomes one of the user's web chats, the app's next message starts a new chat if it was the app's current one, scheduled tasks made in it deliver to the web chat from then on, and the bot says in the app that the chat moved. It is refused while a reply runs or waits in the chat, and happens in order with the app's messages, so none lands in the chat once it moved. The bot names an earlier chat that moved, so it is not taken for the current one. Moves are audited as `chat.moved_to_web`. The files an agent sent are listed and downloaded only for the user's own chats, web or linked, not the ones it sent other people in a chat app.

The link needs the bot's username on the account; without it the page shows only the code:

```toml
[[channels.telegram.accounts]]
id           = "acme-bot"
bot_username = "acme_bot"    # the Telegram bot's username, for https://t.me/acme_bot?start=<code>
```

The reply to a link can be set like the other texts, with `{name}` for the account's name: `linked = "已连接到 Havn 账号 {name}。"` under `[channel_texts.<channel>.<account>]`.

## Capability Packs

Some agent workloads need capabilities havn doesn't build into core (browser, code interpreter, vendor CLI). Havn provides generic primitives — bind mounts, tmpfs mounts, seccomp allowances — and external capability packs wire them together. Packs live in separate repositories and install under `/opt/havn/<pack>/`.

| Pack | Repository | Description |
|------|-----------|-------------|
| Browser | [havn-pack-browser](https://github.com/amplimit/havn-pack-browser) | Playwright CLI + Chromium for web browsing |

## License

Copyright 2025-2026 Amplimit

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
