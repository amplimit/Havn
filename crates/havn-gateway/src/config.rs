//! Gateway configuration loading.
//!
//! Loaded once at startup. Mid-process config edits go through the
//! hot-reload path described in spec §8.4 (rule table, fs-watch,
//! debounced) — wired into this module in a follow-up commit.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    pub listen: String,
    /// Path to the gateway's `SQLite` database file. Per locked scope, no
    /// other datastore is supported. Created on first run with WAL + foreign
    /// keys.
    pub db_path: PathBuf,
    /// Unix socket path that agent runtime processes dial back into.
    pub agent_socket_path: PathBuf,
    /// Root directory under which per-agent workspaces are created.
    pub workspace_root: PathBuf,
    /// Path to the `havn-runtime` binary the spawner exec()s. When unset,
    /// the gateway resolves it from the same directory as its own binary.
    pub runtime_binary: Option<PathBuf>,
    /// Path to the `havn-init` PID-1 shim used by the namespace spawner
    /// (spec §4.1). When unset, resolved next to the gateway binary like
    /// `runtime_binary`. The subprocess fallback ignores this field.
    pub init_binary: Option<PathBuf>,
    /// `Origin` headers accepted on the `WebSocket` `WebChat` endpoint
    /// (spec §11 ClawJacked-class mitigation). Defaults cover the Next.js
    /// dashboard's dev server. Production deployments add their domain.
    pub allowed_origins: Vec<String>,
    pub reload: ReloadConfig,
    /// Trust-header configuration (spec §1.7). When enabled, the
    /// gateway resolves the calling user from the `X-User-ID` header
    /// set by an upstream reverse proxy. Required for any non-loopback
    /// listen address; the startup guard refuses to bind otherwise.
    pub trust_header: TrustHeaderConfig,
    /// Hybrid-retrieval embedding config (spec §9.4 v0.7). Shipped
    /// to each runtime via the Welcome frame; runtime instantiates
    /// the right `EmbeddingProvider`. Defaults to `provider = "disabled"`
    /// — operators opt in by editing the `[embedding]` block in
    /// config.toml. When disabled, `memory_search` falls back to
    /// the v0.6 FTS5-only behaviour byte-identically.
    pub embedding: serde_json::Value,
    /// Operator-declared extra bind-mounts injected into every agent's
    /// namespace (spec §4.1 v0.7). Used by capability packs that drop
    /// binaries under a host path the agent would otherwise not see —
    /// e.g. `/opt/havn/playwright-cli` for havn-pack-browser.
    ///
    /// **Operator-only.** Per-agent config CANNOT specify mounts (would
    /// give any agent owner arbitrary host-FS access — spec §1.7 trust
    /// model says operator owns mount config, same as `HAVN_AGE_KEY`
    /// and trust_header).
    ///
    /// TOML form:
    /// ```toml
    /// [[extra_mounts]]
    /// path = "/opt/havn/playwright-cli"
    /// mode = "ro"   # or "rw"; default "ro"
    /// ```
    #[serde(default)]
    pub extra_mounts: Vec<havn_spawner::ExtraMount>,
    /// Operator-declared in-namespace tmpfs mounts (spec §16.2).
    /// Mounted inside the agent rootfs after pivot, with
    /// `MS_NOSUID|MS_NODEV` always on. The capability-pack motivating
    /// case is `/dev/shm` for any browser pack — Chromium crashes
    /// without it on JS-heavy pages — but havn-spawner is unaware of
    /// the pack.
    ///
    /// **Operator-only**, same trust posture as `extra_mounts`.
    ///
    /// TOML form:
    /// ```toml
    /// [[tmpfs_mounts]]
    /// path = "/dev/shm"
    /// size_mb = 1024
    /// ```
    #[serde(default)]
    pub tmpfs_mounts: Vec<havn_spawner::TmpfsMount>,
    /// Seccomp section — operator-declared relaxations to the runtime
    /// blocklist (spec §16.2). Default empty: agents run with the v0.6
    /// baseline blocklist. Names are validated by the runtime against
    /// `libc::SYS_*`; unknown names refuse the runtime startup (typo
    /// guard).
    ///
    /// TOML form:
    /// ```toml
    /// [seccomp]
    /// allow_extra = ["pivot_root", "userfaultfd"]
    /// ```
    #[serde(default)]
    pub seccomp: SeccompConfig,
    /// Per-agent network knobs (spec §6.3 v0.7 / §16). Default values
    /// match what the gateway hard-coded before this section was
    /// introduced — operators only fill it in when they need to
    /// override (internal DNS, conflicting CIDR with VPN/Docker, …).
    #[serde(default)]
    pub network: NetworkConfig,
    /// Channel-adapter account configuration (spec §3.4). Keyed by
    /// channel id ("telegram", "slack", …); each value lists the
    /// operator-controlled accounts (bot identities) on that channel.
    /// WebChat is NOT a channel adapter — it's the dashboard's console
    /// (spec §3.6) — and does not appear here.
    #[serde(default)]
    pub channels: std::collections::BTreeMap<String, ChannelEntry>,
    /// Cross-channel agent bindings (spec §3.5). Each binding routes
    /// inbound messages from a `(channel, account_id)` pair to a
    /// specific agent. Bindings are config-only — there is no DB
    /// table; operators edit gateway.toml + restart (or hot-reload via
    /// §8.4 once that path supports new TOML keys).
    #[serde(default)]
    pub bindings: Vec<ChannelBindingConfig>,
}

/// One channel's accounts list (spec §3.4). The TOML shape is
/// `[[channels.<channel>.accounts]]` — multiple accounts per channel,
/// one bot per account.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ChannelEntry {
    #[serde(default)]
    pub accounts: Vec<ChannelAccount>,
}

/// One operator-controlled bot identity on a chat platform (spec §3.4).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ChannelAccount {
    /// Operator-supplied stable id (e.g. "alice-tg-bot"). Referenced by
    /// `[[bindings]] account_id = ...`. Unique within its channel; the
    /// gateway's startup validation flags duplicates.
    pub id: String,
    /// Reference to the credential row holding the WS auth token —
    /// `secret:<provider>:<name>` format. Provider should be
    /// `channel:<channel_id>` (e.g. `channel:telegram`); name should
    /// match `id` above by convention but the gateway only enforces
    /// resolvability, not naming convention.
    pub adapter_token_ref: String,
    /// DM access policy. Default `owner_only` — only the bound agent's
    /// owner may DM. Operators with public bots set `open`; allowlist
    /// for explicit user-id lists.
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Platform-prefixed sender IDs allowed when `dm_policy = "allowlist"`.
    /// Ignored under other policies.
    #[serde(default)]
    pub allow_from: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DmPolicy {
    /// Only senders listed in `allow_from` may DM. Default for explicit
    /// access control.
    Allowlist,
    /// Any sender on the platform may DM. Use for public help bots.
    Open,
    /// Only the bound agent's owner may DM. Default for `webchat:*`
    /// loopback accounts (when the channel grows that surface) and a
    /// safe default for other channels too.
    #[default]
    OwnerOnly,
}

/// One cross-channel binding (spec §3.5). The same agent can have
/// multiple bindings — one per channel it should appear on.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ChannelBindingConfig {
    pub agent_id: String,
    pub channel: String,
    pub account_id: String,
}

/// Validate `[[bindings]]` against `[[channels.<channel>.accounts]]`
/// at gateway startup (spec §3.5). Returns the list of binding
/// indices that reference a channel/account pair that doesn't exist —
/// caller logs warnings, does not refuse to start (operators should
/// be able to comment out an account temporarily without crashing
/// every other binding's gateway).
///
/// Agent-id existence is NOT checked here — that's a DB lookup the
/// gateway can't do at config-load time without circular dep on the
/// pool. Bindings to nonexistent agents surface their own clear
/// errors at first inbound message dispatch.
#[must_use]
pub fn dangling_bindings(cfg: &GatewayConfig) -> Vec<DanglingBinding> {
    let mut out = Vec::new();
    for (idx, b) in cfg.bindings.iter().enumerate() {
        let entry = cfg.channels.get(&b.channel);
        let known_channel = entry.is_some();
        let known_account = entry.is_some_and(|e| e.accounts.iter().any(|a| a.id == b.account_id));
        if !known_channel || !known_account {
            out.push(DanglingBinding {
                index: idx,
                agent_id: b.agent_id.clone(),
                channel: b.channel.clone(),
                account_id: b.account_id.clone(),
                reason: if known_channel {
                    DanglingReason::UnknownAccount
                } else {
                    DanglingReason::UnknownChannel
                },
            });
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct DanglingBinding {
    pub index: usize,
    pub agent_id: String,
    pub channel: String,
    pub account_id: String,
    pub reason: DanglingReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DanglingReason {
    UnknownChannel,
    UnknownAccount,
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct SeccompConfig {
    /// Names of syscalls to remove from the runtime's seccomp blocklist.
    /// See spec §16.2 — the only mechanism by which an operator can let
    /// a workload (typically a capability pack) call a syscall the
    /// default blocklist denies. Pack documentation lists which names
    /// are needed; the operator pastes them here.
    #[serde(default)]
    pub allow_extra: Vec<String>,
}

/// Per-agent network configuration (spec §6.3 v0.7 / §16).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// DNS servers written into the agent namespace's `/etc/resolv.conf`
    /// when the agent has `network_policy.egress_allowed=true`.
    /// Defaults to public resolvers; operators with internal DNS
    /// (Pi-hole, corporate split-horizon, DoH bridge) override here.
    /// Does NOT affect agents whose policy disallows egress —
    /// resolv.conf isn't even mounted in that case.
    pub agent_dns: Vec<String>,
    /// CIDR pool for per-agent /30 subnets. Default `10.42.0.0/16`
    /// gives 16384 simultaneous agents. Operators with conflicting
    /// VPN / Docker bridge / k8s pod networks override here. Setup
    /// refuses to start the gateway if this collides with an existing
    /// host route.
    pub agent_pool_cidr: String,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            agent_dns: vec!["1.1.1.1".into(), "8.8.8.8".into(), "9.9.9.9".into()],
            agent_pool_cidr: "10.42.0.0/16".into(),
        }
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            // Loopback by default; explicit opt-in for public exposure (spec §11
            // ClawJacked-class mitigation).
            listen: "127.0.0.1:8080".into(),
            db_path: default_data_dir().join("havn.db"),
            agent_socket_path: default_data_dir().join("agent.sock"),
            workspace_root: default_data_dir().join("agents"),
            runtime_binary: None,
            init_binary: None,
            allowed_origins: default_allowed_origins(),
            reload: ReloadConfig::default(),
            trust_header: TrustHeaderConfig::default(),
            embedding: default_embedding_config(),
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp: SeccompConfig::default(),
            network: NetworkConfig::default(),
            channels: std::collections::BTreeMap::new(),
            bindings: Vec::new(),
        }
    }
}

/// Default = `{ "provider": "disabled" }` — preserves byte-for-byte
/// v0.6 behaviour for operators who haven't opted into hybrid
/// retrieval. Stored as raw `Value` so the gateway crate doesn't
/// have to depend on havn-runtime for the strongly-typed enum;
/// the runtime parses it.
fn default_embedding_config() -> serde_json::Value {
    serde_json::json!({ "provider": "disabled" })
}

fn default_data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/havn")
    } else {
        PathBuf::from("./havn-data")
    }
}

fn default_allowed_origins() -> Vec<String> {
    // Match the dashboard's default port (`apps/dashboard/server.mjs`
    // listens on 8101 and rewrites Origin to a canonical loopback
    // value — see `app/agents/[id]/page.tsx` chat header). The
    // `:3000` form is kept for `next dev` (which still defaults to
    // 3000) so the dev workflow round-trips without a config edit.
    vec![
        "http://127.0.0.1:8101".into(),
        "http://localhost:8101".into(),
        "http://127.0.0.1:3000".into(),
        "http://localhost:3000".into(),
    ]
}

impl GatewayConfig {
    /// Resolve the runtime binary path: explicit config wins; otherwise
    /// fall back to the directory of the current `havn-gateway` binary.
    pub fn resolved_runtime_binary(&self) -> anyhow::Result<PathBuf> {
        Self::resolve_sibling_binary(self.runtime_binary.as_ref(), "havn-runtime")
    }

    /// Resolve the `havn-init` shim path. Same convention as
    /// [`resolved_runtime_binary`]: explicit config wins, otherwise look
    /// next to the gateway binary.
    pub fn resolved_init_binary(&self) -> anyhow::Result<PathBuf> {
        Self::resolve_sibling_binary(self.init_binary.as_ref(), "havn-init")
    }

    fn resolve_sibling_binary(explicit: Option<&PathBuf>, name: &str) -> anyhow::Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(p.clone());
        }
        let exe = std::env::current_exe().map_err(|e| {
            anyhow::anyhow!("locating current executable to derive {name} path: {e}")
        })?;
        let dir = exe.parent().ok_or_else(|| {
            anyhow::anyhow!("current executable has no parent dir: {}", exe.display())
        })?;
        Ok(dir.join(name))
    }
}

impl GatewayConfig {
    /// Load config from `$HAVN_CONFIG` (if set) or `~/.config/havn/config.toml`.
    /// Falls back to defaults if no file is present. After parsing, applies
    /// env-var overrides (spec §1.7): `HAVN_TRUST_HEADER=1` flips
    /// `trust_header.enabled` to true.
    pub fn load() -> anyhow::Result<Self> {
        let path = match std::env::var_os("HAVN_CONFIG") {
            Some(p) => PathBuf::from(p),
            None => default_config_path(),
        };

        let mut cfg = if path.exists() {
            Self::load_from(&path)?
        } else {
            tracing::warn!(path = %path.display(), "no config file found; using defaults");
            Self::default()
        };

        if matches!(
            std::env::var("HAVN_TRUST_HEADER").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        ) {
            cfg.trust_header.enabled = true;
        }

        Ok(cfg)
    }

    /// Load and parse a config file at an explicit path. Used by the hot-reload
    /// watcher (spec §8.4). Errors propagate so the watcher can roll back to
    /// the last known good snapshot.
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: Self = toml::from_str(&raw)?;
        parsed.validate_channel_ids()?;
        Ok(parsed)
    }

    /// Reject `|` (the `channel_id_codec` separator) in any channel
    /// name or account_id at load time. The codec is the gateway's
    /// only way to round-trip a (channel, account, sender) triple
    /// through `InboundMessage.channel_id` so the runtime can route
    /// outbound replies; a `|` in those fields would corrupt the
    /// encoded id and surface as a confusing "no binding" warning on
    /// the first message instead of a clear config error at startup
    /// (spec §3.4 / §3.5).
    fn validate_channel_ids(&self) -> anyhow::Result<()> {
        use crate::channel_id_codec::validate_channel_id_part;

        for channel_name in self.channels.keys() {
            validate_channel_id_part("channel", channel_name).map_err(|e| {
                anyhow::anyhow!("[[channels.<name>]]: invalid channel key {channel_name:?} — {e}")
            })?;
        }
        for (channel_name, entry) in &self.channels {
            for acct in &entry.accounts {
                validate_channel_id_part("account_id", &acct.id).map_err(|e| {
                    anyhow::anyhow!("[[channels.{channel_name}.accounts]] id={:?}: {e}", acct.id)
                })?;
            }
        }
        for (idx, b) in self.bindings.iter().enumerate() {
            validate_channel_id_part("channel", &b.channel)
                .map_err(|e| anyhow::anyhow!("[[bindings]][{idx}] channel={:?}: {e}", b.channel))?;
            validate_channel_id_part("account_id", &b.account_id).map_err(|e| {
                anyhow::anyhow!("[[bindings]][{idx}] account_id={:?}: {e}", b.account_id)
            })?;
        }
        Ok(())
    }

    /// Resolve the config path the watcher should target. Mirrors the
    /// resolution `load()` does, so they stay in lockstep.
    pub fn resolved_path() -> PathBuf {
        std::env::var_os("HAVN_CONFIG").map_or_else(default_config_path, PathBuf::from)
    }
}

fn default_config_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/havn/config.toml")
    } else {
        PathBuf::from("./havn.toml")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReloadConfig {
    pub debounce_ms: u32,
}

impl Default for ReloadConfig {
    fn default() -> Self {
        // No `mode`: hot paths apply in place, deploy-time paths are reported
        // for the operator to apply via `havn gateway restart` (spec §8.4,
        // issue #10). A legacy `[reload] mode = ...` in an existing config is
        // ignored rather than rejected.
        Self { debounce_ms: 300 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustHeaderConfig {
    /// `true` to accept `X-User-ID` from upstream and resolve identity
    /// against the `users` table. Required for any non-loopback
    /// listen address (spec §1.7); the gateway refuses to start
    /// otherwise. Default `false` so a fresh single-user install on
    /// loopback works out of the box.
    pub enabled: bool,
    /// Optional CIDR allowlist of source IPs we trust to set the
    /// header. v0.6 ships the field for future enforcement but does
    /// not yet check connection-source IP — operators rely on the
    /// reverse proxy + listen-address discipline. Reserved.
    #[allow(dead_code, reason = "reserved for source-IP enforcement")]
    pub allowed_proxies: Vec<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn defaults_listen_on_loopback() {
        let cfg = GatewayConfig::default();
        assert!(
            cfg.listen.starts_with("127.0.0.1"),
            "loopback default required (§11)"
        );
    }

    #[test]
    fn empty_toml_round_trips_to_defaults() {
        let parsed: GatewayConfig = toml::from_str("").expect("parse empty");
        assert_eq!(parsed.reload.debounce_ms, 300);
        assert!(parsed.channels.is_empty());
        assert!(parsed.bindings.is_empty());
    }

    #[test]
    fn legacy_dead_config_blocks_are_ignored_not_rejected() {
        // `[reload] mode` (removed in #10) and the whole `[defaults]` block
        // (removed in #18 — it was never consumed) must keep parsing in an
        // existing config rather than fail to load.
        let parsed: GatewayConfig = toml::from_str(
            "[reload]\nmode = \"hybrid\"\ndebounce_ms = 500\n\n[defaults]\nmodel = \"x\"\nmemory_mb = 999",
        )
        .expect("legacy dead config blocks should be ignored");
        assert_eq!(parsed.reload.debounce_ms, 500);
    }

    #[test]
    fn channel_accounts_parse_from_toml_array_of_tables() {
        // Spec §3.4 — operator-facing config shape.
        let s = r#"
            [[channels.telegram.accounts]]
            id = "alice-tg-bot"
            adapter_token_ref = "secret:channel:telegram:alice-tg-bot"
            dm_policy = "allowlist"
            allow_from = ["tg:123456789"]

            [[channels.telegram.accounts]]
            id = "team-help-bot"
            adapter_token_ref = "secret:channel:telegram:team-help-bot"
            dm_policy = "open"

            [[channels.slack.accounts]]
            id = "sales-bot"
            adapter_token_ref = "secret:channel:slack:sales-bot"
        "#;
        let cfg: GatewayConfig = toml::from_str(s).expect("parse");
        assert_eq!(cfg.channels.len(), 2);
        let tg = cfg.channels.get("telegram").expect("telegram entry");
        assert_eq!(tg.accounts.len(), 2);
        assert_eq!(tg.accounts[0].id, "alice-tg-bot");
        assert_eq!(tg.accounts[0].dm_policy, DmPolicy::Allowlist);
        assert_eq!(tg.accounts[0].allow_from, vec!["tg:123456789"]);
        assert_eq!(tg.accounts[1].dm_policy, DmPolicy::Open);
        // Default dm_policy is owner_only when omitted.
        let slack = cfg.channels.get("slack").expect("slack entry");
        assert_eq!(slack.accounts[0].dm_policy, DmPolicy::OwnerOnly);
    }

    #[test]
    fn bindings_parse_as_top_level_array() {
        // Spec §3.5 — bindings live at the top level (not nested per
        // channel) so cross-channel agents read naturally.
        let s = r#"
            [[bindings]]
            agent_id = "019def5c-c91a-74e1-9389-a41ac23d3e2c"
            channel = "telegram"
            account_id = "alice-tg-bot"

            [[bindings]]
            agent_id = "019def5c-c91a-74e1-9389-a41ac23d3e2c"
            channel = "whatsapp"
            account_id = "alice-wa-account"
        "#;
        let cfg: GatewayConfig = toml::from_str(s).expect("parse");
        assert_eq!(cfg.bindings.len(), 2);
        assert_eq!(cfg.bindings[0].agent_id, cfg.bindings[1].agent_id);
        assert_eq!(cfg.bindings[0].channel, "telegram");
        assert_eq!(cfg.bindings[1].channel, "whatsapp");
    }

    #[test]
    fn dangling_binding_to_unknown_channel_flagged() {
        let mut cfg = GatewayConfig::default();
        cfg.bindings.push(ChannelBindingConfig {
            agent_id: "a1".into(),
            channel: "telegram".into(),
            account_id: "x".into(),
        });
        let dangling = dangling_bindings(&cfg);
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].reason, DanglingReason::UnknownChannel);
    }

    #[test]
    fn dangling_binding_to_unknown_account_flagged() {
        let mut cfg = GatewayConfig::default();
        cfg.channels.insert(
            "telegram".into(),
            ChannelEntry {
                accounts: vec![ChannelAccount {
                    id: "alice-tg-bot".into(),
                    adapter_token_ref: "secret:channel:telegram:alice-tg-bot".into(),
                    dm_policy: DmPolicy::Allowlist,
                    allow_from: Vec::new(),
                }],
            },
        );
        cfg.bindings.push(ChannelBindingConfig {
            agent_id: "a1".into(),
            channel: "telegram".into(),
            account_id: "bob-tg-bot".into(), // not in accounts list
        });
        let dangling = dangling_bindings(&cfg);
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].reason, DanglingReason::UnknownAccount);
    }

    #[test]
    fn fully_resolved_binding_passes_validation() {
        let mut cfg = GatewayConfig::default();
        cfg.channels.insert(
            "telegram".into(),
            ChannelEntry {
                accounts: vec![ChannelAccount {
                    id: "alice-tg-bot".into(),
                    adapter_token_ref: "secret:channel:telegram:alice-tg-bot".into(),
                    dm_policy: DmPolicy::OwnerOnly,
                    allow_from: Vec::new(),
                }],
            },
        );
        cfg.bindings.push(ChannelBindingConfig {
            agent_id: "a1".into(),
            channel: "telegram".into(),
            account_id: "alice-tg-bot".into(),
        });
        assert!(dangling_bindings(&cfg).is_empty());
    }

    #[test]
    fn unknown_dm_policy_value_rejected() {
        // Typo guard — operator pasting `dm_policy = "alowlist"` should
        // fail config parse, not silently default to owner_only.
        let s = r#"
            [[channels.telegram.accounts]]
            id = "x"
            adapter_token_ref = "secret:channel:telegram:x"
            dm_policy = "alowlist"
        "#;
        let r: Result<GatewayConfig, _> = toml::from_str(s);
        assert!(r.is_err(), "typo should not silently parse");
    }

    fn write_tmp_config(body: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "havn-gateway-config-test-{}.toml",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&p, body).expect("write tmp config");
        p
    }

    #[test]
    fn load_from_rejects_pipe_in_channel_name() {
        // `|` is the channel_id_codec separator (channel_id_codec.rs).
        // A misconfigured TOML with `|` in a channel key would corrupt
        // the encoded channel_id and surface as a confusing "no
        // binding" warning at first message. Fail at load instead.
        let p = write_tmp_config(
            r#"
            [[channels."tele|gram".accounts]]
            id = "alice"
            adapter_token_ref = "secret:channel:telegram:alice"
            "#,
        );
        let r = GatewayConfig::load_from(&p);
        assert!(r.is_err(), "expected rejection of pipe in channel key");
        let msg = format!("{:#}", r.unwrap_err());
        assert!(
            msg.contains("tele|gram"),
            "msg should name the bad value: {msg}"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_from_rejects_pipe_in_account_id() {
        let p = write_tmp_config(
            r#"
            [[channels.telegram.accounts]]
            id = "alice|bot"
            adapter_token_ref = "secret:channel:telegram:alice"
            "#,
        );
        let r = GatewayConfig::load_from(&p);
        assert!(r.is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_from_rejects_pipe_in_binding_fields() {
        let p = write_tmp_config(
            r#"
            [[bindings]]
            agent_id = "019def5c-c91a-74e1-9389-a41ac23d3e2c"
            channel = "telegram"
            account_id = "alice|bad"
            "#,
        );
        let r = GatewayConfig::load_from(&p);
        assert!(r.is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_from_accepts_well_formed_config() {
        let p = write_tmp_config(
            r#"
            [[channels.telegram.accounts]]
            id = "alice-tg-bot"
            adapter_token_ref = "secret:channel:telegram:alice-tg-bot"

            [[bindings]]
            agent_id = "019def5c-c91a-74e1-9389-a41ac23d3e2c"
            channel = "telegram"
            account_id = "alice-tg-bot"
            "#,
        );
        let r = GatewayConfig::load_from(&p);
        assert!(r.is_ok(), "well-formed config should load: {:?}", r.err());
        let _ = std::fs::remove_file(&p);
    }
}
