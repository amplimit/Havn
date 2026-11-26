//! Secrets for an agent's tools (issue #233): the gateway's half.
//!
//! The owner sets and removes them on the dashboard; the agent asks for
//! one it lacks with the `secret_request` tool, and the owner enters the
//! value there, never in a chat. Values are kept in `agent_secrets`,
//! encrypted with the gateway's `HAVN_AGE_KEY` as credentials are.
//!
//! [`SecretStore`] is the one place a value is read: the runtime gets
//! every value of its agent (in `Welcome`, and again on each change),
//! resolves `secret:<name>` in MCP server env and the shell environment,
//! and redacts the values in tool output and its log. Values are
//! decrypted once per agent and kept in memory (each age decryption runs
//! scrypt for about a second); a change goes to the database and to the
//! kept copy under the same lock.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use havn_core::AgentId;
use havn_core::secret_ref::{self, SecretRef};
use havn_db::repo::agent_secrets::{self, Was};
use havn_db::repo::agents::Agent;
use havn_proto::{GatewayToAgent, GatewayToolCall, GatewayToolSpec, Secrets};
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::warn;

use crate::agent_registry::AgentRegistry;
use crate::keyring::KeyRing;

pub const TOOL_NAME: &str = "secret_request";

/// Largest value of a secret (a PEM key fits).
pub const MAX_VALUE_BYTES: usize = 16 * 1024;
/// Characters of the reason the agent gives for asking.
const MAX_REASON_CHARS: usize = 500;
/// Variables in an agent's shell environment, at most.
pub const MAX_SHELL_ENV: usize = 64;
/// Characters of a shell variable's name, and bytes of its value.
const MAX_ENV_NAME_CHARS: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 4096;
/// Variables the runtime sets for tool processes itself.
const RESERVED_ENV: [&str; 8] = [
    "HOME",
    "XDG_CACHE_HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

type Values = BTreeMap<String, String>;

/// The agents' secrets: the database, and the values decrypted so far.
#[derive(Clone)]
pub struct SecretStore {
    db: SqlitePool,
    keyring: Option<KeyRing>,
    /// Per agent, its values once read; each slot's lock serializes the
    /// agent's reads and changes.
    slots: Arc<Mutex<HashMap<AgentId, Arc<Mutex<Option<Values>>>>>>,
    /// The OAuth tokens of agents' remote MCP servers (issue #234).
    oauth: crate::mcp_oauth::McpOAuth,
    /// Policies are clamped to their owners' (`trust_header`).
    multi_user: bool,
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("encrypts", &self.keyring.is_some())
            .finish_non_exhaustive()
    }
}

impl SecretStore {
    pub fn new(db: SqlitePool, keyring: Option<KeyRing>) -> Self {
        Self {
            oauth: crate::mcp_oauth::McpOAuth::new(db.clone(), keyring.clone()),
            db,
            keyring,
            slots: Arc::default(),
            multi_user: false,
        }
    }

    /// Resolve agents' policies as a multi-user gateway does.
    #[must_use]
    pub fn with_multi_user(mut self, multi_user: bool) -> Self {
        self.multi_user = multi_user;
        self
    }

    /// The OAuth of the agents' remote MCP servers.
    pub fn oauth(&self) -> &crate::mcp_oauth::McpOAuth {
        &self.oauth
    }

    /// Whether secrets can be set: `HAVN_AGE_KEY` is.
    pub fn encrypts(&self) -> bool {
        self.keyring.is_some()
    }

    async fn slot(&self, agent_id: AgentId) -> Arc<Mutex<Option<Values>>> {
        Arc::clone(self.slots.lock().await.entry(agent_id).or_default())
    }

    /// Decrypt the agent's values from the database. One that cannot be
    /// read is left out and logged; without a key there are none (the
    /// gateway does not start with secrets and no key).
    async fn load(&self, agent_id: AgentId) -> Result<Values, String> {
        let Some(ring) = self.keyring.clone() else {
            return Ok(Values::new());
        };
        let rows = agent_secrets::list(&self.db, agent_id)
            .await
            .map_err(|e| format!("reading the agent's secrets failed: {e}"))?;
        let mut values = Values::new();
        for row in rows {
            let Some(ciphertext) = row.value_ciphertext else {
                continue;
            };
            let ring = ring.clone();
            let plain = tokio::task::spawn_blocking(move || ring.decrypt(&ciphertext)).await;
            match plain {
                Ok(Ok(bytes)) => match String::from_utf8(bytes) {
                    Ok(value) => {
                        values.insert(row.name, value);
                    }
                    Err(_) => {
                        warn!(%agent_id, secret = %row.name, "an agent secret is not text; left out")
                    }
                },
                Ok(Err(e)) => {
                    warn!(%agent_id, secret = %row.name, error = %e, "an agent secret cannot be decrypted; left out")
                }
                Err(e) => {
                    warn!(%agent_id, secret = %row.name, error = %e, "decrypting an agent secret failed; left out")
                }
            }
        }
        Ok(values)
    }

    /// The agent's values held in `held`, read first when they are not.
    async fn loaded(&self, agent_id: AgentId, held: &mut Option<Values>) -> Result<Values, String> {
        if let Some(values) = held.as_ref() {
            return Ok(values.clone());
        }
        let values = self.load(agent_id).await?;
        *held = Some(values.clone());
        Ok(values)
    }

    /// Every value set for the agent, by name.
    pub async fn values(&self, agent_id: AgentId) -> Result<Values, String> {
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        self.loaded(agent_id, &mut held).await
    }

    /// Set the agent's secret `name` to `value`, trimmed: what is used and
    /// what is redacted are the same. Returns what it was. Refused when the
    /// agent's secrets would take more than [`MAX_TOTAL_BYTES`].
    pub async fn set(&self, agent_id: AgentId, name: &str, value: &str) -> Result<Was, SetError> {
        let value = value.trim();
        let ring = self.keyring.clone().ok_or(SetError::NoKey)?;
        let plain = value.as_bytes().to_vec();
        let ciphertext = tokio::task::spawn_blocking(move || ring.encrypt(&plain))
            .await
            .map_err(|e| SetError::Encrypt(e.to_string()))?
            .map_err(|e| SetError::Encrypt(e.to_string()))?;
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let values = self
            .loaded(agent_id, &mut held)
            .await
            .map_err(SetError::Read)?;
        let total: usize = values
            .iter()
            .filter(|(n, _)| n.as_str() != name)
            .map(|(_, v)| escaped_len(v))
            .sum::<usize>()
            + escaped_len(value);
        if total > MAX_TOTAL_BYTES {
            return Err(SetError::TooLarge(total));
        }
        let was = agent_secrets::set(&self.db, agent_id, name, &ciphertext)
            .await
            .map_err(SetError::Db)?;
        if let Some(values) = held.as_mut() {
            values.insert(name.to_string(), value.to_string());
        }
        Ok(was)
    }

    /// Remove the agent's secret `name`, or the request for it.
    pub async fn remove(&self, agent_id: AgentId, name: &str) -> Result<Was, havn_db::DbError> {
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let was = agent_secrets::remove(&self.db, agent_id, name).await?;
        if let Some(values) = held.as_mut() {
            values.remove(name);
        }
        Ok(was)
    }

    /// Forget what was read of a deleted agent.
    pub async fn forget(&self, agent_id: AgentId) {
        self.slots.lock().await.remove(&agent_id);
        self.oauth.forget(agent_id).await;
    }

    /// What the runtime of `agent` gets, under `policy`: its values, shell
    /// environment, and the OAuth tokens of its own MCP servers. Values that
    /// cannot be read are left out, logged.
    pub async fn for_runtime(&self, agent: &Agent, policy: &havn_core::Policy) -> Secrets {
        let values = match self.values(agent.id).await {
            Ok(values) => values,
            Err(e) => {
                warn!(agent_id = %agent.id, error = %e, "the agent starts without its secrets");
                Values::new()
            }
        };
        let own = own_mcp_servers(&agent.config, policy);
        Secrets {
            values,
            shell_env: shell_env(&agent.config),
            mcp_tokens: self.oauth.tokens(agent.id, policy, &own).await,
        }
    }

    /// Send the running runtime of `agent_id` its secrets again, after a
    /// change. What it sends is read, and sent, under the agent's lock, so
    /// sends go out in the order of the changes and a stale set never
    /// follows a newer one. A runtime that is not connected gets them in
    /// its next `Welcome`; one that stalls is disconnected by the registry,
    /// and gets them when it connects again.
    pub async fn push(&self, registry: &AgentRegistry, agent_id: AgentId) {
        if !registry.is_connected(agent_id).await {
            return;
        }
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let agent = match havn_db::repo::agents::find_by_id(&self.db, agent_id).await {
            Ok(Some(agent)) => agent,
            Ok(None) => return,
            Err(e) => {
                warn!(%agent_id, error = %e, "could not read the agent to send its secrets");
                return;
            }
        };
        let values = match self.loaded(agent_id, &mut held).await {
            Ok(values) => values,
            Err(e) => {
                warn!(%agent_id, error = %e, "could not read the agent's secrets to send them");
                return;
            }
        };
        let policy = crate::policy_resolver::for_session(&self.db, &agent, self.multi_user).await;
        let own = own_mcp_servers(&agent.config, &policy);
        let secrets = Secrets {
            values,
            shell_env: shell_env(&agent.config),
            mcp_tokens: self.oauth.tokens(agent_id, &policy, &own).await,
        };
        if let Err(e) = registry
            .send(agent_id, GatewayToAgent::Secrets(secrets))
            .await
        {
            warn!(%agent_id, error = ?e, "could not send the agent its secrets");
        }
    }
}

/// Bytes an agent's secrets may take in all, as JSON escapes them in the
/// frames that carry them to its runtime (each frame at most 4 MiB).
pub const MAX_TOTAL_BYTES: usize = 512 * 1024;

/// Bytes `value` takes in a frame, escaped as JSON.
fn escaped_len(value: &str) -> usize {
    serde_json::to_string(value).map_or(value.len() * 6, |s| s.len())
}

#[derive(Debug, thiserror::Error)]
pub enum SetError {
    #[error("secrets are not encrypted on this gateway: set HAVN_AGE_KEY and restart the gateway")]
    NoKey,
    #[error("encrypting the secret failed: {0}")]
    Encrypt(String),
    #[error("{0}")]
    Read(String),
    #[error(
        "an agent's secrets take at most {} KB in all; with this one they would take {} KB",
        MAX_TOTAL_BYTES / 1024,
        .0.div_ceil(1024)
    )]
    TooLarge(usize),
    #[error(transparent)]
    Db(havn_db::DbError),
}

/// The agent's shell environment: `config.shell_env`, whose values are
/// text or `secret:<name>`.
pub fn shell_env(config: &serde_json::Value) -> BTreeMap<String, String> {
    config
        .get("shell_env")
        .and_then(serde_json::Value::as_object)
        .map(|env| {
            env.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// The MCP servers of `policy` (the agent's, as resolved) that the agent's
/// own settings (`config.policy.mcp_servers`) define as they run: only
/// their `secret:<name>` references are resolved. In multi-user mode a
/// server runs with the owner's role's settings, which a team admin
/// writes; the owner's secret goes to one only where the owner wrote the
/// same entry (issue #233).
pub fn own_mcp_servers(config: &serde_json::Value, policy: &havn_core::Policy) -> Vec<String> {
    let own: HashMap<String, havn_core::McpServerConfig> = config
        .pointer("/policy/mcp_servers")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let mut names: Vec<String> = policy
        .mcp_servers
        .iter()
        .filter(|(name, runs)| own.get(*name).is_some_and(|o| o.runs_as(runs)))
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names
}

/// The enabled MCP servers of `policy` that refer to secrets in their
/// `env` but are not the agent's own ([`own_mcp_servers`]): they do not
/// start, as their references are not resolved.
pub fn refused_mcp_servers(config: &serde_json::Value, policy: &havn_core::Policy) -> Vec<String> {
    let own = own_mcp_servers(config, policy);
    let mut names: Vec<String> = policy
        .mcp_servers
        .iter()
        .filter(|(name, server)| server.enabled && server.uses_secrets() && !own.contains(*name))
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names
}

/// The shell variables of `config` that get the secret `name`.
pub fn variables_of(config: &serde_json::Value, name: &str) -> Vec<String> {
    let reference = format!("{}{name}", secret_ref::PREFIX);
    shell_env(config)
        .into_iter()
        .filter(|(_, v)| *v == reference)
        .map(|(k, _)| k)
        .collect()
}

/// A value the owner may set: not blank, no NUL (no environment variable
/// can hold one), at most [`MAX_VALUE_BYTES`].
pub fn check_value(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("the value is empty".into());
    }
    if value.contains('\0') {
        return Err("the value contains a NUL character".into());
    }
    if value.len() > MAX_VALUE_BYTES {
        return Err(format!("the value is longer than {MAX_VALUE_BYTES} bytes"));
    }
    Ok(())
}

/// A shell variable's name: a letter or `_`, then letters, digits and `_`;
/// not one the runtime sets itself.
pub fn check_env_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let starts = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "{name:?} is not a variable name: a letter or `_`, then letters, digits and `_`"
        ));
    }
    if name.chars().count() > MAX_ENV_NAME_CHARS {
        return Err(format!(
            "a variable name is at most {MAX_ENV_NAME_CHARS} characters"
        ));
    }
    if RESERVED_ENV.contains(&name) {
        return Err(format!("{name} is set by Havn for every tool process"));
    }
    Ok(())
}

/// A shell environment the owner may set: at most [`MAX_SHELL_ENV`]
/// variables, each value text or a reference to one of the agent's own
/// secrets.
pub fn check_shell_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    if env.len() > MAX_SHELL_ENV {
        return Err(format!(
            "a shell environment has at most {MAX_SHELL_ENV} variables"
        ));
    }
    for (name, value) in env {
        check_env_name(name)?;
        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(format!(
                "{name}: a value is at most {MAX_ENV_VALUE_BYTES} bytes"
            ));
        }
        if value.contains('\0') {
            return Err(format!("{name}: the value contains a NUL character"));
        }
        if value.starts_with(secret_ref::PREFIX) {
            match secret_ref::parse(value) {
                Ok(SecretRef::Agent { .. }) => {}
                Ok(SecretRef::Credential { .. }) => {
                    return Err(format!(
                        "{name}: {value:?} names a gateway credential; use `secret:<name>` \
                         with one of the agent's secrets"
                    ));
                }
                Err(e) => return Err(format!("{name}: {e}")),
            }
        }
    }
    Ok(())
}

/// A `config.shell_env` as JSON, checked.
pub fn read_shell_env(raw: &serde_json::Value) -> Result<BTreeMap<String, String>, String> {
    let env: BTreeMap<String, String> = serde_json::from_value(raw.clone())
        .map_err(|_| "shell_env maps variable names to text".to_string())?;
    check_shell_env(&env)?;
    Ok(env)
}

#[must_use]
pub fn spec() -> GatewayToolSpec {
    GatewayToolSpec {
        name: TOOL_NAME.into(),
        description: "Ask your owner for a secret you lack, such as an API token a command or \
                      MCP server needs. The owner enters it on the dashboard, never in a chat: \
                      do not ask anyone to send a secret in a message. Once set, shell \
                      commands get it as the variable the owner gives it (`env` asks for \
                      one), and MCP servers through `secret:<name>` in their env. Its value \
                      shows as [secret:<name>] in tool output. Tell the person you are \
                      talking to that you asked."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The secret's name: letters, digits, `_`, `-` or `.`, \
                                    such as github."
                },
                "reason": {
                    "type": "string",
                    "description": "What it is for, in a sentence the owner reads: which \
                                    service, and what you will do with it."
                },
                "env": {
                    "type": "string",
                    "description": "The variable shell commands should get it as, such as \
                                    GH_TOKEN."
                }
            },
            "required": ["name", "reason"],
            "additionalProperties": false
        }),
    }
}

/// Who a tool call answered, as the owner sees it with a request: a name
/// (the owner's, or the Havn user a chat-app sender is linked to, with the
/// sender's id) or the sender's id; where (`web chat`, the chat app,
/// `group on <app>`, `webhook`); and whether the turn was answering the
/// owner (which does not make the owner the one who asked: what the agent
/// read in the turn can lead it to). Nobody for a scheduled task or a
/// heartbeat.
pub async fn asked_by(
    db: &SqlitePool,
    agent: &Agent,
    call: &GatewayToolCall,
) -> (Option<String>, Option<String>, bool) {
    let by_owner = call.from_owner;
    let Some(conversation) = call.conversation.as_deref() else {
        return (call.sender.clone(), None, false);
    };
    if conversation.starts_with(crate::agent_socket::WEBHOOK_CONVERSATION) {
        return (call.sender.clone(), Some("webhook".into()), false);
    }
    if let Some((channel, _, peer)) = crate::channel_id_codec::decode(conversation) {
        let sender = call.sender.clone().unwrap_or_else(|| peer.to_string());
        let name = match havn_db::repo::channel_access::linked_user(db, channel, &sender).await {
            Ok(Some(link)) => format!("{} ({sender})", link.user_name),
            _ => sender,
        };
        let place = if call.in_group {
            format!("group on {channel}")
        } else {
            channel.to_string()
        };
        return (Some(name), Some(place), by_owner);
    }
    if conversation.parse::<crate::webchat::SessionId>().is_ok() {
        let owner = havn_db::repo::users::find_by_id(db, agent.owner_id)
            .await
            .ok()
            .flatten()
            .map(|u| u.display_name);
        return (owner, Some("web chat".into()), by_owner);
    }
    (call.sender.clone(), None, by_owner)
}

/// `secret_request`: record that `agent` asked for a secret, for its owner
/// to set on the dashboard, with whom it was answering. Returns what the
/// model is told, and what it was before.
pub async fn run_request(
    db: &SqlitePool,
    agent: &Agent,
    call: &GatewayToolCall,
) -> Result<(String, Was), String> {
    let input = &call.input;
    let field = |key: &str| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let name = field("name").ok_or("missing required field: name")?;
    secret_ref::check_name(name)?;
    let reason = field("reason").ok_or("missing required field: reason")?;
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(format!(
            "the reason is longer than {MAX_REASON_CHARS} characters"
        ));
    }
    let env = field("env");
    if let Some(env) = env {
        check_env_name(env)?;
    }
    let (by, place, by_owner) = asked_by(db, agent, call).await;
    let was = agent_secrets::request(
        db,
        agent.id,
        name,
        agent_secrets::NewRequest {
            reason,
            env,
            asked_by: by.as_deref(),
            asked_in: place.as_deref(),
            by_owner,
        },
    )
    .await
    .map_err(|e| match e {
        havn_db::DbError::Conflict(why) => why.to_string(),
        other => format!("recording the request failed: {other}"),
    })?;
    let reference = format!("`{}{name}`", secret_ref::PREFIX);
    if was == Was::Requested {
        return Ok((
            format!(
                "You asked for the secret `{name}` already, and the owner has not set it yet; \
                 the dashboard keeps your first reason. Go on without it."
            ),
            was,
        ));
    }
    if was == Was::Set {
        let vars = variables_of(&agent.config, name);
        let reach = if vars.is_empty() {
            format!(
                "No shell variable gets it yet: the owner gives it one on the dashboard. MCP \
                 servers get it through {reference} in their env."
            )
        } else {
            format!(
                "Shell commands get it as {}.",
                vars.iter()
                    .map(|v| format!("${v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Ok((format!("The secret `{name}` is set already. {reach}"), was));
    }
    let reach = match env {
        Some(env) => {
            format!("Once set, shell commands get it as ${env} if the owner keeps that variable.")
        }
        None => format!(
            "Once set, shell commands get it as the variable the owner gives it, and MCP \
             servers through {reference} in their env."
        ),
    };
    Ok((
        format!(
            "Asked. Your owner enters the secret `{name}` on the dashboard (this agent's \
             settings, Secrets); it never passes through a chat. {reach} Until then, go on \
             without it, and tell the person you are talking to that you asked."
        ),
        was,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_db::connect_in_memory;
    use havn_db::repo::agents::{NewAgent, create as create_agent};
    use havn_db::repo::users::{NewUser, create as create_user};

    async fn agent(db: &SqlitePool, config: serde_json::Value) -> Agent {
        let owner = create_user(db, NewUser { display_name: "o" })
            .await
            .expect("user");
        create_agent(
            db,
            NewAgent {
                owner_id: owner.id,
                team_id: None,
                name: "a",
                config,
            },
        )
        .await
        .expect("agent")
    }

    #[test]
    fn shell_environments_are_checked() {
        let ok = BTreeMap::from([
            ("GH_TOKEN".to_string(), "secret:github".to_string()),
            ("REGION".to_string(), "eu-west-1".to_string()),
        ]);
        assert!(check_shell_env(&ok).is_ok());
        for (name, value) in [
            ("1BAD", "x"),
            ("HAS-DASH", "x"),
            ("HOME", "/tmp"),
            ("https_proxy", "http://x"),
            ("TOKEN", "secret:channel:telegram:bot"),
            ("TOKEN", "secret:-bad"),
        ] {
            let env = BTreeMap::from([(name.to_string(), value.to_string())]);
            assert!(check_shell_env(&env).is_err(), "{name}={value} passed");
        }
        assert!(read_shell_env(&json!({"A": 1})).is_err());
        assert_eq!(read_shell_env(&json!({"A": "b"})).expect("ok")["A"], "b");
    }

    #[tokio::test]
    async fn values_are_kept_encrypted_and_read_back_once() {
        let db = connect_in_memory().await.expect("db");
        let a = agent(&db, json!({"shell_env": {"GH_TOKEN": "secret:github"}})).await;
        let store = SecretStore::new(db.clone(), Some(KeyRing::from_passphrase("test-key")));
        assert_eq!(
            store
                .set(a.id, "github", "ghp_0123456789")
                .await
                .expect("set"),
            Was::Absent
        );
        let row = agent_secrets::find(&db, a.id, "github")
            .await
            .expect("find")
            .expect("row");
        let stored = row.value_ciphertext.expect("value");
        assert!(crate::keyring::is_age_ciphertext(&stored));
        assert!(
            !stored
                .windows(b"ghp_0123456789".len())
                .any(|w| w == b"ghp_0123456789")
        );

        // Another store over the same database decrypts it.
        let fresh = SecretStore::new(db.clone(), Some(KeyRing::from_passphrase("test-key")));
        let secrets = fresh.for_runtime(&a, &havn_core::Policy::default()).await;
        assert_eq!(secrets.values["github"], "ghp_0123456789");
        assert_eq!(secrets.shell_env["GH_TOKEN"], "secret:github");

        assert_eq!(fresh.remove(a.id, "github").await.expect("rm"), Was::Set);
        assert!(fresh.values(a.id).await.expect("values").is_empty());

        let without_key = SecretStore::new(db, None);
        assert!(matches!(
            without_key.set(a.id, "x", "value-123").await,
            Err(SetError::NoKey)
        ));
    }

    fn call(
        input: serde_json::Value,
        conversation: Option<String>,
        owner: bool,
    ) -> GatewayToolCall {
        GatewayToolCall {
            request_id: "r".into(),
            name: TOOL_NAME.into(),
            input,
            conversation,
            chat: None,
            sender: Some("tg:42".into()),
            from_owner: owner,
            in_group: false,
        }
    }

    #[tokio::test]
    async fn the_agent_asks_and_is_told_where_a_set_secret_reaches() {
        let db = connect_in_memory().await.expect("db");
        let a = agent(&db, json!({"shell_env": {"GH_TOKEN": "secret:github"}})).await;
        let telegram = Some(crate::channel_id_codec::encode("telegram", "bot", "tg:42"));
        let (text, was) = run_request(
            &db,
            &a,
            &call(
                json!({"name": "github", "reason": "to open a PR", "env": "GH_TOKEN"}),
                telegram.clone(),
                false,
            ),
        )
        .await
        .expect("asked");
        assert_eq!(was, Was::Absent);
        assert!(text.contains("dashboard") && text.contains("$GH_TOKEN"));
        let request = agent_secrets::find(&db, a.id, "github")
            .await
            .expect("find")
            .expect("row")
            .request
            .expect("asked");
        assert_eq!(request.asked_by.as_deref(), Some("tg:42"));
        assert_eq!(request.asked_in.as_deref(), Some("telegram"));
        assert!(!request.by_owner);

        // Asked again, from the owner's web chat: the first request stays.
        let web = Some(uuid::Uuid::now_v7().to_string());
        let (text, was) = run_request(
            &db,
            &a,
            &call(
                json!({"name": "github", "reason": "other", "env": "OTHER"}),
                web.clone(),
                true,
            ),
        )
        .await
        .expect("again");
        assert_eq!(was, Was::Requested);
        assert!(text.contains("already"));
        let request = agent_secrets::find(&db, a.id, "github")
            .await
            .expect("find")
            .expect("row")
            .request
            .expect("asked");
        assert_eq!(request.reason, "to open a PR");
        assert_eq!(request.env.as_deref(), Some("GH_TOKEN"));
        assert_eq!(request.again, 1);

        let (by, place, owner) = asked_by(&db, &a, &call(json!({}), web, true)).await;
        assert_eq!(
            (by.as_deref(), place.as_deref(), owner),
            (Some("o"), Some("web chat"), true)
        );
        // The owner's own message in a group: answering the owner, there.
        let mut in_group = call(json!({}), telegram.clone(), true);
        in_group.in_group = true;
        let (_, place, owner) = asked_by(&db, &a, &in_group).await;
        assert_eq!((place.as_deref(), owner), (Some("group on telegram"), true));

        assert!(
            run_request(
                &db,
                &a,
                &call(json!({"name": "bad name", "reason": "x"}), None, true)
            )
            .await
            .is_err()
        );
        assert!(
            run_request(&db, &a, &call(json!({"name": "github"}), None, true))
                .await
                .is_err()
        );

        agent_secrets::set(&db, a.id, "github", b"cipher")
            .await
            .expect("set");
        let (text, was) = run_request(
            &db,
            &a,
            &call(
                json!({"name": "github", "reason": "again"}),
                telegram,
                false,
            ),
        )
        .await
        .expect("told");
        assert_eq!(was, Was::Set);
        assert!(text.contains("set already") && text.contains("$GH_TOKEN"));
    }

    #[test]
    fn only_entries_the_agent_defines_itself_get_its_secrets() {
        let server = |url: &str| havn_core::McpServerConfig {
            binary: "gh".into(),
            args: vec![url.into()],
            env: HashMap::from([("TOKEN".to_string(), "secret:github".to_string())]),
            ..Default::default()
        };
        let config = json!({"policy": {"mcp_servers": {"gh": server("https://api.github.com")}}});
        let mut policy = havn_core::Policy::default();
        policy.mcp_servers = HashMap::from([("gh".to_string(), server("https://api.github.com"))]);
        assert_eq!(own_mcp_servers(&config, &policy), vec!["gh".to_string()]);
        assert!(refused_mcp_servers(&config, &policy).is_empty());
        // The role's entry, as a team admin wrote it, sends it elsewhere.
        policy.mcp_servers = HashMap::from([("gh".to_string(), server("https://evil.example"))]);
        assert!(own_mcp_servers(&config, &policy).is_empty());
        assert_eq!(
            refused_mcp_servers(&config, &policy),
            vec!["gh".to_string()]
        );
        assert_eq!(
            refused_mcp_servers(&json!({}), &policy),
            vec!["gh".to_string()]
        );
    }

    #[test]
    fn a_role_cannot_point_the_owners_oauth_server_elsewhere() {
        let server = |url: &str| havn_core::McpServerConfig {
            url: url.into(),
            auth: havn_core::McpAuth::Oauth(havn_core::McpOAuth::default()),
            ..Default::default()
        };
        let config =
            json!({"policy": {"mcp_servers": {"notion": server("https://mcp.notion.com/mcp")}}});
        let mut policy = havn_core::Policy::default();
        policy.mcp_servers =
            HashMap::from([("notion".to_string(), server("https://mcp.notion.com/mcp"))]);
        assert_eq!(
            own_mcp_servers(&config, &policy),
            vec!["notion".to_string()]
        );
        policy.mcp_servers =
            HashMap::from([("notion".to_string(), server("https://evil.example/mcp"))]);
        assert!(own_mcp_servers(&config, &policy).is_empty());
        assert_eq!(
            refused_mcp_servers(&config, &policy),
            vec!["notion".to_string()]
        );
    }

    #[tokio::test]
    async fn the_runtime_gets_oauth_tokens_of_its_own_servers_at_their_url_only() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let a = agent(&db, json!({})).await;
        let store = SecretStore::new(db.clone(), Some(KeyRing::from_passphrase("test-key")));
        let server = |url: &str| havn_core::McpServerConfig {
            url: url.into(),
            auth: havn_core::McpAuth::Oauth(havn_core::McpOAuth::default()),
            ..Default::default()
        };
        let mut policy = havn_core::Policy::default();
        policy.mcp_servers = HashMap::from([
            ("notion".to_string(), server("https://mcp.notion.com/mcp")),
            ("moved".to_string(), server("https://mcp.example.com/v2")),
        ]);
        let own = vec!["notion".to_string(), "moved".to_string()];
        // Nothing connected: no tokens.
        assert!(store.oauth().tokens(a.id, &policy, &own).await.is_empty());
        store
            .oauth()
            .keep_for_test(
                a.id,
                "notion",
                "https://mcp.notion.com/mcp",
                "oauth-access-1",
            )
            .await;
        store
            .oauth()
            .keep_for_test(
                a.id,
                "moved",
                "https://mcp.example.com/v1",
                "oauth-access-2",
            )
            .await;
        let tokens = store.oauth().tokens(a.id, &policy, &own).await;
        assert_eq!(
            tokens.len(),
            1,
            "a token goes only to the URL it was issued for"
        );
        assert_eq!(tokens["notion"].access_token, "oauth-access-1");
        // A server that is not the agent's own gets none.
        let theirs = store
            .oauth()
            .tokens(a.id, &policy, &["moved".to_string()])
            .await;
        assert!(theirs.is_empty());
        // Stored encrypted, decrypted by another store.
        let rows = agent_secrets::oauth_list(&db, a.id).await.expect("rows");
        assert!(
            rows.iter()
                .all(|r| crate::keyring::is_age_ciphertext(&r.value_ciphertext))
        );
        let fresh = SecretStore::new(db.clone(), Some(KeyRing::from_passphrase("test-key")));
        let tokens = fresh.oauth().tokens(a.id, &policy, &own).await;
        assert_eq!(tokens["notion"].access_token, "oauth-access-1");
        assert!(
            fresh
                .oauth()
                .disconnect(a.id, "notion")
                .await
                .expect("disconnect")
        );
        assert!(fresh.oauth().tokens(a.id, &policy, &own).await.is_empty());
    }

    #[tokio::test]
    async fn values_are_trimmed_and_their_total_size_is_capped() {
        let db = connect_in_memory().await.expect("db");
        let a = agent(&db, json!({})).await;
        let store = SecretStore::new(db.clone(), Some(KeyRing::from_passphrase("test-key")));
        store
            .set(a.id, "t", "  tok-0123456789\n")
            .await
            .expect("set");
        assert_eq!(
            store.values(a.id).await.expect("values")["t"],
            "tok-0123456789"
        );
        // Control characters take six bytes each in a frame.
        let big = "\u{1}".repeat(MAX_TOTAL_BYTES / 6 + 10);
        assert!(matches!(
            store.set(a.id, "big", &format!("x{big}x")).await,
            Err(SetError::TooLarge(_))
        ));
    }
}
