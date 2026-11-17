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

The new gateway and agents migrate their databases when they start, and there is no way back, so keep the backup until the new version works. Upgrade the dashboard separately: check out the new tag in its directory, then run `npm install` and `npm run build`, and restart it.

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

The dashboard is not part of the release binaries. Run it on the gateway
host with Node.js 22.13+, from your source checkout or a fresh clone:

```bash
# Clone the tag matching the release you installed, e.g. v0.2.4
# (`havn --version` shows it); skip if you built from source:
git clone --branch v0.2.4 https://github.com/amplimit/havn
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
- Token caps enforced gateway-side: per credential (`max_tokens_per_day`), per user on a shared credential (`per_user.max_tokens_per_day`, `per_user.max_requests_per_minute`) and per agent across all credentials (policy `budget.max_tokens_per_day`, 1,000,000 for the seeded member role; `on_exhaust = "warn_only"` only logs, other values refuse the call). Each call reserves its worst case (estimated input plus `max_tokens`) before it is sent and is settled to the provider's reported usage afterwards, so parallel requests cannot overspend a cap; near a cap, `max_tokens` is lowered to what remains. Usage counts prompt-cache reads and writes and reasoning tokens; a response without usage keeps its reservation as the recorded usage. `max_tokens` is also clamped to the model's `max_output_tokens` in `[[llm.models]]` (or, without one, its known output limit for Anthropic, OpenAI and Gemini models) and to `[llm] max_tokens_ceiling`
- Agents can only use the models in `[[llm.models]]`. A policy's `allowed_models` narrows that further; entries are `provider:model` ids, or `*` for any listed model. For agents without network egress, the gateway also refuses LLM requests that would make the provider fetch a URL (URL-sourced images and documents, provider-side tools, models with built-in web search under common names); restrict `allowed_models` to be sure an agent cannot pick a model with its own web access
- WebChat WebSocket connections are checked against an Origin allowlist (`allowed_origins`) and must present the caller's `ws_token` from `GET /me`; the token identifies the user rather than being a secret, so the Origin check is what blocks cross-site connections
- No marketplace — skills are local files; sharing via git

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

The agent settings page keeps a list of tools whose every call needs approval (`GET` and `PUT /agents/{id}/approvals`). Requests and decisions are audited as `tool.approval.requested` and `tool.approval.decided`, with who decided. Like `deny`, `ask` guards against model mistakes on matching inputs; the sandbox and permissions stay the enforcement.
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

A model is asked to think only when its entry says how, since providers and models take it differently and no list of them is kept: `thinking = { style = "toggle" }` is on or off; `style = "effort"` takes `levels` (`["low", "medium", "high"]`, as the model names them); `style = "budget"` takes `budgets`, numbers of tokens (not for `openai:` models). `default` is a chat's choice when it made none (off without one), and `mandatory = true` leaves off out for a model that always thinks. The gateway asks for it as each API takes it: Anthropic adaptive thinking, with `output_config.effort` for a level (current Claude models take `effort` levels such as `low`, `medium`, `high`, `xhigh`, `max`; a `budget` style is only for Haiku 4.5 and older models, which current ones refuse), shown summarized and without temperature; OpenAI-compatible `reasoning_effort`; OpenRouter `reasoning`; Gemini `thinkingConfig`. Within a reply, the reasoning goes back with each tool call, as Anthropic and DeepSeek want it; the reasoning a model returns is shown folded in the web chat. Off sends nothing to Anthropic and OpenAI-compatible endpoints, so a model that thinks by default keeps doing so: give such a model `mandatory = true` and its lowest levels (`low`, or `minimal`/`none` where its API takes them); likewise for Gemini models that cannot stop thinking (2.5 Pro, 3 Pro), where off is `thinkingBudget = 0`. Set it with `havn model thinking <id> effort:low,medium,high --default medium` (or `toggle`, `budget:2048,8192`, `none`) or on the models page, and try it with `havn model check <id>` or the page's check button, which sends the model a very short question (on the caller's credentials, at most 4096 tokens) and says whether it answered with reasoning, without, or was refused; with `trust_header` on, only listed models are checked, as listed. The web chat's composer picks the model and the thinking of one chat (`/think` in a chat too, for the agent's owner or an account's admins); the thinking menu is greyed out for a model without `thinking`, and a choice the chat's model does not take falls back to its default. Titles, summaries and memory curation are asked not to think.

```toml
[[llm.models]]
id = "openrouter:deepseek/deepseek-v4.1-flash-1017"
thinking = { style = "effort", levels = ["low", "medium", "high"], default = "medium" }

[[llm.models]]
id = "anthropic:claude-sonnet-5-5"
thinking = { style = "effort", levels = ["low", "medium", "high"], default = "medium", mandatory = true }
```

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

Agents whose owner's policy has `can_schedule_cron` get a `schedule` tool, so users can ask in chat for reminders and recurring tasks ("remind me at 17:00 to call Anna", "every weekday at 9, summarise the news"). The agent creates, lists, pauses, resumes and deletes its own tasks: once at a time (local in the user's zone, or with an offset) or in some minutes, or on a calendar schedule in words or as a cron expression, at most every 5 minutes, with up to 20 active tasks per agent. A task made in a channel conversation (Telegram) sends its result to that conversation, with the same allowlist checks as the agent's own messages; one made in the dashboard chat shows in its open tabs. If a run fails, its conversation (or the dashboard chat) is told once a day (UTC). Tasks change only in a user's turn, not in a scheduled run or heartbeat, and in a channel conversation the agent lists and changes only that conversation's tasks. A one-shot that ran is shown as done and cannot be resumed; done one-shots are deleted after 7 days. Each change is audited as `agent.schedule`, and the agent settings page lists the tasks with their next and last run, to pause or delete.

Cron jobs (`POST /agents/{id}/cron`) can also run on calendar times in a time zone, with a five-field cron expression or in words: `{"cron": "0 8 * * 1-5"}`, or `{"every": "daily 08:00"}` (also `weekdays`, `weekends`, `weekly mon 10:00`, `monthly 1 09:00`, `hourly :15`). The zone is `timezone` when given, else the owner's at the time the job is created; the job keeps it. Times stay local across daylight-saving changes: a time a change skips runs right after the change, and one it repeats runs once. A job that was due while the gateway was down runs once when it is back; the runs it missed before that are skipped. An agent's `set_timezone` calls are audited as `user.timezone.updated` and limited to 3 per minute per user; subagents and cross-agent queries do not get the tool.

Config edits are picked up live: `allowed_origins`, `[llm] default_model` and `[[llm.models]]` apply in place, while every other change (`listen`, `db_path`, paths, `[[bindings]]`, `[[channels]]`, ...) is logged as needing a `havn gateway restart` to apply — the gateway never restarts itself. Per-agent resource limits come from the agent's role policy, not a global config block.

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

An agent can only send through the channel accounts that `[[bindings]]` binds to it. With an account's `dm_policy = "allowlist"` (the default) it can only message senders in `allow_from` or admitted with an invite code (below); with `dm_policy = "open"` it can message anyone the bot can reach on that platform, so use `open` only for bots meant to be public.

An agent answers one message at a time per conversation. A message sent while it is still replying waits and is answered after that reply, in order (up to 20 waiting; channel users are told it was queued). The dashboard chat has a stop button while a reply runs, and in a channel conversation `/stop` (or `/stop@botname`) stops it: the agent ends the model call or tool call in progress, kills the processes a running shell command started, and keeps what it had said so far, marked `(stopped)`. Waiting messages are answered after it.

Each user has several chats with an agent. The dashboard chat lists them next to the conversation, most recent first: "new chat" starts another, and a chat can be renamed or deleted (with its messages). In a channel conversation, `/new` (or `/new@botname`) starts a new chat; the earlier one is kept. Chats keep their own history, queue, stop button and "approve for this conversation" approvals, and a task the agent schedules in a chat reports there and keeps its results in that chat's history; `/stop` in a channel stops the reply running in any of the sender's chats. Deleting a chat stops its reply and drops its waiting messages; the agent's memory is shared by all of them, and `memory_search` finds past messages of every chat. History from before chats existed became each conversation's first chat.

In the dashboard chat, a message can be edited and a reply regenerated. Either starts a branch: the messages after that point stay stored on their own branch, `‹ 2/3 ›` under a message switches between its versions, and the agent sees only the branch on screen. In a channel conversation, `/retry` (or `/retry@botname`) answers the last message again. A branch does not undo what tools already did: files written, messages sent and memories saved stay.

### Chat titles

A model names each new chat from its first message, while the agent answers it (in a channel too); until then, and if that fails, the chat is named after the start of its first message, and a chat renamed by hand keeps its name. The model that writes titles is `[llm] title_model` in config.toml, set on the dashboard's models page or with `havn model title <provider:model>` (a small, cheap model does well); without one, each agent's own model writes them. A role's policy can name another (`"chat_titles": {"model": "..."}`) or turn titles off (`"chat_titles": {"enabled": false}`; in multi-user mode an agent cannot turn them back on, and a user with several roles has them when any of the roles does), and an agent's settings page turns them off for that agent. A message of files alone keeps their names as its chat's title. Changes apply on the agent's next start; a title model removed meanwhile gives way to the agent's own model.

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
| `/approve [always]`, `/deny` | answer the step of this conversation waiting for approval |
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

A Telegram adapter that knows the command list sets it as the bot's command menu (`setMyCommands`) when it connects, so typing `/` in Telegram shows them.

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

Manage invites and admitted senders with the same direct-database commands as `havn user` (no gateway restart needed):

```sh
havn invite list [--channel telegram] [--account acme-bot]   # status of each invite; codes are never shown again
havn invite revoke <invite-id>                               # an unused invite
havn access list [--channel telegram] [--account acme-bot]   # allow_from entries and invited senders
havn access revoke --channel telegram --account acme-bot tg:123456789
havn access revoke --all --account acme-bot [--channel telegram]   # every invited sender of the account
```

`access revoke` applies to the sender's next message, and the agent can no longer message them either. Entries in `allow_from` can only be removed from the config file; `access revoke` warns when a sender it removed is still listed there.

Invited senders are stored per account and outlive config edits. If an account is removed or switched to `open`, its grants stop applying but are kept, and apply again if the account comes back as `allowlist`. The gateway warns about such grants at start-up and `access list` marks them; clear them with `havn access revoke --all`.

### Connecting a chat account

A Havn user can connect their own Telegram account (or another chat app's) to their Havn account, without the operator. On an agent's settings page, **chat apps** > **connect** makes a pairing code for each channel account the agent is bound to; it works once, for 10 minutes. The person sends it to the bot, or opens the link shown beside it, which sends `/start <code>`. The gateway then admits the sender as an invite does, links the sender to the Havn user who made the code, and replies "Connected to Havn account <name>."

- A linked sender who owns the agent may use the admin commands (`/model`, `/approve`, ...) without being listed in `admins`; `admins` stays for people without a Havn account.
- A linked sender may write to the bots of every agent of the same user without another code.
- A sender is one user's, and a user has one sender per channel: linking again replaces the earlier link, and the earlier sender loses what the user's codes gave it. A code for a sender linked to another user is refused ("already connected to another Havn account"): the person sends `/disconnect` to the bot first, which ends the link whoever it is to (`havn access unlink --channel telegram tg:123` does the same for the operator). A sender who can already write to the bot links with the link (`/start <code>`) too; a plain invite code sent that way is left for its person.
- Links are listed, and removed, under **chat apps** on any agent's settings page (`GET` and `DELETE /me/channel-links`). Disconnecting takes back the access the user's codes gave the sender; an admission they had otherwise (`allow_from`, an invite) stays.
- An account bound to several agents (`[[bindings]]`) sends its messages to the first; only that agent's owner can make codes for it.
- Links are per channel, so an adapter's sender ids must be the same person on every account of the channel, as Telegram's user ids are. `havn access list` shows who each sender is linked to. Creating a code and linking and unlinking are audited as `channel_link.code_created`, `channel_link.created` and `channel_link.removed`.
- Codes made with `havn invite create` keep working as before: they admit a sender and link them to no one.

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
