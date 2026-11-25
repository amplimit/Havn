# Havn

[![Release](https://img.shields.io/github/v/release/amplimit/Havn?sort=semver)](https://github.com/amplimit/Havn/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.94%2B-orange.svg?logo=rust)](../rust-toolchain.toml)
[![Platform](https://img.shields.io/badge/platform-linux-lightgrey.svg?logo=linux&logoColor=white)](#key-features)
[![Ask DeepWiki](https://img.shields.io/badge/Ask-DeepWiki-blue.svg)](https://deepwiki.com/amplimit/Havn)

[English](../README.md) | **简体中文**

> 本文是 [README.md](../README.md) 的简体中文翻译，如有出入以英文版为准。

用于运行自主 AI Agent 的开放基础设施。单一二进制、单一 SQLite 文件、Linux 原生进程隔离。

havn 是一个自托管服务器，运行一个网关和每个用户各自的 Agent。身份认证、网络暴露、加密密钥以及外部聊天平台集成，都由运维者自己的技术栈提供。havn 专注于别人做得不够好的部分：内核级 Agent 隔离、类型化记忆、技能整理、凭据代理，以及易用的单一二进制部署。

<a id="key-features"></a>
## 核心特性

- **Linux 原生进程隔离** — 每个 Agent 使用 namespace + Landlock LSM + seccomp-bpf + cgroup v2。无需 Docker 守护进程，无需为每个用户准备虚拟机，开销几乎为零。
- **单一网关，多个 Agent** — 一个进程负责消息路由、渠道适配器、凭据解析、LLM 代理以及每个 Agent 的生命周期。
- **由网关代理的 LLM 调用** — Agent 进程永远看不到 API 密钥。
- **带审计记录的类型化记忆** — 事实按类别（identity / preference / project / event）分类，每条可为私有或公开，按召回新鲜度老化，并通过 supersedes 链保留完整历史。
- **技能整理** — Agent 从成功的工作流中创建技能；空闲时触发的整理器会合并技能库并使其老化。
- **渠道适配器协议** — 为外部适配器守护进程（Telegram、Lark、Discord 等）提供稳定的 HTTP + WebSocket API，核心中不含任何平台专用代码。
- **策略即数据** — 角色、配额和约束都是 JSON，在构建注册表时以及每次工具调度时强制执行。
- **单一二进制，单一目录** — 一个 `havn` CLI、一个 SQLite 网关数据库、每个 Agent 一个 SQLite。不需要 Postgres，不需要 Redis。

<a id="architecture"></a>
## 架构

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

<a id="quick-start"></a>
## 快速开始

**环境要求：**

- **操作系统** — 仅支持 Linux，推荐 Ubuntu 24.04 LTS。Agent 隔离依赖内核特性（user namespaces、Landlock ≥ 5.13、seccomp、cgroup v2），macOS/Windows 上没有对应机制。
- **网络工具** — 网关主机上需要 `iptables`（nft 或 legacy 后端）、`ip`（iproute2）和 `nsenter`（util-linux），用于为 Agent 提供出站网络。缺少 `iptables` 或 `nsenter` 时网关仍可启动，但所有 Agent 都没有网络访问；缺少 `ip` 时网关拒绝启动（它要检查 Agent 地址池的主机路由）。`install.sh` 会在缺失时通过 apt 安装它们；`havn doctor` 会检查它们。精简镜像和容器镜像（例如 Docker `ubuntu:24.04`）通常没有 `iptables` 和 `iproute2`。
- **Rust** — 1.94+，仅从源码构建时需要（另需 C 编译器和 libc 头文件，例如 `build-essential`）。安装脚本默认下载预构建的静态二进制，因此无需工具链。
- **Node.js** — 22.13+（仅 Dashboard 需要）

<a id="install-prebuilt--no-toolchain-no-compile"></a>
### 安装（预构建——无需工具链，无需编译）

```bash
curl -fsSL https://raw.githubusercontent.com/amplimit/havn/main/install.sh | bash
```

该脚本从 [Releases](https://github.com/amplimit/havn/releases) 获取静态链接的 `havn` 二进制（经 SHA-256 校验），创建 `havn-wakecheck` 沙箱用户，并运行 `havn setup`。用 `HAVN_VERSION=v0.1.1` 固定版本，或用 `HAVN_FROM_SOURCE=1` 强制从源码构建。

然后进行配置并启动。Agent 隔离（namespaces + cgroup v2）要求网关以 root 运行，可以在 root shell 中运行，也可以通过 [systemd unit](#run-under-systemd) 运行；否则网关会拒绝启动，而不是在无隔离的情况下运行 Agent。`havn doctor` 会报告缺少什么（cgroup slice、网络工具、Landlock）。

```bash
sudo -i                      # 网关以 root 运行
havn setup --non-interactive # 仅一次，若 install.sh 以其他用户身份运行了 setup

# 设置凭据的加密密钥（任意自选的口令）
export HAVN_AGE_KEY="your-secret-passphrase"

# 启动网关并保持运行
havn start

# 在另一个 shell 中：添加你的 LLM 凭据（静态加密存储）。
# `credential add` 会与正在运行的网关通信。
havn credential add anthropic sk-ant-...

# Agent 可使用的模型，格式为 provider:model；第一个添加的即为
# 默认模型。使用 OpenRouter 密钥时：havn model add openrouter:<vendor>/<model>
havn model add anthropic:claude-sonnet-4-6
```

在无需隔离的开发机上，`HAVN_SPAWNER=subprocess havn start` 会把 Agent 作为普通的无沙箱进程运行，此时网关无法分辨连接来自哪个 Agent；不要用它运行任何不可信的东西。

<a id="run-under-systemd"></a>
### 在 systemd 下运行

在运行 systemd 的主机上，`install.sh` 会在 `/usr/local/lib/systemd/system/havn.service` 安装一个参考 unit（源文件：[contrib/systemd/havn.service](../contrib/systemd/havn.service)，发布包中也有）。它默认不启用。它以 root 身份、`HOME=/root` 运行网关，因此使用 `/root` 下的配置和数据（install.sh 安装该 unit 时会为 root 运行 `havn setup`），从 `/etc/havn/havn.env` 读取 `HAVN_AGE_KEY`，并在失败时重启：

```bash
sudo -i
HOME=/root havn setup --non-interactive         # 仅一次；install.sh 已经做过
install -d -m 700 -o root -g root /etc/havn
install -m 600 -o root -g root /dev/null /etc/havn/havn.env
$EDITOR /etc/havn/havn.env                      # 添加：HAVN_AGE_KEY='your-secret-passphrase'
systemctl enable --now havn
journalctl -u havn -f
```

每个数据库和 Agent socket 只运行一个网关，每台主机只运行一个隔离 Agent 的网关（使用 `HAVN_SPAWNER=subprocess` 时只适用前一条）。第二个网关（例如在 unit 运行时执行 `havn start`）不会启动，而是报错退出，并给出正在运行的那个网关的 pid；锁文件为 `<db_path>.lock`、`<agent_socket_path>.lock` 和 `/run/havn-gateway.lock`。v0.1.3 及更早版本的网关不持有这些锁，因此升级后，在启动任何其他网关之前先重启该 unit。

保持 `/etc/havn/havn.env` 的权限为 0600、属主为 root，并用编辑器写入密钥，而不要用 `echo`——后者会把它留在 shell 历史中。systemd 会按字面解析单引号内的值；值中不能包含 `'` 或 `\`。

Agent 的 cgroup 为 `/sys/fs/cgroup/havn/agent-<id>`。网关自己创建这个顶层 cgroup 并把 Agent 移入其中，位于 unit 自身的 cgroup 之外，因此这不是 systemd 委派；`Delegate=yes` 只是让 cpu、memory 和 pids 控制器从根 cgroup 起一路保持启用。`havn` 不是 unit 名，因此 systemd 不会干涉该 cgroup。旧版网关使用 `/sys/fs/cgroup/havn.slice`：启动时，网关会杀掉仍在其中运行的 Agent，删除它们的 cgroup，并删除 `havn.slice` 本身，除非 systemd 有一个活动的 `havn.slice` unit 或其中还有其他东西。本地修改请放在 drop-in 中（`systemctl edit havn`），而不要编辑已安装的文件，重新安装会覆盖它。

<a id="upgrading"></a>
### 升级

自 v0.2.9 起，`sudo havn update` 可以升级以 systemd unit `havn` 运行的网关（即 install.sh 的安装方式）。它读取该网关的 `HAVN_CONFIG` 和 `HOME`，以找到其配置、数据和 `listen` 地址。然后它会：为本机下载最新发布版（或 `--version v0.2.10`），并用发布版的校验和文件核对压缩包的 sha256（这只能说明下载完整，不能说明是谁制作的：两者来自同一个发布版，与 install.sh 相同）；解包其中的二进制；备份当前安装（备份到 `~/havn-backup-before-<version>-<time>.tar.gz`，在 `sudo` 下即 root 的家目录；`--no-backup` 可跳过）；用新二进制逐一替换与当前 `havn` 同目录的旧二进制（旧版保留为 `<name>.old`，网关最后替换）；重启 unit；然后等待网关以新版本身份应答，只要 systemd 仍让它运行就一直等待（大型数据库迁移需要一些时间），最长 10 分钟。若网关失败，或被 systemd 重启，则恢复其先前的二进制并重启；如果此时它已经迁移了数据库，请用 `havn restore` 恢复备份。安装比当前网关更旧的发布版需要 `--allow-downgrade`，因为旧网关无法打开由新网关迁移过的数据库。以其他方式运行的网关（在 shell 中、在其他 unit 中）不会被触碰：先停止它，运行 `havn update`（此时只会替换二进制），再启动它。`havn update --check` 只列出有哪些更新的发布版，不安装任何东西。发布版中的 `havn.service` 不会覆盖你的 unit。网关每天还会检查一次是否有新版本（通过 GitHub 的 API，在启动一分钟后），Dashboard 会在其版本号旁边向所有使用者显示；在 config.toml 中设置 `update_check = false` 可关闭此功能。

自 v0.2.13 起（指运行该命令的 `havn`：从更早版本更新后，请再运行一次 `sudo havn update`），`havn update` 接着会更新本机上的渠道适配器，每个都从其自身仓库的最新发布版更新；`--only gateway` 只更新网关，与以前一样，而 `--version` 和 `--allow-downgrade` 只作用于网关。它首先读取 `/etc/havn/components.toml`：如果该文件无法读取，或包含它不认识的键，则什么都不更新（使用 `--check` 时，表格会注明）。它处理文件中列出的适配器，以及通过 systemd unit 找到的适配器：位于 `/etc/systemd/system`、`/usr/local/lib/systemd/system` 或 `/lib/systemd/system` 中、其文件或某个 drop-in（`<unit>.d/*.conf`）指向 `havn-channel-*` 二进制的 unit，随后会向 systemd 查询它实际运行什么（`systemctl show -p ExecStart`，因此 drop-in 也算数；别名即它所指向的 unit）。不会查看 `x@.service` 这类模板 unit：请在文件中列出它们的实例。找到的适配器只有当其二进制位于 `/usr/local/bin`（发布版的安装位置）时才会更新；位于其他位置的（从源码构建、通过软件包安装）显示为未托管，只有在文件中列出后才会更新。对于每个最新发布版比其当前版本（`<binary> --version` 的输出，否则为文件记录的版本）更新的适配器，它会下载 `<name>-<arch>-unknown-linux-musl.tar.gz`（`<name>` 为仓库名，压缩包中包含同名二进制），并用其 `.sha256` 校验；替换二进制，并沿用旧二进制的属主、权限和文件 capabilities（旧版保留为 `<binary>.old`）；检查新二进制报告的是否为该发布版；然后逐个重启其正在运行的 unit，每个一次（已停止的不会被启动），并观察它：它必须保持 active 且没有被 systemd 重启，其主进程必须运行刚替换的二进制，并且如果它上次运行时记录过 `havn WS connected` 且网关有应答，则必须在一分钟内再次记录该日志。不满足条件的适配器会恢复为先前的二进制并在其上重启它的 unit；网关和其他适配器保持不变，`havn update` 报错退出并指出该适配器，对于列出的但 GitHub 上不存在的仓库也是如此。尚无发布版的仓库会被跳过。如果发布说明中有一行 `Requires Havn >= X.Y.Z`（不区分大小写，可带 Markdown 修饰，可写 `≥` 或 `Requires:`），且指定的网关版本比正在运行的网关、或 `--check` 将要安装的网关更新，则跳过该发布版；如果该行中没有可识别的版本，同样跳过；没有这样一行的说明意味着任何网关都可以。如果网关自身的更新失败，则不会触碰任何适配器。`--check` 会优先采用文件记录的版本，其次才询问二进制，并以 `nobody` 身份运行它。`/opt/havn` 下的能力包（包含 `SKILL.md` 或 `setup.sh` 的目录，或浏览器能力包的 `/opt/havn/playwright-cli`）会被列出但不会更新：能力包通过运行其新版 `setup.sh` 来更新，该脚本会安装系统软件包，且无法回滚。最后会显示一个表格，列出每个组件、其原版本、查看的发布版以及执行的操作（使用 `--check` 时，列出已安装版本和最新版本）。

`/etc/havn/components.toml` 列出网关之外已安装的组件。当发现或安装的内容与文件不一致时，`havn update` 会重写该文件，不保留注释，保持其权限（新文件为 0600，若 `/etc/havn` 不存在则创建为 0700）。手动配置、以自身 unit 运行、二进制位于 `/usr/local/bin` 的适配器无需列出也能被找到。以下情况需要列出：二进制在其他位置、其 unit 以其他方式运行它（模板实例、包装脚本），或其仓库不属于 amplimit；`skip = true` 会让某个适配器或能力包保持不动：

```toml
[[adapter]]
name    = "havn-channel-telegram"
repo    = "amplimit/havn-channel-telegram"
binary  = "/usr/local/bin/havn-channel-telegram"
units   = ["havn-channel-telegram.service"]
version = "0.2.11"

[[adapter]]
name   = "havn-channel-lark"
repo   = "amplimit/havn-channel-lark"
binary = "/opt/lark/havn-channel-lark"
units  = ["lark.service"]
skip   = true

[[pack]]
name = "havn-pack-browser"
repo = "amplimit/havn-pack-browser"
path = "/opt/havn/playwright-cli"
```

`repo` 是其发布版所在的位置，`units` 会在更新后重启，`version` 由 `havn update` 维护。

手动升级，或 v0.2.8 及更早版本：

```bash
sudo -i
# 首先备份，使用生成这些数据的 havn（见“备份与恢复”）；
# 备份失败则终止后续步骤
havn backup -o ~/havn-backup-$(date +%Y%m%d-%H%M%S).tar.gz &&
# 将二进制替换为目标发布版；此时正在运行的网关尚未受影响
curl -fsSL https://raw.githubusercontent.com/amplimit/havn/main/install.sh | HAVN_VERSION=<version> bash &&
systemctl restart havn
```

正在停止的网关不再接收新消息，并给 Agent 最多 30 秒完成手头的工作。打开的聊天仍会收到这些回复，若某条回复被截断也会收到通知。v0.1.4 及更早版本的网关会立即停止其 Agent，因此从这些版本升级前的那次停止会丢弃进行中的工作；请选择空闲时段进行。原先在运行的 Agent 会随网关一起重新启动，Dashboard 会自动重连。

v0.2.0 及更早版本没有 `havn backup`：此时请先停止网关并复制其数据（`systemctl stop havn && cp -a ~/.local/share/havn ~/havn-backup-$(date +%Y%m%d-%H%M%S)`，如果 `db_path` 或 `workspace_root` 在其他位置也一并复制），然后再安装并启动。

新网关和 Agent 启动时会迁移各自的数据库，且无法回退，因此在新版本正常工作之前请保留备份。Dashboard 随网关一起提供，并随网关一起升级。v0.2.7 及更早版本中它作为独立的 Node 进程运行：从这些版本升级时，请在重启网关前停止并移除该进程；网关会在 `dashboard_listen`（默认 `127.0.0.1:3000`）上提供 Dashboard，将 `dashboard_listen` 设为旧进程使用的地址，即可保持隧道和代理配置不变。已打开的 Dashboard 标签页仍运行旧页面，并会在版本号旁提示：刷新即可。

<a id="backup-and-restore"></a>
### 备份与恢复

```bash
sudo -i
havn backup                          # 在当前目录生成 havn-backup-<time>.tar.gz；-o 指定文件名
systemctl stop havn
havn restore havn-backup-<time>.tar.gz [--force]
systemctl start havn
```

`havn backup` 写出一个归档文件（权限 0600，写完之前使用临时文件名），其中包含：

- 网关数据库以及每个 Agent 数据库的一致性副本（SQLite `VACUUM INTO`），因此网关及其 Agent 可以继续运行；
- 每个 Agent 的目录：工作区、inbox、outbox 和 home，保留属主、权限和时间戳；
- `config.toml`。

工作区中的链接作为链接保存，从不跟随；64 MB 及以上的稀疏文件（含空洞）不纳入备份。复制期间持续变化或无法读取的 Agent 数据库会被排除在本次备份之外并给出警告（归档的 manifest 会列出它），其余部分照常备份；随后 `havn backup` 以错误退出，以便定时器能反映出来。请将归档写到 `workspace_root` 之外。备份中不包含 `HAVN_AGE_KEY`：其中的凭据保持加密，恢复它们需要同一个密钥，因此请将密钥与归档分开保管。

`havn restore` 在做任何更改之前会先完整读取归档。它要求网关已停止（并在运行期间持有网关的锁），并要求所用的 havn 认识备份中的数据库迁移，因此在升级前做的备份应使用其原版本恢复；网关启动时会迁移数据库。在已有安装的主机上，除非指定 `--force`，否则它会拒绝恢复；`--force` 会先把网关的数据库文件和 `workspace_root` 移到 `<path>.pre-restore-<time>`。已存在的 `config.toml` 会被保留，归档中的配置写在旁边，名为 `config.toml.restored`；在没有配置的主机上，归档中的配置直接就位，数据库和 Agent 恢复到其中指定的位置。

迁移到新主机：

```bash
sudo -i
curl -fsSL https://raw.githubusercontent.com/amplimit/havn/main/install.sh | HAVN_SKIP_SETUP=1 bash
install -d -m 700 /etc/havn && install -m 600 /dev/null /etc/havn/havn.env
sudoedit /etc/havn/havn.env          # HAVN_AGE_KEY='<旧主机的密钥>'
havn restore havn-backup-<time>.tar.gz
systemctl enable --now havn
```

使用 systemd 每日备份，保留两周：

```ini
# /etc/systemd/system/havn-backup.service
[Service]
Type=oneshot
# 网关 unit 查找其配置和数据的位置
Environment=HOME=/root
EnvironmentFile=-/etc/havn/havn.env
WorkingDirectory=/var/backups/havn
ExecStart=/usr/local/bin/havn backup
# 备份失败后同样执行，避免旧归档不断堆积
ExecStopPost=/usr/bin/find /var/backups/havn -name 'havn-backup-*.tar.gz' -mtime +14 -delete

# /etc/systemd/system/havn-backup.timer
[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

先创建目录（`install -d -m 700 /var/backups/havn`），然后运行 `systemctl enable --now havn-backup.timer`。还要把归档复制到主机之外；与数据在同一块磁盘上的备份，无法在磁盘损坏后幸存。

<a id="build-from-source"></a>
### 从源码构建

```bash
cargo build --release
./target/release/havn setup
# ……然后执行与上文相同的凭据 / 启动步骤。
```

<a id="dashboard"></a>
### Dashboard

网关自带与其版本对应的 Dashboard，无需另行安装或启动：
打开 `http://localhost:3000`，创建一个 Agent，即可开始聊天。
它在 config.toml 的 `dashboard_listen` 上提供服务
（默认 `"127.0.0.1:3000"`，设为 `""` 则不提供），管理 API 位于同源的
`/api` 下，网页聊天位于 `/ws` 下。

它没有自己的登录机制。未设置 `trust_header` 时，它只响应发往
`localhost`、`127.0.0.1` 或 `[::1]`（或 `allowed_origins` 中列出的 origin）的请求，
因此主机名解析到 127.0.0.1 的网页无法使用它；`listen` 上的管理 API 也做同样的检查。
从其他机器访问时，请使用 SSH 隧道
（`ssh -L 3000:127.0.0.1:3000 <server>`）或带认证的反向代理
（见 [在 SSO 代理后的多用户模式](#multi-user-mode-behind-an-sso-proxy)）。

从源码构建的网关，如果先构建了 Dashboard（在 `apps/dashboard` 中运行
`npm ci && npm run build`，需要 Node.js 22.13+；install.sh 检测到 npm 时会自动执行），
就会将其嵌入；否则该端口会提示如何构建。开发服务器（`npm run dev`）请参阅
[apps/dashboard/README.md](../apps/dashboard/README.md)。

<a id="usage"></a>
### 用量

Dashboard 的 **usage** 页面（`GET /usage?days=30`）显示调用者的模型调用用量，时间范围从 7 天到一年：调用次数和 token 数，以及对 `[[llm.models]]` 条目设有 `price` 的模型（见“配置”）以美元计的费用，包括总计以及按天、按 Agent 和按模型的细分。天数按完整的 UTC 日计算。每次调用都计入所请求的模型（以 `[[llm.models]]` 中的名称为准），从提供商提示词缓存读取和写入的输入 token 分开计价；这两点都从 v0.2.9 开始记录，因此此前的调用计入提供商返回的名称（通常不是已配置的名称，因此没有定价），其缓存 token 按输入价格计算。仍在进行中、或提供商未报告用量的调用，按为其预留的量（即上限）计算，页面会显示这类调用的数量。通过搜索后端（Brave、Tavily、Exa）进行的搜索单独统计，不算作模型调用。删除凭据时，其上的用量随之删除。未设置 `trust_header` 时，网关的所有者（其第一个用户，在网关首次启动时创建：在单用户网关上就是你）可以查看任意用户或所有人的用量（`?user=<id>` 或 `?user=all`），并附有按用户的合计；设置 `trust_header` 时，每个人只能看到自己的用量。团队管理员保留团队的用量页面。

<a id="metrics"></a>
### 指标

在 config.toml 中设置 `metrics_listen`（例如 `"127.0.0.1:9464"`；默认为空，即不提供），网关便会在该地址的 `/metrics` 上以文本格式提供 Prometheus 指标，且无需登录：请将其限制在回环地址或只有你的 Prometheus 能访问的网络上（不是回环地址时网关会发出警告，但仍会提供服务，这一点与 `listen` 不同）。它必须使用独立的端口，并在 `havn gateway restart` 时生效；如果端口被占用，网关会提示并在没有指标的情况下运行。计数器在网关启动时从零开始：

- `havn_build_info{version}`、`havn_uptime_seconds`；
- `havn_agents_connected` 和 `havn_channel_adapters_connected`，当前值；
- `havn_llm_calls_total{provider, model, outcome}`，为 Agent 发起的 LLM 调用，对提供商的每次调用只计一次，无论尝试了多少个凭据（网关缩短后重新发送的请求会再计一次；`web_fetch` 尝试的每个模型也各计一次）。通过 OpenRouter 进行的网页搜索不计入此处，网关自身的调用（例如检查模型）也不计入。`outcome` 为 `ok`、`no_credential`、`unavailable`、`upstream_error`、`transport_error`、`budget_exhausted` 或 `other_error`；
- `havn_llm_tokens_total{provider, model, direction}`，有应答的调用的输入 token（含缓存读取和写入）和输出 token；
- `havn_channel_messages_total{channel, direction}`，适配器传入的消息（全部，包括被拒绝的）以及写给适配器的回复（分段发送的回复每段都计数；适配器重连后重写的回复只计一次）；
- `havn_cron_runs_total{outcome}`，定时任务的运行次数：`success`、`failure`（也包括无法发送给其 Agent 或其输出无法保存的情况），或被唤醒检查 `skipped`。

每个提供商最多记录 1000 个模型，渠道最多 1000 个；超出后新出现的一律计为 `__other__`。

<a id="project-structure"></a>
## 项目结构

```
crates/
  havn-core/       共享类型：ID、消息、策略、错误
  havn-proto/      线路协议（Agent↔网关 + 渠道适配器）
  havn-db/         SQLite 连接池、迁移、仓储
  havn-gateway/    网关服务：路由、认证、LLM 代理、cron
  havn-runtime/    Agent 运行时：LLM 循环、工具、记忆、整理器
  havn-spawner/    Agent 启动器：命名空间、cgroup、Landlock、seccomp
  havn-init/       用于 Agent 命名空间初始化的 PID-1 垫片
  havn-cli/        CLI：初始化，Agent/凭据/用户/团队管理
apps/
  dashboard/       Next.js Dashboard + WebChat
```

<a id="design-philosophy"></a>
## 设计理念

havn 是**基础设施，而不是 SaaS**。判断标准：这件事是否需要内核原语、LLM 代理或每个 Agent 独立的 SQLite？如果需要，由 havn 来做；如果不需要，由运维者自行接入。

| 关注点 | 由谁负责 |
|---------|----------------|
| 用户认证 | 运维者的反向代理（Authelia、Keycloak、Cloudflare Access、Tailscale 等） |
| TLS 终止 | 反向代理 |
| 向 Slack/Telegram 的出站投递 | 外部渠道适配器守护进程 |
| 计费 / 美元成本 | 运维者的分析系统（havn 记录 token 数） |
| 技能共享 | GitHub / git clone |

<a id="target-deployment"></a>
## 目标部署场景

- **个人**：一台 5 美元的 VPS，网关与 Agent 部署在一起，通过 CLI 或 Dashboard 管理
- **小团队（5–50 人）**：同样是单台服务器，按 Agent 划分 cgroup 资源，由反向代理负责认证
- **单台服务器承载多个客户**：每个客户一个非特权 LXD 容器，各自运行独立的网关；参见 [docs/multi-tenant.md](multi-tenant.md)

<a id="security-model"></a>
## 安全模型

- Agent 运行在隔离的 Linux 命名空间（PID、mount、network、user）中，并启用 Landlock LSM 和 seccomp-bpf 系统调用过滤
- Agent 运行时、其工具和 MCP 服务器不持有任何 capability，也无法通过 exec 重新获得。在 Agent 自己的网络命名空间中，它们仍可使用 `localhost`（在无出站网络时也可以）、`ping`，以及绑定 1024 以下的端口；绑定进 Agent 的宿主机目录均为 `nosuid`，因此其中的 setuid 位和文件 capability 不会生效。
- 网关以 root 运行时，每个 Agent 以独立的宿主机 uid 和 gid 运行（从 `0x71000000` 起分配，永不复用；若在映射 id 较少的容器内，则从其映射的 65535 以上 id 的中段开始），且没有附加组：Agent 的 root 不是宿主机的 root。Agent 拥有自己的工作区——0.1.7 之前的网关会把工作区留给 root，这类工作区会在 Agent 下次启动时移交（之后以 root 身份复制进工作区的文件不会移交：请将其 `chown` 给工作区的属主）——并且只有 root 能进入工作区的上级目录，因此其他宿主机用户都无法接触 Agent 设为 setuid 的文件。读写型 `extra_mounts` 目录只有在其权限或 ACL 允许该 uid 写入时，Agent 才能写入；Agent 在其中创建的文件归其 uid 所有
- 宿主机 `/usr` 下安装的程序以只读方式对 Agent 可用。对于宿主机的 `/etc` 和 `/var`，Agent 能看到 `/etc/ssl/certs`；有出站网络时还能看到一个生成的 `/etc/resolv.conf`；此外，以只读方式、且仅在存在时，能看到已安装程序运行时读取的目录：`/etc/alternatives`（Debian 和 Ubuntu 上 `java`、`cc`、`awk` 等命令背后的链接）、`/etc/fonts`、`/etc/groff`、`/etc/libreoffice`、`/etc/perl`、`/etc/python3`、`/etc/python3.*`、`/etc/texmf`、`/etc/java-*`、`/etc/ImageMagick-*`、`/var/lib/ghostscript` 和 `/var/lib/texmf`。针对这些路径之一的 `extra_mounts` 条目会替换默认挂载。Shell 命令和其他工具进程的 `HOME` 位于 Agent 工作区的 `.havn/home`（已被 git 忽略）。MCP 服务器的 `HOME=/nonexistent`，除非其 `env` 另行设置，因此工具留在其 home 中的任何东西都不会在 MCP 服务器中运行
- 如果 Landlock 不可用（需要内核 5.13+，且 `lsm=` 列表中包含 `landlock`；`havn doctor` 会检查），Agent 不会启动。若仍要在这类内核上运行不受文件系统限制的 Agent，请以 `HAVN_ALLOW_NO_LANDLOCK=1` 启动网关
- Agent 无法创建 user 命名空间（带 `CLONE_NEWUSER` 的 `unshare`/`clone` 会以 `EPERM` 失败；`clone3` 返回 `ENOSYS`，调用方因而回退到 `clone`），这样 user 命名空间向非特权进程开放的那部分内核代码就不在其可及范围内。工作负载需要嵌套 user 命名空间的能力包（如 Chromium 的沙箱）需要设置 `[seccomp] allow_extra = ["user_namespaces"]`
- 工具启动的每个进程（Shell 命令，以及用于自动提交的 git）都运行在嵌套于运行时之下的 Landlock 域中，因此无法读取 MCP 服务器（它们留在运行时的域中）的环境变量或内存，也无法读取其 `/proc/<pid>/fd` 和 `cwd`。但它仍能看到这些进程的命令行（通过 `env` 而不是 `args` 向 MCP 服务器传递密钥），并且可以读写任一 MCP 服务器 `extra_paths_rw`/`extra_paths_ro` 下的文件——Agent 的每个进程都能使用这些路径。MCP 服务器代表 Agent 运行的代码（例如运行 Agent 仓库中钩子的 git 服务器、执行命令的服务器）留在运行时的域中，因此不要把这类服务器与持有密钥的服务器同时分配给同一个 Agent。每次工具调用都有独立的域：命令无法读取先前调用遗留运行的进程的上述 `/proc` 条目，不过 `ps`、`kill` 和 `pkill` 仍然可用
- Agent 的网络访问有三种设置（`network_policy.egress_allowed` 和 `egress_proxy`，位于 Agent 设置页）：关闭；仅允许的域名，经由网关的出站代理；或所有公网地址。走代理时，Agent 的命名空间没有越过网关主机的路由，发往非本主机地址的流量一律丢弃，其工具会获得指向 `http://<其链路的主机地址>:<[network] egress_proxy_port>`（913）的 `HTTP_PROXY`/`HTTPS_PROXY`。网关在启动 Agent 前绑定该端口，绑定失败则不会启动；过滤入站流量的主机防火墙（ufw、firewalld）必须允许 Agent 地址池访问它。代理会隧道转发 HTTPS（`CONNECT`，端口 443），并将普通 HTTP（端口 80，仅支持带 `Content-Length` 的请求体）转发到允许域名的公网地址；被阻止的域名会被拒绝；对于既不在允许列表也不在阻止列表中的域名，会在 Dashboard 聊天中询问所有者：允许 10 分钟、始终允许（加入 Agent 的允许域名）或拒绝。拒绝的结果保持 10 分钟，且一个 Agent 每分钟最多发起 5 个新询问。常见软件包源（PyPI、npm、crates.io、Go proxy、GitHub、Debian 和 Ubuntu 软件源）的列出主机会合并在一起询问，并注明来源名称。无人回答的询问在 5 分钟后被拒绝；所有回答以 `egress.domain` 记入审计。代理检查的是工具请求的主机名，而不是 TLS 隧道中实际承载的内容：按内部 `Host` 路由请求的服务（共享 CDN 上的域前置）可以访问其背后的其他站点，因此只允许那些你愿意放行其背后一切的域名。今后新建的 Agent 默认采用 `[network] default_egress`（除非设为 `off` 或 `all`，否则为 `allowlist`），除非其配置另有指定；已有 Agent 保持原设置，修改在 Agent 下次启动时生效。走代理时，不遵循代理变量的工具（例如 Node 内置的 `fetch`）或使用其他协议的工具（基于 SSH 的 git）没有网络。
- 有出站网络的 Agent 只能访问公网：发往网关主机（包括 Docker 发布的端口）、其他 Agent、私有地址、链路本地地址（包括云元数据服务）、CGNAT 和保留地址的流量，会在 `mangle PREROUTING` 中先于任何其他防火墙规则被丢弃，且 Agent 链路的主机端没有 IPv6。配置的 `agent_dns` 服务器仍可通过 53 端口访问，即使它们运行在网关主机上；Agent 可以访问的其他内网地址请列在 `[network] agent_private_access = ["10.0.5.20", "192.168.1.0/24"]` 中。如果这些规则无法安装（没有 `iptables`），Agent 将完全没有网络。`firewall-cmd --reload` 或类似的清空操作会移除这些规则；此后启动的 Agent 在网关重启前都没有网络。将 53 端口重定向到本地解析器的全局规则也会破坏 Agent 的 DNS，因为重定向后的目标已不再是 `agent_dns` 服务器
- 即使对没有出站网络的 Agent，搜索查询也会离开主机：`web_search` 会将其发送到搜索后端，因此 Agent 可能把工作区中的数据放进查询里。对数据不得外泄的 Agent，请关闭 `can_search_web`
- `web_fetch` 请求同样会离开主机。Agent 只能读取用户发给它的 URL、搜索结果和已读页面上的链接，或其允许域名下的地址，因此无法把工作区数据塞进自己编造的 URL。但被访问的页面仍能看到请求，恶意页面也能诱导 Agent：一个提供大量链接页面的站点，可以通过 Agent 每次选择哪个链接，在每用户速率限制之内逐步套取其数据。只允许那些你信任不会回显 URL 或任意重定向的域名。对完全不应访问网络的 Agent，请关闭 `can_fetch_web`。没有出站网络时，沙箱本身仍然没有网络：`web_fetch` 不是 Shell 命令下载文件的途径
- API 密钥使用 [age](https://age-encryption.org/) 静态加密——Agent 进程永远看不到它们
- token 上限在网关侧强制执行：按凭据（`max_tokens_per_day`）、按共享凭据上的用户（`per_user.max_tokens_per_day`、`per_user.max_requests_per_minute`），以及按 Agent 跨所有凭据（策略 `budget.max_tokens_per_day`，预置的 member 角色为 1,000,000；`on_exhaust = "warn_only"` 仅记录日志，其他取值会拒绝调用）。每次调用在发送前预留其最坏情况（估算的输入加上 `max_tokens`），返回后按提供商报告的用量结算，因此并行请求不会超出上限；接近上限时，`max_tokens` 会被降到剩余额度。用量包括提示词缓存的读写和推理 token。Agent 向 Anthropic 模型发出的请求（无论直连还是经由 OpenRouter）会把每轮共享的系统提示词部分（连同其前面的工具）和对话末尾标记给 Anthropic 的提示词缓存，这样一轮中的后续回合以及 Agent 的下一轮都能以缓存价格读取它们。为此，系统提示词在各轮之间保持不变：对话的提示词注明所有者的时区，用户的每条消息开头标注发送时间（`[Sent Fri 2026-11-20 09:30]`），最新一条即当前时间。这也使对话能被自行缓存提示词的提供商缓存。一次性调用（标题、摘要、页面问答）不做标记。以 OpenAI 的 `prompt_tokens_details` 或 DeepSeek 的 `prompt_cache_hit_tokens` 报告的缓存读取会单独计数；没有用量信息的响应，以其预留额作为记录的用量。`max_tokens` 还会被限制在 `[[llm.models]]` 中该模型的 `max_output_tokens` 之内（未设置时，对 Anthropic、OpenAI 和 Gemini 模型使用已知的输出上限），以及 `[llm] max_tokens_ceiling` 之内
- Agent 只能使用 `[[llm.models]]` 中的模型。策略的 `allowed_models` 可进一步收窄范围；条目为 `provider:model` 形式的 id，或用 `*` 表示任意已列出的模型。对于没有出站网络的 Agent，网关还会拒绝会让提供商去抓取 URL 的 LLM 请求（URL 来源的图片和文档、提供商侧工具、常见名称下内置网页搜索的模型）；要确保 Agent 无法选用自带网络访问的模型，请限制 `allowed_models`
- WebChat 的 WebSocket 连接会按 Origin 允许列表（`allowed_origins`）校验，并且必须出示调用者从 `GET /me` 获得的 `ws_token`；该令牌用于标识用户而非充当密钥，因此真正阻止跨站连接的是 Origin 检查
- 没有市场——技能是本地文件，通过 git 共享

<a id="policy"></a>
## 策略

Agent 能做什么由一份 JSON 策略（`havn_core::Policy`）决定。其顶层键如下，括号内为省略该键时的默认值：

- `max_agents`：一个用户可创建的 Agent 数量（1；不属于任何团队的用户为 50，或 `HAVN_MAX_AGENTS_PER_USER`）。
- `allowed_models`：Agent 可使用的 `provider:model` id，或用 `*` 表示 `[[llm.models]]` 中的任意模型（`["*"]`）；参见[配置](#configuration)。
- `resource_limits`：Agent 的 cgroup 限制，`memory_mb`（512）、`cpu_cores`（1.0）和 `pids_max`（64）。
- `budget`：`max_tokens_per_day`（0，不设上限）和 `on_exhaust`（`warn_only`、`warn_and_pause` 或 `fail`；默认 `warn_and_pause`）；参见[安全模型](#security-model)。
- `permissions`：工具和功能开关：`can_use_shell`、`can_access_network`、`can_install_skills`、`can_schedule_cron`、`can_search_web` 和 `can_fetch_web`（默认开），`can_use_mcp`、`can_spawn_subagents` 和 `can_query_other_agents`（默认关）。`can_view_own_logs`、`can_export_memory` 和 `can_bind_channels` 属于 schema 的一部分，但 Havn 中没有任何地方检查它们。
- `network_policy`：`egress_allowed`、`egress_proxy`、`allowed_domains` 和 `blocked_domains`；参见[安全模型](#security-model)和[读取网页](#reading-web-pages)。
- `context_toolsets`：在某个上下文中关闭的工具名，形如 `{"cron": {"disabled": ["shell"]}}`。上下文包括 `webchat`（来自 Dashboard 或聊天应用的消息）、`heartbeat`、`cron`、`subagent` 和 `agent_query`。
- `admin_visibility`：`can_view_audit_log` 允许持有该角色的团队管理员读取团队审计日志；`can_view_agent_status`、`can_view_agent_config` 和 `can_view_conversations` 属于 schema 的一部分，但 Havn 中没有任何地方检查它们。
- `mcp_servers`：参见 [MCP 服务器](#mcp-servers)。
- `hooks`：参见[工具钩子](#tool-hooks)。
- `chat_titles`：参见[聊天标题](#chat-titles)。
- `tool_rounds`：`max`（100），一条消息可使用的工具回合数；`subagent`（30），子 Agent 任务、其他 Agent 的提问、心跳或 cron 任务的回合数。达到上限时，模型会被要求总结已完成的工作和剩余事项。
- `shell`：`timeout_seconds`（300），命令未指定时的运行时长；`max_timeout_seconds`（1800），命令可申请的最长时长；`background`（8），可同时在后台运行的命令数。

策略在两处设置：

- **团队角色。**每个团队成员持有一个角色，其策略约束该成员的 Agent。运维者通过 `havn role add <team-id> <name> --policy <file>` 和 `havn role set-policy <role-id> <file>` 设置，后者会用文件内容整体替换策略（省略的键取上述默认值；`havn role show <role-id>` 打印当前策略）。团队管理员可在 Dashboard 的团队角色页面以 JSON 编辑（`PATCH /teams/{id}/roles/{role_id}`）。策略在保存时校验。在团队中创建的 Agent 采用所有者在该团队中的角色；对于所有者的其他 Agent，身兼多个角色的用户取其中最宽的权限（但每个角色的钩子规则和阻止域名仍然生效）。不属于任何团队的用户使用默认值。
- **Agent 的覆盖配置**，即 `config.policy`。它按键叠加在所有者的策略之上，因此只改变它设置的键；`mcp_servers` 和 `context_toolsets` 则整体替换。Agent 设置页会写入其中的部分内容（网络与网页访问、MCP 服务器、需要审批的工具、聊天标题）；带 `config.policy` 的 `PATCH /agents/{id}` 会整体替换已存储的覆盖配置。

在单用户网关上，覆盖配置原样生效，但它不能移除所有者的钩子规则或域名列表，并且除非覆盖配置将其关闭，子 Agent 默认开启。在多用户模式（`trust_header`）下，覆盖配置会被限制在所有者的策略之内：开关只有两边都开启时才开启；限制取两者中较低的值（0 表示不限，让位于另一方）；`on_exhaust` 取更严格的；`allowed_models` 只保留两边都允许的；域名列表和钩子规则累加；所有者策略要求出站走代理时保持走代理；`admin_visibility` 取所有者的；只保留所有者策略中列出的 MCP 服务器，并使用所有者的设置。

Agent 的工具和沙箱所用的策略在 Agent 启动时解析，因此修改在其下次启动时生效。网关自身的检查（模型调用、预算、`web_fetch`、对其他 Agent 的查询）在每次请求时解析策略，修改会立即生效。

<a id="tool-hooks"></a>
## 工具钩子

PreToolUse 钩子在工具调用运行前，若其某个正则模式匹配到调用的主要输入（Shell 命令、文件路径、URL），就拒绝该调用。Agent 会把拒绝信息作为工具结果看到，并可据此调整计划。

运维者把钩子放在策略中（角色的策略，或单个 Agent 的覆盖配置）的 `hooks` 下：

```json
"hooks": {
  "pre_tool_use": [
    { "tool": "shell", "deny": ["^rm -rf", "^sudo "] },
    { "tool": "web_fetch", "deny": ["://internal\\.corp\\.net"] }
  ]
}
```

Agent 工作区中也可以包含 `.havn/hooks.yaml`，其中是同样的 `pre_tool_use` 列表，因此现有文件可以原样粘贴进策略。由于 Agent 可以编辑自己的工作区，这个文件只能增加规则：运行时会同时应用策略中的规则和文件中的规则，修改或删除该文件永远不会移除策略规则。两者都在 Agent 启动时读取。单个 Agent 的策略覆盖可以增加钩子规则，但不能移除其所有者的规则；身兼多个角色的用户会获得每个角色的规则。一份策略最多包含 64 条规则和 256 个模式，每个模式最长 1024 字节，无效的策略在保存时会被拒绝。工作区文件的读取较为宽松：未知键会被忽略，无效模式以及超出上述上限的规则会被跳过并给出警告。针对 Agent 不具备的工具的规则，会在 Agent 启动时记录警告。

钩子是防范模型失误的护栏，而不是安全边界。模式匹配的是文本，一个有意为之的 Agent 可以用任何模式都匹配不到的形式执行同样的操作（`rm  -rf`、`cd / && rm -rf`、脚本文件）。要真正阻止某个操作，请使用策略权限（`can_use_shell`、`can_access_network`）、`network_policy`、Agent 获得的挂载，以及上文所述的沙箱。

规则也可以用 `ask` 代替 `deny`：展平后的输入匹配 `ask` 模式（且不匹配任何 `deny` 模式）的调用会等待 Agent 所有者批准。Agent 的 Dashboard 聊天的每个标签页都会显示该工具及其完整输入，并提供批准、在本对话中批准（相同工具和输入，直到 Agent 重启）和拒绝三个选项，在等待期间新打开或重新加载的标签页也会显示；如果调用来自某个渠道会话，该会话会被告知正在等待，因为渠道的发送者（比如客户）不应批准 Agent 的操作。10 分钟内无人回应的调用会被拒绝；输入超过 4000 个字符的调用，以及在定时任务、心跳或其他 Agent 查询中、无人可问的调用，也会被拒绝。`.havn/hooks.yaml` 中无法编译的 `ask` 模式会对其工具的每次调用发起询问。`tool` 可以以 `*` 结尾来匹配前缀，例如某个 MCP 服务器的所有工具：

```yaml
pre_tool_use:
  - tool: mcp__github__*
    ask: ["(?s).*"]
  - tool: shell
    ask: ["\\bdeploy\\b", "^git push"]
```

Agent 通过 `file_find`（按名称，使用 glob）和 `file_search`（按内容）查找文件，两者都不需要 `shell`。它们会排除那些被关于 `file_read`（或 `file_*`）的规则拒绝或需要询问的文件（按文件在工作区中的路径匹配），因此搜索无法读到 `file_read` 不可读取的内容；它们从不跟随链接，并跳过 `.git`、`.havn` 和 Agent 的数据库。

`view_image` 向 Agent 展示其工作区中的一张图片（PNG、JPEG、GIF、WebP 或 HEIC）：它截的图、画的图表、下载的图片。网关读取该文件（不跟随链接），并将其放入发给 Anthropic 的工具结果中。对于 OpenAI 兼容的提供商，由于其工具结果不能携带图片，图片会放在紧随结果之后的一条消息中；对于 Gemini，则放在函数响应之后。它与附件图片受相同的限制：不支持 `vision` 的模型会收到一行说明；超过 3.75 MB、单边超过 8000 像素或为 HEIC 格式的图片，会以网关为该请求生成的副本发送（最多读取 25 MB；不写入任何文件）；超出图片数量或总大小限制的则不展示。后续轮次只保留一行注明该图片的文字，而不保留图片本身，因此 Agent 需要时会再次查看。关于 `file_read`（或 `file_*`）的规则同样适用于它，在某个上下文中禁用 `file_read` 也会一并禁用它。

Agent 设置页维护一个每次调用都需要审批的工具列表（`GET` 和 `PUT /agents/{id}/approvals`）。请求和决定以 `tool.approval.requested` 和 `tool.approval.decided` 记入审计，并记录决定者。与 `deny` 一样，`ask` 防范的是匹配输入上的模型失误；真正的强制执行仍靠沙箱和权限。

<a id="mcp-servers"></a>
## MCP 服务器

Agent 可以使用运维者安装的 MCP（Model Context Protocol）服务器提供的工具。服务器是宿主机上 `/usr/share/havn/mcp-servers/` 中的二进制文件，Agent 通过其 `/usr` 以只读方式看到它们；Agent 和用户都无法自行添加。只有开启 `policy.permissions.can_use_mcp`（默认关闭）时，Agent 才会运行服务器，并且只运行 `policy.mcp_servers` 中以运维者自定名称列出的服务器：

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

- `binary` 是该目录中的文件名，不能是路径（不含 `/` 或 `..`）。
- `args` 原样传给二进制文件，不经过 Shell；`env` 会添加到其环境变量中。密钥请通过 `env` 而不是 `args` 传递（参见[安全模型](#security-model)）。
- 服务器启用期间，`extra_paths_rw` 和 `extra_paths_ro` 会被加入 Agent 每个进程（包括其自身工具）的 Landlock 允许列表；不存在的路径会被跳过。
- `timeout_seconds`（默认 60）限制每次工具调用的时长；超时的调用会向 Agent 返回错误。
- `enabled: false` 保留该条目但不运行服务器。

运行时在 Agent 启动时、其 Landlock 和 seccomp 规则就位后，启动每个已启用的服务器，作为通过 stdin 和 stdout 使用 MCP 通信的子进程；除非 `env` 另行设置，其 `HOME=/nonexistent`，并在 Agent 网络经由出站代理时设置代理变量。运行时列出服务器的工具，并以 `mcp__<server>__<tool>` 注册每一个，附带服务器提供的描述和输入 schema；子 Agent 也会获得这些工具。只使用工具，不使用资源或提示词，且只使用结果中的文本：其中的图片、音频和资源会变成一行占位文字。启动失败或无法列出工具的服务器会被跳过，并在 Agent 日志中记录警告，Agent 照常启动；服务器写到 stderr 的内容也会进入该日志。钩子规则可以用前缀指代某个服务器的工具，例如 `mcp__github__*`（参见[工具钩子](#tool-hooks)）。

Agent 的设置页有一个 **MCP servers** 区域：`can_use_mcp`、带开关和删除按钮的服务器列表，以及添加服务器的表单（名称、从已安装中选择的二进制文件、以空格分隔的参数、读写和只读路径）。`env` 和 `timeout_seconds` 需通过 API 设置：`GET /agents/{id}/mcp` 返回 `can_use_mcp`、`servers` 和 `available_binaries`，`PATCH /agents/{id}/mcp` 接受 `can_use_mcp` 和 `servers`，后者整体替换 Agent 的服务器列表。两者都写入 Agent 的策略覆盖配置，每次修改都以 `agent.mcp.updated` 记入审计。修改在 Agent 下次启动时生效。在多用户模式下，Agent 只运行其所有者角色中以相同名称列出的服务器，并使用角色中的设置，因此团队管理员通过角色的 `mcp_servers` 授予服务器（参见[策略](#policy)）。

<a id="skills"></a>
## 技能

技能是 Agent 工作区中的一个目录 `skills/<name>/SKILL.md`：先是包含 `name` 和 `description` 的 YAML front matter（两者必填，可选 `version` 和 `triggers`），然后是不超过 100 KB 的 Markdown 正文。

```markdown
---
name: code-review
description: Reviews a diff against the team's checklist.
triggers: [review, PR, diff]
---

Look at the diff first. Check that every change has a test.
```

Agent 启动时，运行时读取每个 `skills/*/SKILL.md` 并在 Agent 的数据库中建立索引；无法解析的文件会被跳过并给出警告。对于每条消息、每次心跳和每个 cron 任务，与其文本最匹配的三个技能（基于名称、描述和正文的关键词搜索，配置了 `[embedding]` 时再结合向量搜索）会被加入该轮的系统提示词，仅对该轮有效，位于 "Skills available right now" 标题下。这些匹配计为使用次数。

- **`skill_manage`** 让 Agent 创建、更新、删除、列出、置顶和取消置顶自己的技能。它写入 `skills/<name>/SKILL.md`（名称由小写字母、数字、`-` 和 `_` 组成，最长 64 个字符）并立即建立索引，因此新技能从下一条消息起即可被匹配；删除操作会移除目录及其索引条目。Agent 在拥有 `permissions.can_install_skills`（默认开启）时获得该工具；没有该权限时，工作区中的技能仍会被读取。
- **手动安装技能**：将技能目录复制到工作区的 `skills/` 中（默认为网关所属用户的 `~/.local/share/havn/agents/<agent-id>/workspace/skills/`，位于 `workspace_root` 下），将其属主设为工作区的属主（`chown -R --reference=<workspace> <workspace>/skills/<name>`），然后重启 Agent（`havn agent stop <agent-id> && havn agent start <agent-id>`）。手动删除的技能目录仍会留在索引中：请改为让 Agent 通过 `skill_manage` 删除。
- **整理器。**每 7 天，在 Agent 已有 2 小时没有收到消息时，整理器会执行一轮：将 90 天未使用且使用次数少于 5 次的技能归档，移动到工作区的 `.archive/<name>/`；然后请模型给出合并重叠技能的方案，写入每个合并后的技能，并归档被合并的技能。置顶的技能不参与这两步。每轮都会在工作区的 `.curator/` 中写入一份报告。
- Dashboard 中 Agent 的**技能**页面列出其活跃和已归档的技能及其使用次数和最近使用时间，可以置顶和取消置顶（`POST /agents/{id}/skills/{name}/pin` 和 `/unpin`，需 Agent 正在运行），并展示整理器的报告。

<a id="heartbeat"></a>
## 心跳

网关每隔 Agent 配置中的 `config.heartbeat.interval_seconds`（默认 1800，即 30 分钟；最小 60）向每个运行中的 Agent 发送一次心跳，第一次在 Agent 连接后经过一个间隔发送。Agent 设置页以分钟为单位设置该值，也可以通过 `PATCH /agents/{id}` 并传入 `{"config": {"heartbeat": {"interval_seconds": 3600}}}` 设置；网关每次检查时都会读取它，因此修改无需重启。

收到心跳时，Agent 会读取工作区中的 `HEARTBEAT.md`（可在设置页编辑），每次都重新读取，因此编辑会在下一次心跳时生效。文件不存在或为空时，心跳什么也不做，也不产生模型调用。否则，模型会被要求先保存它了解到的关于所有者的稳定新事实（`memory_remember`），再按 `HEARTBEAT.md` 执行，若两者都没有产生需要告知的内容，则回答 `HEARTBEAT_OK`。

- 心跳使用 Agent 的系统提示词、近期事件和匹配的技能，以及心跳自身的历史运行记录（最近 40 条消息，属于它自己的独立线程，而非任何聊天）。
- 它拥有 Agent 的工具，但不包括 `conversation_search` 以及 `context_toolsets.heartbeat.disabled` 中的工具，最多运行 `tool_rounds.subagent` 个回合，并可以所有者身份修改记忆。钩子会要求询问的调用会被拒绝，且它不能修改定时任务。
- 以 `HEARTBEAT_OK` 结尾的回复不会投递。其他回复会保存在所有者 Web 聊天中名为 Heartbeat 的聊天里，并按 `config.heartbeat.deliver`（设置页也可设置）指定的位置发送：
  - `"last"`（默认）：所有者最近一次与该 Agent 对话的地方，即 Web 聊天或其已连接的某个聊天应用账号（设置页的 Agent 聊天应用区域）。若所有者从未在任何地方发过消息，则为 Web 聊天。
  - `"web"`：仅 Web 聊天。
  - 某个渠道，例如 `"telegram"`：通过 Agent 的机器人发送到所有者在该渠道上已连接的账号。若没有连接账号，则为 Web 聊天。

  Agent 的机器人不能向其发送消息的聊天应用账号（尚未准入该机器人的 `allowlist` 账号）会被排除。

  在 Web 聊天中，回复会出现在所有者已打开的标签页中的 Heartbeat 聊天里。在聊天应用中，回复也会保留在那里的聊天中，因此在那里的回复会接续它。其他人与该 Agent 的聊天永远不会收到心跳回复。

<a id="webhooks"></a>
## Webhook

Webhook 是一个地址，供其他系统（告警、CI 构建、表单、GitHub）调用，让 Agent 根据其发送的内容采取行动。Agent 所有者在其设置页（**webhooks**；`POST /agents/{id}/webhooks`，携带 `name`、`instructions` 和 `deliver`）创建 Webhook，每个 Agent 最多 20 个：每个 Webhook 有一个 id 和一个密钥，密钥只显示一次。调用地址为网关 `listen` 地址上的 `POST /hooks/<id>`，因此若调用方在主机之外，请将该路径置于反向代理之后；与 API 不同，它不限于主机自身的名称。调用须以下列方式之一携带密钥，按以下顺序检查（以最先出现的一种为准），否则返回 401；Webhook 不存在时同样返回 401：

- `Authorization: Bearer <secret>`（若代理设置了自己的 `Authorization`，其余方式将不可见），或 `X-Havn-Token: <secret>`；
- `X-Hub-Signature-256: sha256=<hex>`（即 GitHub 的发送方式，在 GitHub 中将该密钥设为 Webhook 的 secret），或 `X-Havn-Signature-256`：以密钥对请求体计算的 HMAC-SHA256。

请求体为文本（UTF-8，最大 256 KB，超出返回 413；Agent 获得前 32,000 个字符），类型不限：JSON、表单、一行纯文本均可。每个 Webhook 每分钟接受 30 次调用；超出返回 429。网关重启期间，调用返回 503。调用通过校验后返回 202 及其 `run_id`；随后 Agent（未运行时会被启动）在全新的上下文中处理它，使用子 Agent 的工具回合上限和 `cron` 上下文的工具，但没有 `schedule`、`set_timezone`、子 Agent 或向其他 Agent 提问的能力（来自外部的内容不应在调用结束后继续存留，也不应在之后以所有者身份运行）；Agent 会被告知所有者要求它如何处理调用（`instructions`），并以标记为外部来源的 JSON 字符串形式获得调用携带的内容。调用携带的内容被视为来自所有者以外的人（参见“他人对话中的工具”）：读取类工具返回的内容（包括 `memory_search`）以及执行操作的调用需等待所有者批准——所有者会在其与该 Agent 每个机器人的聊天中以及 Web 聊天中被询问，并被告知在那里选择"始终"将对该 Webhook 之后的所有调用生效；所有者保存的事实不会提供给它，也不会有任何内容存入记忆。同一个 Webhook 的调用逐个处理，最多 5 个排队等待（超出的会被丢弃，其聊天中会说明），一个 Agent 最多同时处理 4 个 Webhook 调用，其余的等待。等待所有者批准的调用在此期间保留其排队位置（审批最多等待 10 分钟）。

每个 Webhook 在所有者的 Web 聊天中都有一个独立的对话（"Webhook: <name>"），其中保存每次调用（取前 4,000 个字符，以来自外部的引用形式呈现）以及 Agent 对它的处理结果，后者作为 Agent 的消息，而不会作为所有者的消息。该对话不会被所有者的 `conversation_search` 搜索到，不算作所有者最近一次发言的位置，绝不会被定时任务当作所有者的对话，其摘要也不会向记忆保存任何内容，即便所有者在其中发言也是如此。`deliver` 还会像心跳那样发送结果（参见"心跳"）：`web`（默认）只保留在该对话中，`last` 发送到所有者最近一次发言的会话，填写某个渠道名则发送给该渠道上的所有者。以 `[SILENT]` 开头的回答只保留在对话中，不会发送。创建、修改（`PATCH /agents/{id}/webhooks/{hook}`，可改其 `instructions` 和 `deliver`）、调用和删除 Webhook 分别记入审计日志 `webhook.created`、`webhook.updated`、`webhook.called` 和 `webhook.deleted`。由于签名调用需要用密钥校验，密钥按原样保存在网关的数据库中；如需更换，请删除该 Webhook 并重新创建一个。

<a id="configuration"></a>
## 配置

网关配置位于 `~/.config/havn/config.toml`：

```toml
listen = "127.0.0.1:8080"
db_path = "~/.local/share/havn/havn.db"
```

Agent 可用的模型列在 `[[llm.models]]` 中，每项写作 `provider:model`。其中 provider 为 `anthropic`、`openai`、`openrouter`、`gemini` 或 `[llm.endpoints]` 中的某个端点（见下文），决定所用的 API 和凭据；模型名是第一个 `:` 之后的全部内容。未设置自己 `model` 的 Agent 使用 `[llm] default_model`，未设置时使用列表第一项，并随其变化而变化。未列出的模型会被拒绝，因此列表为空时 Agent 无法聊天。`havn model add/remove/default` 和 Dashboard 的模型页面会通过网关编辑此列表，并写回该文件（其他键和注释保持不变）；开启 `trust_header` 时，只能通过该文件修改：

```toml
[llm]
default_model = "openrouter:deepseek/deepseek-v4.1-flash-1017"   # 可选

[[llm.models]]
id = "anthropic:claude-sonnet-4-6"
label = "Sonnet 4.6"            # 可选，在 Dashboard 中显示

[[llm.models]]
id = "openrouter:deepseek/deepseek-v4.1-flash-1017"
max_output_tokens = 32000       # 可选，该模型 max_tokens 的上限
```

模型的 `price` 给出其 token 价格（美元 / 百万 token），供用量页面使用：`price = { input = 3.0, output = 15.0, cache_read = 0.3, cache_write = 3.75 }`（缓存读写未填写时按输入价格计）。Dashboard 的模型页面也可以设置、修改和清除它（开启 `trust_header` 时只能通过文件修改）。价格不会自动获取：没有价格的模型只显示 token 数。费用按查看页面时设定的价格计算，因此修改价格也会改变过去的费用。预算仍以 token 计。

只有模型条目说明了思考方式时，才会要求模型思考，因为各提供方和模型的方式不同，且没有维护相应的列表：`thinking = { style = "toggle" }` 为开或关；`style = "effort"` 接受 `levels`（如 `["low", "medium", "high"]`，按模型自身的叫法）；`style = "budget"` 接受 `budgets`，即 token 数（不适用于 `openai:` 模型，也不适用于 OpenAI API 端点的模型）。`default` 是对话未作选择时的取值（未设置则为关），`mandatory = true` 则为始终思考的模型去掉"关"选项。网关按各 API 的方式发出请求：Anthropic 使用自适应思考，档位通过 `output_config.effort` 指定（当前 Claude 模型接受 `low`、`medium`、`high`、`xhigh`、`max` 等 `effort` 档位；`budget` 方式仅适用于 Haiku 4.5 及更早的模型，当前模型会拒绝），以摘要形式显示，且不带 temperature；OpenAI 兼容接口使用 `reasoning_effort`；OpenRouter 使用 `reasoning`；Gemini 使用 `thinkingConfig`。在一次回复中，推理内容会随每次工具调用一并回传，这是 Anthropic 和 DeepSeek 的要求；模型返回的推理在 Web 聊天中折叠显示。"关"对 Anthropic 和 OpenAI 兼容端点不发送任何参数，因此默认会思考的模型仍会继续思考：请为这类模型设置 `mandatory = true` 及其最低档位（`low`，或其 API 支持时的 `minimal`/`none`）；无法停止思考的 Gemini 模型（2.5 Pro、3 Pro）同理，其"关"即 `thinkingBudget = 0`。可用 `havn model thinking <id> effort:low,medium,high --default medium`（或 `toggle`、`budget:2048,8192`、`none`）或在模型页面设置，并用 `havn model check <id>` 或页面上的检查按钮测试：它会向模型发送一个很短的问题（使用调用者的凭据，最多 4096 token），并报告模型是带推理回答、不带推理回答还是拒绝了请求；开启 `trust_header` 时，只检查已列出的模型，且按列出的配置检查。Web 聊天的输入框可为单个对话选择模型和思考方式（Agent 所有者或账户管理员也可在对话中使用 `/think`）；对没有 `thinking` 的模型，思考菜单显示为灰色；对话模型不支持的选择会回退到其默认值。生成标题、摘要和整理记忆时会要求模型不思考。

```toml
[[llm.models]]
id = "openrouter:deepseek/deepseek-v4.1-flash-1017"
thinking = { style = "effort", levels = ["low", "medium", "high"], default = "medium" }

[[llm.models]]
id = "anthropic:claude-sonnet-5-5"
thinking = { style = "effort", levels = ["low", "medium", "high"], default = "medium", mandatory = true }
```

聊天中发送的图片（Agent 收件箱中的 PNG、JPEG、GIF、WebP 或 HEIC）会根据当前轮次模型的 `vision` 设置展示给模型。超过 3.75 MB 或单边超过 8000 像素的图片，以及 HEIC 照片，会以网关在收到时生成的副本展示：单边最多 2048 像素，按照片方向旋转，sRGB，JPEG 格式（透明图片则为 PNG），与原图一起保存为 `.<file>.shown`。原图保留在收件箱中供 Agent 的工具使用。无法生成副本时（文件损坏、超过 6700 万像素、非 HEVC 编码的 HEIC、AVIF），模型会收到一行说明图片未展示。`vision = true` 时始终展示，`vision = false` 时从不展示：模型会收到一行说明它看不到该图片，Agent 会如实告知用户。未设置时则会尝试展示。当提供方直接拒绝包含图片的请求（4xx，但不包括认证、付费或限流类拒绝）时，请求会以说明文字替代每张图片再发送一次；若仍被拒绝，Agent 收到第一次的拒绝结果。若第二次请求成功，且拒绝信息表明该模型不接受图片（如 OpenRouter 返回 404 "No endpoints found that support image input"，或 OpenAI 返回 "image_url is only supported by certain models"），则此后该模型直接收到说明文字，直到其条目变更或网关重启，网关还会记录一条警告，建议设置 `vision = false`。其他原因的拒绝（图片损坏、上下文过长）不会被记住。可用 `havn model add <id> --vision` 或 `--no-vision`，或在模型页面设置。

从没有模型列表的旧版网关升级后首次启动时，Agent 和策略中的模型名会被改写为 `provider:model`，provider 按旧版网关从名称推断的方式确定；若 `config.toml` 中还没有 `[llm] models`，现有 Agent 使用的模型会被加入其中（若存在未指定模型的 Agent，则以旧版网关为其分配的 `anthropic:claude-opus-4-6` 打头）。全新安装或没有 Agent 的升级安装以空列表启动：首次聊天前请用 `havn model add` 添加模型。

无论 Agent 请求多少，每次 LLM 调用的 `max_tokens` 都受 `[llm] max_tokens_ceiling` 限制（默认 128000，即已知模型中最大的输出上限；在 `havn gateway restart` 时生效）。进行中的调用会在其计费的各上限中预留至多这么多输出 token，因此在共享凭据设置了较小上限时，请调低该值，以免某个用户的大 `max_tokens` 占用其他所有人的剩余预算：

```toml
[llm]
max_tokens_ceiling = 16000   # 0 会被拒绝
```

如需将某个提供方的调用发往使用相同 API 的其他端点（内部 LLM 网关，或测试中的 mock），请在 `[llm.base_url]` 下设置其基础 URL。提供方的凭据也会发送到那里，因此凡不在网关主机上的地址都应使用 `https`。只有运维者的配置可以设置它，Agent 和 API 用户不能，且在 `havn gateway restart` 时生效。各键及默认值为 `anthropic`（`https://api.anthropic.com`）、`openai`（`https://api.openai.com/v1`）、`openrouter`（`https://openrouter.ai/api/v1`）和 `gemini`（`https://generativelanguage.googleapis.com/v1beta`）：

```toml
[llm.base_url]
anthropic = "https://llm-gateway.internal.example"
```

本地模型服务（Ollama、llama.cpp 的 server、vLLM、LM Studio）或任何其他 OpenAI / Anthropic API 服务，可在 `[llm.endpoints]` 下以自定义名称添加，其模型随后以该名称作为 provider（如 `ollama:qwen3`，用于 `[[llm.models]]`、`havn model add`、模型页面和 Agent 配置），内置提供方保持不变。`api` 为 `openai`（Chat Completions，地址为 `<base_url>/chat/completions`，发送 `max_tokens`，思考通过 `reasoning_effort` 指定，即按档位或开/关）或 `anthropic`（Messages，地址为 `<base_url>/v1/messages`）。若需要密钥，则存为 `llm:<name>`（`havn credential add llm:ollama <key>`，或在凭据页面列表中选择该端点），且只查找这个名称，不查找不带前缀的名称；调用者没有该密钥时，请求不带密钥发送，但仍计入 Agent 的预算并显示在用量页面。名称以小写字母开头，后跟最多 31 个小写字母、数字、`-` 或 `_`，且不能与内置提供方重名。`base_url` 为 http(s) URL：明文 `http` 只能用于网关主机或你自己的网络，因为提示词、回复和密钥都会以明文传输（对其他主机使用明文 `http` 时网关会记录警告）。端点仅由运维者配置，在 `havn gateway restart` 时生效：在此之前，引用了启动后新增、删除或修改的端点的模型列表不会重新加载（网关会记录原因），模型页面和 `havn model` 也不会保存运行中的网关无法调用、或 config.toml 无法据以启动的配置：

```toml
[llm.endpoints.ollama]
api = "openai"
base_url = "http://127.0.0.1:11434/v1"

[llm.endpoints.lan-vllm]
api = "openai"
base_url = "http://192.168.1.20:8000/v1"

[[llm.models]]
id = "ollama:qwen3:8b"
context_window = 32768          # 与服务启动时的设置一致
```

<a id="web-search"></a>
### 网页搜索

Agent 可使用由网关代为执行的 `web_search` 工具，因此没有网络出口的 Agent 也能使用，搜索密钥也不会进入沙箱。该工具返回标题、URL、摘要和发布日期，Agent 会被要求以纯链接形式引用 URL。在默认的 `provider = "auto"` 下，每个 Agent 所有者的搜索使用以下各项中第一个其拥有密钥的后端：

1. Brave、Tavily 或 Exa 的密钥，以凭据形式添加为 `search:brave`、`search:tavily` 或 `search:exa`（`havn credential add search:brave <key>`）；
2. 其 OpenRouter LLM 密钥，在列出了 `openrouter:` 模型时，通过 OpenRouter 基于 Exa 的网页搜索（每次约 $0.007，消耗 OpenRouter 额度）。搜索使用 `[search] openrouter_model`；未设置时，若默认模型是 OpenRouter 模型则用默认模型，否则用列表中第一个 OpenRouter 模型；
3. 位于 `searxng_url` 的 SearXNG 实例，无需密钥（需启用其 `json` 格式）。

若某个后端失败（配额用尽、服务中断、密钥被拒），会尝试下一个。一个都没有时不提供该工具；`havn doctor` 和 `GET /search` 会显示所用的后端。每次搜索都会连同查询内容记入审计日志 `web.search`，同一用户的所有 Agent 合计每分钟最多搜索 `per_user_per_minute` 次。策略权限 `can_search_web`（默认开启）可移除该工具。在 `havn gateway restart` 时生效：

```toml
[search]
provider = "auto"            # 或 off、brave、tavily、exa、openrouter、searxng
searxng_url = "http://127.0.0.1:8888"
max_results = 5              # 1 到 10；Agent 可自行指定数量
per_user_per_minute = 20     # 0 表示不限

[search.base_url]            # 可选：brave、tavily、exa
brave = "https://search-proxy.internal.example"
```

<a id="reading-web-pages"></a>
### 读取网页

Agent 还可使用由网关代为执行的 `web_fetch` 工具：Agent 提供 URL 和想了解的内容，网关读取页面并通过一次模型调用基于页面作答，Agent 得到的是答案而非页面本身。没有网络出口的 Agent 也能使用，长页面也只占用 Agent 上下文中答案的篇幅。

- 网关会把 `http` 升级为 `https`，拒绝包含用户名或密码的 URL，并只连接公网地址（在 DNS 解析后及每次重定向时检查，范围与 Agent 防火墙相同，另加网关主机自身的地址）。同一主机内的重定向会被跟随；跳转到其他主机的重定向会返回给 Agent，由它再发起一次调用读取。
- HTML 转换为 Markdown 并将链接改为绝对地址，PDF 转换为文本，文本和 JSON 按原样读取；GBK 等旧字符集会被解码。页面最多读取 10 MB，答案每次覆盖页面的 100,000 个字符；结果会告知 Agent 继续读取的偏移量。转换后的页面按 Agent 缓存 15 分钟。
- 页面在网关二进制的一个子进程中解析，该子进程在异步运行时启动之前创建，没有网络（独立的网络命名空间）、不能获取新权限、地址空间为 1 GB、CPU 时间 20 秒、不能写文件，30 秒后被终止；网关以 root 运行时，子进程以 `nobody` 身份运行。以其他用户运行的网关需要非特权用户命名空间来创建子进程，否则 `web_fetch` 会失败；网关在启动时检查这一点并记录警告。子进程仍可读取其用户有权读取的文件，因此请保持网关文件的私密性。页面 URL 通过 stdin 传给子进程，而不是放在参数中。导致解析器崩溃或卡住的页面只会让该次调用失败。转换后的文本每页最多保留 8 MB，15 分钟缓存最多占用 128 MB。
- 若 Agent 的 `allowed_models` 允许且其所有者拥有该提供方的凭据，答案由 `[fetch] model` 生成，否则使用 Agent 自身的模型，并像 Agent 自身的调用一样计费：计入 Agent 所有者的凭据和 Agent 的 token 预算。整次调用限时 75 秒。
- Agent 只能读取网关看到来自其模型之外的 URL：用户发给它的消息中（WebChat 和渠道）、`web_search` 结果中、以这种方式读取的页面上的链接，或重定向目标，有效期为网关启动后的 24 小时内。模型自身的文本和其他工具的输出（shell 命令、文件、MCP 服务器）都不会新增可读 URL，因此模型无法把 Agent 中的数据塞进它自选的 URL。策略 `network_policy.allowed_domains` 中的域名（一个域名同时匹配其子域名）无需满足上述条件即可读取，路径和查询不限；仅因域名被允许而读取的页面上的链接和重定向不会被记录，因为这类页面可能回显模型放进 URL 的内容。`blocked_domains` 中的域名永远不会被读取。被拒绝的 URL 不会被回显。
- 每次通过策略和速率检查的调用都会连同 URL 记入审计日志 `web.fetch`，同一用户的所有 Agent 合计每分钟最多读取 `per_user_per_minute` 个页面。策略权限 `can_fetch_web`（默认开启）可移除该工具；升级时，对已关闭 `can_search_web` 或 `can_access_network` 的角色和 Agent 覆盖配置，会一并关闭此权限。在 `havn gateway restart` 时生效：

```toml
[fetch]
model = "openrouter:deepseek/deepseek-v4.1-flash-1017"   # 可选，须为 [[llm.models]] 之一
per_user_per_minute = 30     # 0 表示不限
```

允许和屏蔽的域名可在多个层级设置，且只会叠加：Agent 获得所有层级的条目，屏蔽优先于允许。

- 运维者用 `[network] blocked_domains = ["example.com"]` 为所有 Agent 屏蔽域名（重启后生效）。
- 团队管理员在角色策略中设置 `network_policy.allowed_domains` 和 `blocked_domains`，作用于成员的 Agent。属于多个角色的成员会获得所有角色的屏蔽域名。
- 每个用户可为自己的所有 Agent 维护列表（`GET` 和 `PUT /me/domains`，或 Agent 设置页面），每个 Agent 也有自己的列表（`PATCH /agents/{id}/web`，带 `allowed_domains` 和 `blocked_domains`）。二者都不能移除上级设置的条目。

条目为 `example.com`（涵盖其子域名）或 `*.example.com` 这样的域名，使用 ASCII（国际化域名用 punycode），至少包含一个点；每个列表最多 500 条。列表对 `web_fetch` 立即生效：网关在每次调用时解析。若无法读取某用户自己的列表，该用户的 Agent 在恢复之前都无法使用 `web_fetch`。

<a id="time-zones"></a>
### 时区

每一轮对话都会告知 Agent 当前的日期、时间和星期，采用其所有者的时区，因此"今天"、"明天 9 点"和星期几都能正确理解。未设置时区的用户使用网关主机的时区（`TZ`、`/etc/timezone` 或 `/etc/localtime`，否则为 UTC）。用户可在 Agent 设置页面或通过 `PUT /me/timezone`（`{"timezone": "Asia/Shanghai"}`，或 `null` 表示使用主机时区）设置时区；用户说明自己所在位置时，Agent 也可用其 `set_timezone` 工具设置。修改会立即作用于所有者正在运行的 Agent。

所有者策略包含 `can_schedule_cron` 的 Agent 可使用 `schedule` 工具，用户因此可以在聊天中要求提醒和周期任务（"17:00 提醒我给 Anna 打电话"、"每个工作日 9 点总结新闻"）。Agent 可创建、列出、暂停、恢复和删除自己的任务：可在某个时间执行一次（按用户时区的本地时间，或带偏移量），或在若干分钟后执行，或按以文字或 cron 表达式描述的日历计划执行，最短间隔 5 分钟，每个 Agent 最多 20 个活动任务。在渠道会话（Telegram）中创建的任务会将结果发送到该会话，并执行与 Agent 自身消息相同的白名单检查；在 Dashboard 聊天中创建的任务会显示在其已打开的标签页中。若某次运行失败，其所在会话（或 Dashboard 聊天）每天（UTC）会收到一次通知。任务只在用户的轮次中变更，不会在定时运行或心跳中变更；在渠道会话中，Agent 只列出和修改该会话的任务。已运行的一次性任务显示为已完成，不能恢复；已完成的一次性任务 7 天后删除。每次变更都记入审计日志 `agent.schedule`，Agent 设置页面会列出各任务及其下次和上次运行时间，可立即运行、暂停或删除。**立即运行**（`POST /agents/{id}/cron/{job_id}/run`，审计为 `cron.run_now`）会立刻运行任务，无论是否暂停，且跳过其唤醒检查：周期任务保持原计划，尚未运行的一次性任务就此完成（立即运行过的提醒不会在原定时间再次运行）。其结果的去向与定时运行相同。需要 Agent 正在运行、所有者策略允许 `can_schedule_cron`，且该任务没有仍在进行的运行（15 分钟内未产生结果的运行不再计入）。每次运行的输出文件以其时间和运行 id 命名。

Cron 任务（`POST /agents/{id}/cron`）也可以按某个时区的日历时间运行，使用五段式 cron 表达式或文字描述：`{"cron": "0 8 * * 1-5"}`，或 `{"every": "daily 08:00"}`（还有 `weekdays`、`weekends`、`weekly mon 10:00`、`monthly 1 09:00`、`hourly :15`）。时区为给定的 `timezone`，否则为任务创建时所有者的时区；任务会一直保留该时区。夏令时切换前后时间保持为本地时间：被切换跳过的时间会在切换后立即运行，被重复的时间只运行一次。网关停机期间到期的任务会在恢复后运行一次；此前错过的其他运行会被跳过。Agent 的 `set_timezone` 调用会记入审计日志 `user.timezone.updated`，每个用户每分钟最多 3 次；子 Agent 和跨 Agent 查询不提供该工具。

配置修改会实时生效：`allowed_origins`、`[llm] default_model`、`[[llm.models]]` 和 `[channels]` 就地应用，其他修改（`listen`、`db_path`、各种路径、`[[bindings]]` 等）会记录为需要 `havn gateway restart` 才能生效——网关从不自行重启。在 `[channels]` 中，谁可以联系机器人（`dm_policy`、`allow_from`、`group_policy`、`allow_groups`、`admins`、`bot_username`）对下一条消息生效；新增或移除账户，或修改其 `adapter_token_ref`，会记录为需要重启，因为适配器的会话是凭其令牌接入的（已修改账户的策略仍会立即生效）。`[workspace] browse`、`[channel_texts]`、`[transcription]` 和 `[attachments]` 也会就地应用。每个 Agent 的资源限制来自 Agent 的角色策略，而不是全局配置块。

如需在非回环地址上暴露服务，请配置反向代理并启用 trust-header 模式：

```toml
listen = "0.0.0.0:8080"

[trust_header]
enabled = true
# 允许设置身份头的对端（即你的代理发起连接的地址）；
# 其他对端一律返回 403。为空表示仅限回环地址。
allowed_proxies = ["192.0.2.10/32"]
# 携带用户身份的请求头（默认 "X-User-ID"）。
header = "X-Forwarded-Email"
```

在 trust-header（多用户）模式下：

- 运维者创建团队（`havn team add --name <name> --admin <user-id>`）。团队角色定义每个成员的 Agent 能做什么。
- 团队管理员可以为其团队的角色设置任何策略，包括挂载主机路径的 MCP 服务器。请像任命运维者一样任命他们。
- 每个 Agent 的策略覆盖只能收窄成员的角色策略。
- 网关级的 embedding 配置只能在 `config.toml` 中修改。

Cron 任务可以带有 `wake_check` 脚本。它以专用的 `havn-wakecheck` 用户（由 `install.sh` 创建；网关须以 root 运行才能切换到该用户）在主机上运行，环境变量精简到最少。没有该用户时会跳过脚本，任务总是被唤醒。

<a id="browsing-an-agents-workspace"></a>
### 浏览 Agent 的工作区

Agent 的所有者可以在 Dashboard（Agent 的 **workspace** 页面）中浏览其工作区中的文件：目录、就地显示的文本和图片，以及可下载的任意文件。该功能只读，且默认关闭，需网关允许：

```toml
[workspace]
browse = true   # 默认 false
```

Dashboard 的 **settings** 页面可开启或关闭它并写入 config.toml（开启 `trust_header` 时只能通过文件修改）；修改立即生效。开启时，每个 Agent 都有该页面，除非其所有者在 Agent 设置页面关闭（`PUT /agents/{id}/workspace/access`）；关闭时，所有 Agent 都没有该页面，Agent 的设置也无法开启它。只有所有者可以浏览，且不能从其他站点的页面访问。工作区中的链接会列出其指向但从不跟随；Agent 的数据库和 `.havn` 从不列出或发送（按名称判断：Agent 复制到别处的副本会像普通文件一样显示，因为 Agent 本来就能读取它们）；超过 50 MB 的文件不会发送；图片就地显示，其他内容均作为下载发送。变更记入审计日志 `workspace.browse.changed` 和 `agent.workspace_browse.changed`。

<a id="multi-user-mode-behind-an-sso-proxy"></a>
### 在 SSO 代理之后运行多用户模式

网关从一个请求头 `trust_header.header`（默认 `X-User-ID`）读取用户身份，且只接受来自 `allowed_proxies` 对端的连接上的该头。它按以下顺序将该值解析为 havn 用户：

1. 当 `match_user_id` 开启且该值是规范形式（小写、带连字符）的内部用户 id 时：对应该用户。`match_user_id` 对 `X-User-ID` 默认开启，对其他请求头默认关闭。只有在用户无法在身份提供方自行选择名称时，才应为其他请求头开启它，否则用户可能把别人的内部 id 用作自己的名称。
2. 外部 id 与该值相等的用户。匹配是精确且区分大小写的：`Alice@example.com` 和 `alice@example.com` 是不同的身份，因此请严格按代理发出的值配置。外部 id 为可见 ASCII 字符（允许中间有空格），不含逗号，且不能是 UUID。

其他情况一律返回 403；用户不会被自动创建。请求中该头出现多次时会被拒绝。请求头名称不能是 HTTP 已在使用的名称（`Host`、`Cookie`、`Authorization`、`Connection` 等）。

使用代理将发送的值来创建用户：

```sh
havn user add --external-id alice@example.com --display-name "Alice"   # 自动生成内部 id
havn user add 0190a3c4-... --display-name "Bob"                         # 仅内部 id（X-User-ID 部署）
havn user list                                                          # id、名称、外部 id
havn user set-external-id <existing-id> ops@example.com                 # 例如单用户引导时创建的用户
havn user set-external-id <existing-id> --clear
```

将 SSO 代理指向 `dashboard_listen`。其 `/api` 与 `listen` 提供相同的 API，并遵循同样的规则：只从 `allowed_proxies` 对端读取身份头，因此代理与网关在同一主机时，`allowed_proxies = ["127.0.0.1/32"]` 即可覆盖。UI、`/api` 和 `/ws` 都要经过 SSO 代理路由，不要留未认证的路径：代理跳过的路径会把客户端发送的任何请求头原样放行。Web 聊天的 WebSocket 只接受来自页面自身源（其 scheme 和 `Host`）或 `allowed_origins` 中所列源的连接，因此请让代理转发 `Host` 头（oauth2-proxy 默认如此，nginx 需 `proxy_set_header Host $host`），或在其中列出 Dashboard 的公开源。

<a id="oauth2-proxy"></a>
#### oauth2-proxy

oauth2-proxy（当前的 v7 版本）位于 Dashboard 之前，全部部署在同一主机上，oauth2-proxy 之前再放一个终止 TLS 的代理。`--pass-user-headers`（默认开启）使 oauth2-proxy 在转发到上游的每个请求（包括 WebSocket 升级请求）中替换 `X-Forwarded-User` 和 `X-Forwarded-Email`：

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

`X-Forwarded-User` 携带 OIDC 的 `sub` claim。许多提供方（Keycloak、Authentik）在此使用 UUID，而 UUID 不能作为外部 id，因此请以邮箱作为用户标识。若 oauth2-proxy 作为 nginx 的 `auth_request` 端点运行，而不是位于 Dashboard 之前，请使用 `--set-xauthrequest` 启动它，并像下一个示例那样将其 `X-Auth-Request-Email` 响应头复制到转发的请求中。

<a id="nginx-auth_request-with-authelia"></a>
#### nginx `auth_request` 配合 Authelia

nginx 终止 TLS，询问 Authelia 请求是否已认证，并将 Authelia 的 `Remote-User` 响应头复制到发往 Dashboard 的请求中。`proxy_set_header` 会替换客户端发送的任何 `Remote-User`：

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
havn user add --external-id alice --display-name "Alice"   # Authelia 用户名
```

同样的做法也适用于其他任何会把某个请求头替换为已认证用户名的代理。使用 Cloudflare Access 时，设置 `header = "Cf-Access-Authenticated-User-Email"`，并把 `allowed_proxies` 设为 `cloudflared` 发起连接的地址。havn 不校验 `Cf-Access-Jwt-Assertion` 令牌，因此源站必须只能经由 `cloudflared` 访问；此外，Access 的 bypass 策略会转发未经任何认证的请求。

<a id="channel-adapters"></a>
## 渠道适配器

Havn 在 `/api/v1/channel` 提供一套稳定的 HTTP + WebSocket 协议，供外部适配器守护进程接入。适配器运行在 havn 的信任边界之外，负责在聊天平台与 havn 的传输格式之间进行转换。

| 适配器 | 仓库 | 状态 |
|---------|-----------|--------|
| Telegram | [havn-channel-telegram](https://github.com/amplimit/havn-channel-telegram) | 可用 |
| Lark / Feishu | [havn-channel-lark](https://github.com/amplimit/havn-channel-lark) | 可用 |
| Discord | [havn-channel-discord](https://github.com/amplimit/havn-channel-discord) | 可用（需网关 0.2.11 或更高版本） |
| Slack | — | 计划中 |

自 v0.2.13 起，`sudo havn update` 会在更新网关之后更新本机上的适配器，每个适配器各自更新到其最新发布版本（见“升级”）。

Lark / Feishu 适配器通过 HTTP Webhook（`im.message.receive_v1`）接收 Lark 的事件，且只回复一对一聊天；配置方法及其支持的消息类型见其 README。

Discord 适配器会回复私信和服务器频道中的消息：在频道中提及机器人，或回复它的某条消息，会开启一个帖子（thread），即一个独立的对话；可以整个服务器放行，也可以逐个频道放行（见“群聊”）。机器人的配置方法见其 README。

Agent 只能通过 `[[bindings]]` 绑定给它的渠道账号发送消息。若账号设置为 `dm_policy = "allowlist"`（默认值），它只能给 `allow_from` 中的发送者或凭邀请码（见下文）放行的人发消息；若设置为 `dm_policy = "open"`，它可以给机器人在该平台上能触达的任何人发消息，因此 `open` 只应用于本就面向公众的机器人。在 `dm_policy = "allowlist"` 下，只有当某个群可以使用该机器人时（见“群聊”），它才能给这个群发消息；在 `open` 下，则可以发给机器人能触达的任何聊天，包括群。

**放行与连接。** 这是两件不同的事，所有者自己的账号需要的是后者：

- *放行*（`allow_from`、邀请码，或 `open` 账号上的任何人）：机器人会回复该发送者。无论对方是谁，Havn 都把他们视为 Agent 所有者以外的人。
- *连接*（关联到某个 Havn 账号；见“连接聊天账号”）：机器人以该 Havn 用户的身份回复他们；如果他们是 Agent 的所有者，则以所有者身份回复。

如果所有者自己的聊天应用账号只是被放行，那么在那里他会被当作别人：Agent 的 shell、文件和记忆工具需要等待他批准，而他只能在 Web 聊天中批准（见“他人对话中的工具”）；他在那里说的任何话都不会改变 Agent 的记忆；管理命令（`/model` 等）需要列入 `admins`，即便如此，需要等待所有者批准的调用（shell、文件、记忆、MCP）也只能在 Web 聊天中批准；这些聊天也不会与他的其他聊天一起被搜索，也不会列在 Web 聊天中。因此，即使机器人已经会回复你自己的账号，也请在 Agent 的设置页面（**chat apps** > **connect**）连接它：打开显示的链接，或给机器人发送 `/start <code>`（单独发送代码只能连接机器人尚未回复的账号；对已在回复的账号，代码会被转交给 Agent）。账号仍保持原先的放行状态。**chat apps** 区域只列出已连接的账号。`havn access list` 显示通过 `allow_from` 或邀请放行的发送者，已连接的会在旁边标注 `linked="<name>"`（`open` 账号的发送者不会列出）。

在每个对话中，Agent 一次只回复一条消息。在它回复期间发来的消息会排队，在这次回复之后按顺序回复（最多排队 20 条；渠道用户会被告知消息已排队）。回复进行中时，Dashboard 聊天里有一个停止按钮；在渠道对话中，`/stop`（或 `/stop@botname`）可以停止回复：Agent 会结束正在进行的模型调用或工具调用，并终止正在运行的 shell 命令所启动的进程。保留下来的回复是模型已经写出的文本，后跟一个空行和 `(stopped)`；如果模型还没写出任何内容，则只有 `(stopped)`。在 Dashboard 聊天中，这包括进行中的模型调用已经写出的内容：网关停止从提供商读取并保留这些内容，该调用的推理过程与回复的其他步骤放在一起，而它已经指定的工具调用不会执行。如果模型调用的文本当前没有展示给任何人（例如在渠道对话中），则不会被中途截断：它会执行完毕，其文本被丢弃，与 v0.2.13 之前的网关或 Agent 的行为相同。排队的消息会在之后得到回复。

每个用户可以与一个 Agent 进行多个聊天。Dashboard 聊天会在对话旁按最近时间顺序列出这些聊天：“new chat”会开启一个新聊天；聊天可以重命名或删除：删除时会一并删除其中的消息和在其中发送的文件（用户上传到 Agent 收件箱的文件，连同其文本、转写和为显示而生成的副本，除非其他聊天的某条消息也引用了它们；以及 Agent 发送的文件），但不会删除 Agent 保存到记忆中或写入其工作区其他位置的内容。**export** 会把当前屏幕上的聊天导出为 Markdown，或导出为 JSON（包含每条消息及其携带的文件名），导出内容来自屏幕上显示的分支，最多为最新的 500 条消息。在渠道对话中，`/new`（或 `/new@botname`）会开启一个新聊天，之前的聊天会被保留。每个聊天有各自的历史、队列、停止按钮和“approve for this conversation”批准记录；Agent 在某个聊天中安排的任务会在该聊天中报告，结果也保存在该聊天的历史中；在渠道中发送 `/stop` 会停止该发送者任一聊天中正在进行的回复。删除聊天会停止其回复并丢弃其排队的消息；Agent 的记忆由所有聊天共享。`conversation_search` 用于查找过去的消息：对 Agent 的所有者，范围是他发过消息的所有聊天（Dashboard，以及关联到他的聊天应用账号）；对其他人，仅限其本人的聊天；在群中，仅限该对话的聊天（若帖子是独立对话，则为该帖子自己的聊天），所有者也是如此。Agent 保存的每条事实要么是私有的，要么是公开的。关于其所有者的私有事实（他是谁、喜欢什么、在做什么）会出现在所有者在任何渠道上的每个私聊中，但绝不会出现在群聊中；在其他地方，Agent 只能通过 `memory_search` 获取它们，而该工具需要等待所有者审核，除非所有者已允许（见“他人对话中的工具”）。公开事实是任何能与机器人对话的人都可以知道的信息（名字、称呼方式、语言、公开的工作），它们会出现在每个聊天的提示词中，包括他人的聊天和群聊，`SOUL.md` 也是如此——即 Agent 设置页面在“bootstrap files”下编辑的文件（定时任务和 Webhook 只有 `SOUL.md`）：最多 100 条，最近使用的排在前面，每条截断为 2,000 个字符，在 4,000 个字符的总量内能放多少放多少（放不下的一条会被略过，后面的仍然会给出），且它们不会过期。Agent 保存的一切都是私有的。在所有者自己的私聊中，Agent 可以建议将某条事实设为公开（`suggest_public`）；只有所有者能在 Dashboard 上把事实设为公开：在保存该事实的那条回复下方，或在记忆页面（可按私有、建议公开和公开筛选）。Agent 只是重复一遍的事实保持原有可见性；Agent 对公开事实的修改会使其退回私有、建议公开的状态，直到所有者批准新文本；Agent 遗忘的事实不再显示。所有者无论朝哪个方向切换，都会以 `memory.visibility` 记入审计日志；而在事实公开期间别人读到的内容无法收回。以上规则针对的是 Agent 的记忆工具：它的文件工具可以修改它自己的文件（包括 `SOUL.md`），允许运行 shell 命令的 Agent 还能修改其数据库，因此在包含不可信内容的聊天中，文件和 shell 访问权限才是真正的边界。重新设为私有的事实，其完整的生命周期从那时起重新计算。`USER.md` 已不再读取：升级时，非空的 `USER.md` 会变成一条公开的 `identity` 事实（键为 `USER.md`），文件被重命名为 `USER.md.bak`（该名称已被占用时为 `USER.md.bak.1` 等）；如果读取或保存失败（会记录日志），则不会再次尝试，文件保持为 `USER.md` 且不被读取；Agent 的文件工具在任何聊天中仍可读取该文件，因此如果其中的内容不应留在工作区，请删除它。所有者在已关联的聊天应用账号上（群聊之外）发送的消息，会像 Dashboard 中的消息一样被记住。聊天功能出现之前的历史记录，成为了各个对话的第一个聊天。

在 Dashboard 聊天中，可以编辑消息，也可以重新生成回复。两者都会开启一个分支：该点之后的消息仍保存在它们自己的分支上，消息下方的 `‹ 2/3 ›` 用于在不同版本之间切换，Agent 只能看到屏幕上显示的分支。在渠道对话中，`/retry`（或 `/retry@botname`）会重新回复最后一条消息。分支不会撤销工具已经做过的事：已写入的文件、已发送的消息和已保存的记忆都会保留。

<a id="chat-titles"></a>
### 聊天标题

每个新聊天由模型根据其第一条消息命名，命名过程与 Agent 回复同时进行（渠道中也是如此）；在此之前，或者命名失败时，聊天以其第一条消息的开头命名；手动重命名过的聊天保留其名称。写标题的模型是 config.toml 中的 `[llm] title_model`，可以在 Dashboard 的模型页面设置，或用 `havn model title <provider:model>` 设置（小而便宜的模型就够用）；未设置时，由各 Agent 自己的模型来写。角色的策略可以指定另一个模型（`"chat_titles": {"model": "..."}`），或关闭标题功能（`"chat_titles": {"enabled": false}`；在多用户模式下，Agent 无法重新开启它；拥有多个角色的用户，只要其中任一角色开启即可使用），Agent 的设置页面也可以为该 Agent 关闭此功能。只包含文件的消息，以文件名作为其聊天的标题。更改在 Agent 下次启动时生效；如果期间标题模型被移除，则改用 Agent 自己的模型。

<a id="tools-in-other-peoples-conversations"></a>
### 他人对话中的工具

Agent 的记忆、工作区和机器都属于其所有者。在与其他人的对话中（所有者在聊天应用账号上放行的人，或 `open` 账号上的任何人），近期事件不会放入提示词，而能触及这些内容的工具需要等待所有者：

| 工具 | 何时询问所有者 |
|---|---|
| `shell`、`process`、`file_write`、`file_edit`、`view_image`、`skill_manage`、`send_file`、`set_timezone`、MCP 工具 | 调用运行**之前**，展示其完整输入 |
| `memory_search`、`file_read`、`file_find`、`file_search` | 运行**之后**，展示它返回的全部内容（与模型将得到的内容完全一致；总计超过 4000 个字符时，其余部分在两边都会被略去，且两边都会被告知） |

对方会被告知 Agent 正在征询其所有者。所有者会在 Web 聊天中看到是谁提出的请求、对方写了什么以及具体的调用；如果所有者关联了该聊天应用的账号，还会在他与同一机器人的私聊中看到，并可在那里作答：

- **allow once** / **use**：仅此一次，执行该调用，或让模型得到它返回的内容；读取类工具或子 Agent 返回的内容不会保存在回复的步骤中，因此那里之后的轮次不会拥有这些内容；
- **always allow there**：对读取类工具，此后在该对话中直接回答，不再询问；对执行或修改类工具，只有完全相同的那个调用如此（在群中总是允许 `shell` 会让群里的任何人都能运行任何命令）；
- **deny**，或 10 分钟内未作答：模型会被告知不允许，且不得透露它本会得到什么；该回复中对该工具的后续调用会被直接拒绝，不再询问。

所有者可以补充哪些内容不能说（在 Web 聊天的输入框中，或在自己的聊天中发送 `/approve <id> not my address`），模型会与结果一起收到这条说明；若选择 always allow，它会作为规则保留，在那里之后的每一轮都提供给模型（在群中所有者自己的轮次里，只随结果一起提供）。Agent 的设置页面（“tools in other people's conversations”）列出了所有总是允许的项及其规则，可编辑或撤销。所有者还可以在那里允许某个读取类工具（`memory_search`、`file_read`、`file_find`、`file_search`）在与该 Agent 的所有对话中使用，并附上哪些内容不能说的规则：此后它在所有对话中都无需询问即可回答，因此在所有地方允许 `file_read`，就意味着任何能触达该 Agent 的人都能在所有者不过目的情况下读取它的文件。对话自身针对该工具的条目保留其规则；当该规则未涉及某方面时，则提供适用于所有对话的规则。该条目与其他条目一样可以列出、编辑和撤销，并以 `agent.tool_access.allowed` 记入审计日志。执行或修改类工具永远不能在所有地方允许。只有所有者能作决定，对话中的其他人永远不能；每个对话同一时间只有一个问题在等待，一个回复中的多个调用会依次询问。规则中的 `ask`（见“策略”）仍会对每个此类调用进行询问，无论是否总是允许。审计日志记录的是曾经询问过以及所作的决定，而不是调用本身、其返回内容或附加说明。在他人对话中创建的定时任务以及其他 Agent 的提问没有可询问的人，这些工具在那里会被拒绝；所有者在群聊之外的轮次、心跳以及所有者的任务照常使用这些工具（所有者在群中的轮次，见“群聊”下的“谁是谁”）。

<a id="language"></a>
### 语言

Agent 的设置页面可以设置 Havn 为它编写内容所用的语言：English（默认）或中文（`config.language`，取值 `"en"` 或 `"zh"`），由 Agent 的所有者设置。Agent 所写的内容在其下次启动时改变；网关自己回复的内容（它处理的命令、批准通知、失败提示）立即改变。涵盖范围包括：Havn 添加的系统提示词部分（平台描述、对话的对象和场合、已保存的事实、近期事件、摘要、时间、主动保存）、聊天命令的回复（`/help`、`/status`、`/whoami`、`/model`、`/think`、`/new`、`/compact`、`/undo`、`/usage`、`/approve` 等）、批准和排队通知、失败提示（回复失败、消息或文件未送达 Agent、定时任务失败）、提供给它的技能的标题，以及记忆工具的拒绝信息。工具名称和描述保持英文，因为模型对英文的遵循效果最好；所有者的 `SOUL.md` 和记忆保持其原本书写的语言；发送者配对之前收到的文本在 `channel_texts` 中按账号设置。无论哪种设置，Agent 都会用对方的语言回答。在中文中，Agent 的 owner 称为“所有者”。

<a id="subagents-and-asking-other-agents"></a>
### 子 Agent 与询问其他 Agent

Agent 可以把任务交给子 Agent（`subagent_spawn`）：子 Agent 以同一个 Agent 的身份运行，在一个回复中请求的多个子 Agent 会同时运行。对于每个子 Agent，Agent 会在其策略允许的模型（`allowed_models`）中选择一个模型，选择其思考程度（与 `/think` 的取值方式相同，在该模型支持的范围内；默认情况下，如果使用聊天的模型则沿用聊天的选择，否则使用模型自身的默认值），以及它能否看到目前为止的对话（`context: fork`，即最近 10 条消息，为默认值），还是只带着任务从零开始（`isolated`）。子 Agent 显示在启动它的那条回复的步骤中，附带它运行时使用的模型和思考设置以及它自己的步骤，而不会作为单独的聊天出现。子 Agent 可以使用 Agent 的工具，但不包括 `subagent_spawn`、`agent_query`、`set_timezone` 和 `schedule`（以及 `context_toolsets.subagent.disabled` 中列出的名称），每个子 Agent 最多运行 `tool_rounds.subagent` 轮；一个 Agent 中最多同时运行 8 个子 Agent。如果子 Agent 的模型不可用，它会自行回退，而不会使用回复的模型。

Agent 还可以向其所有者的另一个 Agent 提问（`agent_query`），对方会用自己的工具作答，但在他人对话中需要等待所有者的工具除外（见“他人对话中的工具”）。

两者都在 Agent 的设置页面（“working with other agents”）中，对应策略中的 `permissions.can_spawn_subagents` 和 `permissions.can_query_other_agents`。在单用户网关上，子 Agent 默认开启，除非 Agent 的策略将其关闭；询问其他 Agent 默认关闭。在多用户模式下，两者都遵循所有者的角色，所有者角色关闭的功能 Agent 无法开启。开启任一功能在 Agent 下次启动时生效；关闭询问其他 Agent 则立即停止。

<a id="questions-with-buttons"></a>
### 带按钮的问题

Agent 可以用 `ask_choice` 工具让正在对话的人在 2 到 10 个选项中作选择（问题最多 300 个字符，每个选项最多 100 个字符）。消息会列出带编号的选项。在能够显示的地方，单选问题为每个选项提供一个按钮，多选问题则显示为投票：在 Web 聊天中是按钮和复选框，在 Telegram 中是按钮和原生投票，在 Discord 中是按钮和可多选的菜单。在其他地方，对方回复编号即可。点击按钮或投票会发送 `/choose <question> <numbers>`，网关会将其转换为该用户发给 Agent 的下一条消息，发送到提出问题的那个聊天中，格式为 `[Chosen with a button, for "<question>": "<option>"]` 或 `[Chosen in the poll "<question>": "<option>", ...]`。一个问题只能回答一次（以答案送达 Agent 为准），只能在其所属的对话中回答，有效期为 24 小时，每个对话最多有 20 个问题在等待（超出时最早的被移除）；之后再点击会提示已回答或已过期。在聊天应用中，答案会发送到当前正在使用的聊天，它可能比问题所在的聊天更新；答案中会写明对应的问题。问题保存在网关的内存中，因此重启后会丢失。在 Web 聊天中，只要提问的聊天显示在屏幕上，问题就会显示在其中，直到页面重新加载；在未显示的聊天中提出的问题需要通过打字回答。按钮和投票只能由网关产生：Agent 在自己的消息上附加的按钮和投票会被丢弃。

<a id="voice-messages"></a>
### 语音消息

当设置了 `[transcription]` 时，送达 Agent 收件箱的语音消息和音频文件（Ogg/Opus、MP3、M4A、WAV、WebM、FLAC；最大 20 MB），无论来自聊天应用还是在 Web 聊天中上传，都会由网关转写。转写保存在文件旁边（`<file>.txt`），Agent 会连同文件一起收到它，并标注为用户提供的音频的转写；如果没有设置 `[transcription]`，或转写失败，Agent 会收到文件和一行说明未转写的文字。转写永远不会被当作命令：命令必须打字输入。此配置节无需重启即可生效。

```toml
[transcription]
kind        = "endpoint"                   # 兼容 OpenAI 的 /audio/transcriptions 接口
base_url    = "https://api.openai.com/v1"  # 默认值；也可以是 Groq、本地 whisper.cpp 服务器等
model       = "gpt-4o-mini-transcribe"     # 默认值
api_key_env = "OPENAI_API_KEY"             # 默认值：存放密钥的环境变量，位于网关的环境中（/etc/havn/havn.env）

# 或者：使用 [[llm.models]] 中一个接受音频的模型来转写，使用 Agent
# 所有者的凭据，并受 Agent 的策略和预算约束
# kind  = "model"
# model = "openrouter:google/gemini-3.8-flash"
```

端点的密钥从环境变量读取，绝不从 config.toml 读取；当该变量未设置或为空时（本地服务器），不发送密钥。使用 `kind = "model"` 时，请选择接受音频的 OpenRouter 或 Gemini 模型（Anthropic 模型不接受音频，OpenAI 自己的 API 只接受 WAV 和 MP3，而 Telegram 语音消息是 Ogg/Opus）；该模型必须已列出并允许该 Agent 使用，其 token 计入 Agent 的预算。转写耗时超过 90 秒会被放弃；最多同时进行四个转写。

<a id="chat-commands"></a>
### 聊天命令

只包含命令的消息由网关或 Agent 回复，永远不会到达模型，在渠道对话和 Dashboard 聊天中都是如此（在 Dashboard 中输入 `/` 会列出所有命令）。`/name@botname` 的形式同样有效；未知命令，或带了它不接受的参数的命令，会收到一条简短的回复加以说明。

| 命令 | 作用 |
|---------|------|
| `/new`（`/reset`、`/clear`） | 开启新聊天；之前的聊天会保留 |
| `/stop` | 停止正在生成的回复 |
| `/retry` | 重新回复最后一条消息 |
| `/undo` | 撤回最后一条消息及其回复；它们仍作为一个版本保存 |
| `/status` | 显示 Agent、该聊天的模型和思考设置、聊天标题和长度、等待中的回复 |
| `/model [name\|number\|default]` | 列出 Agent 可用的模型，或为此聊天单独设置模型；`/new` 会恢复为 Agent 的模型 |
| `/think [off\|on\|level\|tokens\|default]` | 显示或设置此聊天模型的思考程度，范围以其 `thinking` 提供的为准 |
| `/agent_model [name\|number\|default]` | 设置 Agent 的模型，与其设置页面的作用相同，适用于所有未单独设置模型的聊天 |
| `/usage [days]` | 按模型统计 Agent 的模型调用次数和 token（默认 30 天） |
| `/approve [always] [id]`、`/deny [id]` | 答复此对话中等待批准的步骤：指定请求 id 的那一个，否则为等待最久的那一个 |
| `/allow <sender>`、`/allow group <id>` | Agent 的所有者在与机器人的已连接私聊中，放行被机器人拒绝的某人或某个群（见[谁能触达机器人](#who-can-reach-a-bot)） |
| `/disconnect` | 在聊天应用中，解除此账号与 Havn 账号的关联（见[连接聊天账号](#connecting-a-chat-account)） |
| `/whoami` | 网关认为你是谁，以及你能否使用管理命令 |
| `/help`（`/commands`） | 列出所有命令 |

`/model`、`/think`、`/agent_model`、`/usage`、`/approve` 和 `/deny` 会消耗 Agent 所有者的凭据或放行某个步骤，因此只有管理员可以使用：在 Dashboard 中是 Agent 的所有者；在渠道中，是已将该聊天账号连接到自己 Havn 账号的 Agent 所有者（见[连接聊天账号](#connecting-a-chat-account)），以及账号 `admins` 中列出的发送者（默认没有）：

```toml
[[channels.telegram.accounts]]
id = "acme-bot"
allow_from = ["tg:123456789", "tg:987654321"]
admins = ["tg:123456789"]
```

在其适配器能显示按钮的聊天应用中（Telegram、Discord），当该聊天中的人有权决定时，提示某个步骤等待批准的消息会带有 **Approve**、**Approve for this chat** 和 **Deny** 按钮：点击会针对该请求发送 `/approve <id>`、`/approve always <id>` 或 `/deny <id>`，效果与手动输入相同，检查也相同；点击其中一个按钮后，机器人会移除消息上的按钮。如果请求在此期间已被决定（在 Web 聊天中，或通过另一次点击），会回复说明它已被决定。不支持按钮的适配器只显示文字，其中说明了如何用命令作答。

知道命令列表的 Telegram 适配器会在连接时将其设置为机器人的命令菜单（`setMyCommands`），这样在 Telegram 中输入 `/` 就会显示这些命令。Discord 适配器会将它们设置为机器人的斜杠命令，对于名称之后需要参数的命令，会带有一个 `text` 选项。

<a id="invite-codes"></a>
### 邀请码

运维者无需把每个团队成员都加入 `allow_from` 再重启网关，而可以为 `allowlist` 账号发放一次性邀请码：

```sh
havn invite create --channel telegram --account acme-bot           # 24 小时后过期
havn invite create --channel telegram --account acme-bot --expires 2h
```

该命令只会打印一次邀请码，例如 `7K3M-QX9P-2HWD-RT4B`。请通过你信任的渠道把它发给对方（你发放邀请码的方式本身就是对对方身份的担保）。对方在私聊中把邀请码发给机器人，或打开 `https://t.me/<bot>?start=<code>`（它会发送 `/start <code>`）；大小写、短横线和空格均不影响。随后网关会把对方的发送者 id 加入该账号的允许列表，将邀请码标记为已使用，记录一条 `channel_access.granted` 审计条目，并回复一条简短的欢迎语。邀请码本身不会传给 Agent。对方之后的消息会像 `allow_from` 中的任何人一样到达 Agent。

- 每个邀请码只能使用一次，只对创建它时指定的渠道和账号有效，并在 `--expires` 之后过期（默认 24h，最长 30d）。havn 只保存其 SHA-256 哈希。
- 错误、已使用、已过期或已撤销的邀请码得到的回复都相同（“That pairing code is not valid or has expired.”），因此发送者无法分辨是哪种情况。每个发送者在每个账号上每 15 分钟有 5 次尝试机会；超出后，任何邀请码（包括有效的）都会得到“Too many tries”的回复，直到这 15 分钟结束。失败的尝试会记录日志。
- 在 `dm_policy = "open"` 的账号上，所有人本来就已被放行，因此邀请码会作为普通消息传给 Agent。
- 邀请码只是放行发送者，并不会把他们连接到 Havn 账号。对于运维者或 Agent 所有者自己的聊天应用账号，请改用 **chat apps** > **connect**（见“连接聊天账号”）；如果账号已被放行，也请同样执行连接。

尚未配对的发送者会收到回复，但其消息永远不会到达 Agent：`/start` 会收到一条要求提供配对码的欢迎语，`/help` 会说明如何配对，其他任何消息则收到“Not paired yet. Send your pairing code, or /help.”。每条消息都会得到回复，但每个发送者每分钟最多 3 条（每个账号同时最多针对 10,000 个发送者），因此陌生人无法让机器人发送更多消息。命令不区分大小写，带不带 `@botname` 均可识别。这些文本可以按账号设置，以便机器人使用其他语言；省略或留空的文本保持默认值，更改无需重启即可生效：

```toml
[channel_texts.telegram.acme-bot]
welcome      = "你好！使用前请先配对：发送你拿到的配对码。"
unpaired     = "还没有配对。请发送配对码，或发送 /help。"
invalid_code = "配对码无效或已过期。"
help         = "这个机器人只回复已配对的人。请发送管理员给你的配对码。"
paired       = "配对成功，现在可以聊天了。"
too_many_tries = "尝试次数太多，请几分钟后再发送配对码。"
```

每条文本最多截取 2000 个字符；对于配置中未声明的账号的文本，网关会发出警告。

Agent 的所有者也可以在其设置页面完成同样的操作（见[谁能触达机器人](#who-can-reach-a-bot)）。运维者使用与 `havn user` 相同的直接操作数据库的命令来管理邀请和已放行的发送者（无需重启网关）：

```sh
havn invite list [--channel telegram] [--account acme-bot]   # 每个邀请的状态；邀请码不会再次显示
havn invite revoke <invite-id>                               # 撤销一个未使用的邀请
havn access list [--channel telegram] [--account acme-bot]   # allow_from 条目和受邀发送者
havn access revoke --channel telegram --account acme-bot tg:123456789
havn access revoke --all --account acme-bot [--channel telegram]   # 该账号的所有受邀发送者
```

`access revoke` 从该发送者的下一条消息起生效，Agent 也无法再给对方发消息。`allow_from` 中的条目只能在配置文件中删除；如果被移除的发送者仍列在那里，`access revoke` 会发出警告。

受邀发送者按账号保存，不受配置编辑的影响。如果某个账号被删除或改为 `open`，其授权不再生效但会保留，当该账号恢复为 `allowlist` 时重新生效。网关会在启动时对此类授权发出警告，`access list` 也会标注它们；可用 `havn access revoke --all` 清除。

<a id="group-chats"></a>
### 群聊

机器人可以在其平台的群中使用（Telegram 的群组和超级群组，不包括广播频道；Discord 服务器的频道及其帖子），按账号设置：

```toml
[[channels.telegram.accounts]]
id = "acme-bot"
group_policy = "allowlist"           # "off"（默认）、"allowlist" 或 "open"
allow_groups = ["tg:-1001234567890"] # 配合 "allowlist" 使用
```

- `off`：不使用群消息。向机器人发话的群会被告知机器人暂未在群中启用，该群会作为被拒绝的群列给所有者看，所有者也会在其已连接的私聊中被告知该机器人的群功能已关闭以及在哪里开启（没有按钮，因为此时放行该群不会产生任何效果）。
- `allowlist`：`allow_groups` 中的群，以及 Agent 所有者放行的群（见“谁能触达机器人”），可以使用它。未被放行的群向机器人发话时，会被告知尚未放行，该群会作为被拒绝的群列给所有者看，所有者也会在其已连接的私聊中收到通知，并附带 **Allow the group** 按钮（`/allow group <id>`）。在这类群中未向机器人发话的消息不会被保留。
- `open`：机器人被加入的所有群。当账号为 `open` 时，Agent 可以向它曾被使用过的群发送消息。

被放行的群让其成员可以在该群中使用机器人，且仅限于该群：这并不会让他们获准进行私聊。

**每个群一个对话。** 群中所有人共享一个对话，该对话有自己的聊天（`/new` 会为所有人开启一个新聊天；`/retry` 只在私聊中有效，因为最后一条消息可能是别人发的）。回复会以回复所答消息的形式发到群里。Agent 收到的每条消息都会注明作者：平台显示的名称（加引号）和其发送者 id；如果是 Agent 所有者的已连接账号，还会注明“your owner”。成员写的看起来像这些标签的行会被加上引号。

**服务器与帖子。** 如果某个平台的群属于一个更大的群体（Discord：服务器中的频道），平台会说明这一点，并且可以整个服务器放行：所有者收到的频道被拒通知会提供 **Allow the whole server** 和 **Allow this channel**，`allow_groups` 和 `/allow group` 也都接受这两种 id。只要频道本身或其所在服务器被放行，该频道就可以使用机器人；放行服务器即放行其全部内容，包括之后创建的频道，对其中所有人有效。频道中的帖子是一个独立的对话，有自己的聊天，回复会发到帖子中；对未放行频道的答复会发到频道中，除非该帖子此前已被放行。Agent 只能向它见过、且本身或其服务器可以使用机器人的频道和帖子发送消息；服务器本身不是可以发送消息的目标（网关会拒绝它知道的服务器，适配器会拒绝任何服务器）。Agent 会被告知消息所在的服务器、频道和帖子。

**何时回复。** Agent 会回复向机器人发话的消息：提及它的消息（`@acme_bot`）、回复它的某条消息，或者是命令。其他消息不会被回复。网关会为每个群保留其中最近的 30 条（每条 500 个字符，仅保存在内存中），并在下一条向 Agent 发话的消息中一并提供给它，作为自上次向它发话以来的对话内容。

**Telegram 的隐私模式。** 它默认开启，并在机器人被加入某个群时就为该群固定下来。开启期间，Telegram 在群中只会把以下内容交给机器人：服务消息（成员加入和离开、置顶、新标题）、发给它的命令（`/help@acme_bot`，或者在它是最后一个在群里发言的机器人时的 `/help`）、对它的消息以及对发给它的消息的回复，以及通过它发送的消息：不是回复的提及永远不会到达它。可以通过 BotFather 关闭隐私模式（`/setprivacy`，Disable），然后把机器人从每个群中移除再重新加入，因为只有这样才能让更改对该群生效；已放行的群仍保持放行。作为群管理员的机器人无论如何都能看到所有消息。在适配器能够得知的情况下，它会针对每个群说明机器人能否看到其中的所有消息：依据机器人被加入时的模式、它是否为该群管理员，或者某条只有这类机器人才能收到的消息。如果无法得知，或者网关不知道（例如重启后，直到该群再次发言），群的欢迎语会请成员回复机器人或给它发送命令（如 `/help@acme_bot`），并且只在确知提及能到达机器人时才提到提及的方式。如果确知提及无法到达，放行该群的所有者会被告知如何关闭隐私模式，Agent 的设置页面也会在已放行的群中标注该群（`allow_groups` 中的群除外，它们只以 id 列出）。

**谁是谁。** 一条消息是否来自所有者是逐条判断的。Agent 在群中说的话整个群都能看到，其他成员的发言也会进入所有者的轮次，因此所有者在群里的已连接账号不会被当作在私聊中对待。所有者的轮次不会得到记忆中私有的任何内容：没有私有事实，没有近期事件，也不会提示主动保存他说的话（但有它掌握的公开事实，每个聊天都是如此）；所有者要求 Agent 保存的内容会被保存为私有，且不会建议将其公开，不过 Agent 只是重复一遍的事实仍保持原有可见性。“他人对话中的工具”下表中列出的工具会像对群里任何人一样等待所有者的批准，询问会发到他与同一机器人的私聊和 Web 聊天中，并且请求会注明是他在群里提出的，在那里选择总是允许即对群里所有人允许（对于执行类工具，仅限那个完全相同的调用）；所有者要求 Agent 保存或遗忘的内容则无需询问直接执行。读取类工具和子 Agent 返回的内容不会保存在回复的步骤中供之后的轮次使用，群的摘要也永远不会把事实保存到记忆中。其他所有人都是所有者以外的人（见“他人对话中的工具”）：他们的调用需要等待所有者，询问会发到所有者与同一机器人的私聊和 Web 聊天中，绝不会发到群里；群会被告知 Agent 正在征询其所有者，所有者会看到该成员写了什么。“Always allow there”对该群生效。在群里，`/approve` 和 `/deny` 需要提供步骤的 id，因此所有者只能决定自己看到过的步骤。群的聊天不属于所有者本人：它们不会与所有者的聊天一起被搜索，在那里安排的任务也不属于所有者。

<a id="who-can-reach-a-bot"></a>
### 谁能触达机器人

Agent 的设置页面无需 CLI 即可管理其机器人（**who can reach the bot**，仅所有者可用；`GET /agents/{id}/channel-access`）。对于绑定到该 Agent 的每个账号，它可以设置谁能在私聊中给它发消息（`dm_policy`：已放行的人，或任何人），以及在群中的规则（`group_policy`：关闭、已放行的群，或它被加入的任何群）。更改会写入 config.toml 中该账号的 `[[channels.<channel>.accounts]]` 表，保留文件的其余部分（包括注释），并立即生效，无需重启（`PUT .../channel-access/policy`，以 `channel_account.policy.changed` 记入审计日志，并附带原值）。写入的内容必须能被加载：如果 config.toml 以其他方式（内联表）定义该账号，或者存在网关无法加载的手动编辑，则文件保持原样、不做任何更改，页面会说明原因。将机器人向任何人开放之前会先确认。启用 `trust_header` 时，这些设置只能通过 config.toml 修改。开启群功能后，所有者会再次收到关闭期间被拒绝的群的通知；群功能关闭时无法放行群。该页面列出：

- `allow_from` 中的发送者（只能在 config.toml 中修改）；
- 通过邀请码或连接码、或由所有者放行的发送者，以及每个人是否已连接到 Havn 账号（所有者自己的账号会被标注）。**remove** 会从其下一条消息起收回其访问权限（`DELETE .../channel-access/senders`）；所有者自己的已连接账号在连接期间始终保持放行。
- 为它创建的邀请码。**make an invite code**（1 小时到 30 天）只显示一次邀请码，在已知机器人用户名时还会附带一个 `t.me` 链接；未使用的邀请码可以撤销。连接码属于各自用户本人，不会列出。
- 可以使用它的群：`allow_groups` 中的群、所有者放行的群（**remove**），以及在 `open` 下它曾被使用过的群（群功能为 `off` 时不使用任何群，但已放行的群会保留，待开启后使用）。整个放行的服务器会标注为服务器，被放行的频道会注明其所属服务器；
- 它最近拒绝的发送者和群，群以其标题显示，服务器中的频道会附带其服务器，**let in the whole server** 会放行该服务器及其中的所有频道：显示是谁、大约多少次（按得到回复的次数计，每分钟最多 3 次）、最后一次的时间，以及最后所说内容的开头，其中看起来像配对码的部分会被略去。**let in** 放行他们（`POST .../channel-access/senders`），**dismiss** 则将其遗忘。每个账号保留最近发言的 100 个，保留 30 天。通过其他方式已放行的人（`allow_from`、邀请码、所有者的已连接账号）发送无效代码时不会被列出。

也可以按 id 放行发送者。

当被机器人拒绝的人发来消息，且 Agent 的所有者已在该渠道上连接了自己的账号时，所有者会在其与同一机器人的私聊中收到通知（Telegram 只有在所有者已启动该机器人时才会送达）：以纯文本显示是谁发了什么，并附带一个 **Allow** 按钮，点击会发送 `/allow <sender>`。每个发送者每天只通知一次，每个账号每小时最多通知 5 个发送者。同一页面上的设置可以关闭此功能（`config.notify_unpaired = false`）。只有 Agent 的所有者可以使用 `/allow`；它会把发送者放行到所有者发送该命令的那个机器人。它只放行该机器人见过、拒绝过或放行过的 id：群或服务器的 id 作为群放行，带不带 `group` 一词均可；发送者的 id 作为发送者放行（对发送者 id 使用 `/allow group` 会被拒绝）。对于它没见过的 id，会回复说明，且不放行任何人；被拒绝者在最后一次发言后保留 30 天（每个机器人最近的 100 个），因此对于较早的通知，请让对方再给机器人发一次消息。

被放行的发送者会被告知现在可以对话了。放行、移除以及创建或撤销代码分别以 `channel_access.granted`（附带 `"by": "owner"`）、`channel_access.revoked`、`channel_invite.created` 和 `channel_invite.revoked` 记入审计日志。

<a id="connecting-a-chat-account"></a>
### 连接聊天账号

Havn 用户可以自行把自己的 Telegram 账号（或其他聊天应用的账号）连接到自己的 Havn 账号，无需运维者参与。Agent 的所有者应当连接自己用来与其机器人对话的账号：在此之前，机器人会把该账号当作别人来回复（见“放行与连接”）。在 Agent 的设置页面上，**chat apps** > **connect** 会为该 Agent 绑定的每个渠道账号生成一个配对码；配对码只能使用一次，有效期 10 分钟。用户把它发给机器人，或打开旁边显示的链接（它会发送 `/start <code>`）。随后网关会像处理邀请一样放行该发送者，把该发送者关联到生成配对码的 Havn 用户，并回复“Connected to Havn account <name>.”。

- 拥有该 Agent 的已关联发送者无需列入 `admins` 即可使用管理命令（`/model`、`/approve` 等）；`admins` 保留给没有 Havn 账号的人使用。
- 已关联的发送者无需另一个配对码，即可给同一用户所有 Agent 的机器人发消息。
- 一个发送者只属于一个用户，一个用户在每个渠道上只有一个发送者：再次关联会替换之前的关联，之前的发送者会失去该用户的配对码所赋予的权限。为已关联到其他用户的发送者生成的配对码会被拒绝（“already connected to another Havn account”）：用户需先给机器人发送 `/disconnect`，无论关联到谁，都会解除该关联（运维者可用 `havn access unlink --channel telegram tg:123` 达到同样效果）。已经能给机器人发消息的发送者，同样通过该链接（`/start <code>`）进行关联；以这种方式发送的普通邀请码会留给其对应的人。
- 关联可以在任一 Agent 设置页面的 **chat apps** 下列出和删除（`GET` 和 `DELETE /me/channel-links`）。断开连接会收回该用户的配对码赋予该发送者的访问权限；通过其他方式获得的放行（`allow_from`、邀请码）保持不变。
- 绑定到多个 Agent（`[[bindings]]`）的账号会把消息发给第一个 Agent；只有该 Agent 的所有者能为其生成配对码。
- 关联是按渠道进行的，因此适配器的发送者 id 必须在该渠道的每个账号上都对应同一个人，Telegram 的用户 id 就是如此。`havn access list` 显示每个发送者关联到了谁。生成配对码、关联和解除关联分别以 `channel_link.code_created`、`channel_link.created` 和 `channel_link.removed` 记入审计日志。
- 用 `havn invite create` 创建的邀请码照常有效：它们放行发送者，但不把发送者关联到任何人。

**Web 聊天中的聊天应用聊天。** 连接之后，用户在该聊天应用中与某个 Agent 的聊天也会列在该 Agent 的 Web 聊天中，位于 **in chat apps** 下，并标注所属应用（`GET /agents/{id}/channel-chats`）；只列出用户本人的聊天，绝不包括其他人与该 Agent 的聊天。这类聊天以只读方式打开：显示其消息，以及 `/retry` 和 `/undo` 留下的版本，可以像在 Web 聊天中一样切换。消息输入框的位置会显示一行说明，指出该聊天存在于应用中并在那里继续，旁边有 **continue on the web**（`POST /agents/{id}/chats/{chat}/move`）。该操作会永久迁移聊天：它成为用户的 Web 聊天之一；如果它是应用当前的聊天，应用的下一条消息会开启一个新聊天；在其中创建的定时任务此后会投递到 Web 聊天；机器人会在应用中说明该聊天已迁移。当聊天中有回复正在进行或等待时，迁移会被拒绝；迁移与应用的消息按顺序处理，因此迁移之后不会有消息落入该聊天。机器人会注明之前已迁移的聊天，以免被误认为当前聊天。迁移以 `chat.moved_to_web` 记入审计日志。Agent 发送的文件只在用户本人的聊天（Web 聊天或已关联的聊天）中列出和下载，不包括它在聊天应用中发给其他人的文件。

生成链接需要账号上配置了机器人的用户名；否则页面只显示配对码：

```toml
[[channels.telegram.accounts]]
id           = "acme-bot"
bot_username = "acme_bot"    # Telegram 机器人的用户名，用于 https://t.me/acme_bot?start=<code>
```

关联成功后的回复可以像其他文本一样设置，用 `{name}` 表示账号名称：在 `[channel_texts.<channel>.<account>]` 下设置 `linked = "已连接到 Havn 账号 {name}。"`。

<a id="capability-packs"></a>
## 能力包

某些 Agent 工作负载需要 havn 未内置于核心的能力（浏览器、代码解释器、厂商 CLI）。Havn 提供通用原语——bind mount、tmpfs mount、seccomp 放行——由外部能力包将它们组合起来。能力包位于独立的仓库中，安装在 `/opt/havn/<pack>/` 下。`havn update` 会列出它们但不会更新它们：请运行能力包较新版本的 `setup.sh`（见“升级”）。

| 能力包 | 仓库 | 描述 |
|------|-----------|-------------|
| Browser | [havn-pack-browser](https://github.com/amplimit/havn-pack-browser) | Playwright CLI + Chromium，用于网页浏览 |

<a id="license"></a>
## 许可证

Copyright 2025-2026 Amplimit

基于 Apache License 2.0 授权。详情见 [LICENSE](../LICENSE)。

