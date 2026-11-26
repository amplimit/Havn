//! `/agents/:id/mcp` — read & write the agent's MCP configuration
//! (spec §13 Phase 3).
//!
//! This endpoint pair gives the dashboard a focused window into the
//! agent's `policy.permissions.can_use_mcp` flag + `policy.mcp_servers`
//! map without making the operator hand-edit the SQLite row. Both
//! fields live as JSON inside `agent.config.policy`; the per-session
//! resolver (`policy_resolver::override_for`) lays them over the owner's
//! policy at every agent start (spec §6.3 / §9.4 frozen-prompt invariant —
//! edits land on the next restart). The view shows the override's own
//! values, which the editor sends back whole; servers the owner's role
//! grants without the override naming them are not listed.
//!
//! Writes never touch unrelated policy keys (allowed_models,
//! resource_limits, …) — we read agent.config, splice in only the
//! `policy.permissions.can_use_mcp` and `policy.mcp_servers` fields,
//! and write back. Owner-only auth via `agents::resolve_agent`.
//!
//! `available_binaries` lists names under `/usr/share/havn/mcp-servers/`
//! at GET time so the dashboard can present a dropdown of binaries
//! the operator already pre-installed (matches the spec §13 bonus
//! decision: "agent self can't acquire new MCP servers — only
//! whitelisted ones already on the host").

//!
//! Remote servers (issue #234) are entries with a `url`. One whose `auth`
//! is OAuth is connected by its owner: `POST /agents/:id/mcp/:server/oauth`
//! answers the URL to send the browser to, the provider sends it back to
//! `GET /mcp/oauth/callback`, and `DELETE /agents/:id/mcp/:server/oauth`
//! forgets the tokens.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use havn_core::{McpAuth, McpServerConfig};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use tokio::fs;
use tracing::{info, warn};

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::mcp_oauth::{OAuthError, OAuthStatus};

/// Same fixed path the runtime uses (`havn_runtime::mcp::MCP_SERVERS_DIR`)
/// — keep in sync if either ever moves. Re-listing it here rather than
/// pulling in havn-runtime as a gateway dep (would cycle).
const MCP_SERVERS_DIR: &str = "/usr/share/havn/mcp-servers";

#[derive(Debug, Serialize)]
pub struct McpView {
    /// Master gate. Mirrors `policy.permissions.can_use_mcp`. Default
    /// false — agents only see MCP tools when an operator (or owner)
    /// explicitly opts in.
    pub can_use_mcp: bool,
    /// Whitelisted MCP servers, keyed by operator-chosen name.
    pub servers: HashMap<String, McpServerConfig>,
    /// Binaries currently installed at `/usr/share/havn/mcp-servers/`.
    /// Helps the dashboard render a dropdown so the operator doesn't
    /// have to type names. Empty when the directory is missing
    /// (single-user dev install with no servers yet).
    pub available_binaries: Vec<String>,
    /// What the operator allows of remote servers (`[mcp]`).
    pub remote: RemoteView,
    /// How the servers started, while the agent runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<havn_proto::McpServerStatus>>,
    /// The servers connected with OAuth, by name.
    pub oauth: BTreeMap<String, OAuthStatus>,
    /// Servers of the agent that do not run as it has them, and why: the
    /// operator does not allow them, or a role supplies their settings.
    pub refused: BTreeMap<String, String>,
    /// For each remote server, whether the agent's network reaches its
    /// host: `allowed`, `ask` (through the egress proxy, which asks the
    /// owner), `blocked` or `none` (no network access).
    pub reach: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct RemoteView {
    pub allowed: bool,
    /// When not empty, the only hosts allowed.
    pub hosts: Vec<String>,
}

async fn view(state: &AppState, agent: &havn_db::repo::agents::Agent) -> McpView {
    let (can_use_mcp, servers) = read_mcp_from_config(&agent.config);
    let policy =
        crate::policy_resolver::for_session(&state.db, agent, state.trusted_proxies.is_some())
            .await;
    let own = crate::secrets::own_mcp_servers(&agent.config, &policy);
    let mut refused = BTreeMap::new();
    for (name, server) in &servers {
        let why = if let Some(why) = crate::policy_resolver::operator_refuses(server) {
            Some(why)
        } else if !server.enabled {
            None
        } else {
            match policy.mcp_servers.get(name) {
                Some(runs) if runs.uses_secrets() && !own.contains(name) => Some(
                    "it runs with the settings of the owner's role, which do not get the \
                     owner's secrets or OAuth tokens"
                        .to_string(),
                ),
                Some(_) => None,
                None => Some("the owner's role does not list a server of this name".to_string()),
            }
        };
        if let Some(why) = why {
            refused.insert(name.clone(), why);
        }
    }
    let net = &policy.network_policy;
    let reach = servers
        .iter()
        .filter_map(|(name, server)| {
            let host = server.remote_host()?;
            let how = if !(policy.permissions.can_access_network && net.egress_allowed) {
                "none"
            } else if net.is_blocked(&host) {
                "blocked"
            } else if net.egress_proxy && !net.is_allowed(&host) {
                "ask"
            } else {
                "allowed"
            };
            Some((name.clone(), how.to_string()))
        })
        .collect();
    let operator = crate::policy_resolver::operator_mcp();
    McpView {
        can_use_mcp,
        servers,
        available_binaries: list_installed_binaries().await,
        remote: RemoteView {
            allowed: operator.remote == "on",
            hosts: operator.remote_hosts,
        },
        status: state
            .registry
            .mcp_status(agent.id)
            .await
            .map(|status| status.servers),
        oauth: state.secrets.oauth().status(agent.id).await,
        refused,
        reach,
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct McpPatch {
    /// `Some(true)` enables MCP for this agent on its next start;
    /// `Some(false)` disables; `None` leaves the existing value.
    pub can_use_mcp: Option<bool>,
    /// Full replacement for the server map when present. Writers
    /// who want to add/remove a single entry should GET, mutate,
    /// PATCH — keeps the API surface tiny vs introducing a per-
    /// server route.
    pub servers: Option<HashMap<String, McpServerConfig>>,
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<McpView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    Ok(Json(view(&state, &agent).await))
}

pub async fn patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<McpPatch>,
) -> Result<Json<McpView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let id = agent.id;

    // Validate every server config the operator is about to commit.
    // The runtime would also reject these at boot, but failing here
    // means the dashboard can show a precise field-level error before
    // the row is written.
    if let Some(map) = &req.servers {
        for (name, cfg) in map {
            if name.trim().is_empty() {
                return Err(ApiError::BadRequest("server name must be non-empty".into()));
            }
            if !cfg.is_remote() {
                validate_binary_name(&cfg.binary)
                    .map_err(|e| ApiError::BadRequest(format!("server {name:?}: {e}")))?;
            }
            cfg.validate(name)?;
            if let Some(why) = crate::policy_resolver::operator_refuses(cfg) {
                return Err(ApiError::BadRequest(format!("server {name:?}: {why}")));
            }
        }
    }

    // Splice only `policy.permissions.can_use_mcp` + `policy.mcp_servers`
    // into agent.config; leave every other policy key untouched. Build
    // the new config object by surgical mutation.
    let updated = havn_db::repo::agents::update_config(&state.db, id, None, |current| {
        let mut config = current.as_object().cloned().unwrap_or_default();
        let mut policy = config
            .get("policy")
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        if let Some(can) = req.can_use_mcp {
            let mut perms = policy
                .get("permissions")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            perms.insert("can_use_mcp".into(), serde_json::Value::Bool(can));
            policy.insert("permissions".into(), serde_json::Value::Object(perms));
        }
        if let Some(servers) = &req.servers {
            let v = serde_json::to_value(servers)
                .map_err(|e| ApiError::Internal(format!("serialize servers: {e}")))?;
            policy.insert("mcp_servers".into(), v);
        }
        config.insert("policy".into(), serde_json::Value::Object(policy));
        Ok::<_, ApiError>(serde_json::Value::Object(config))
    })
    .await??;
    audit::record_agent_action(
        &state.db,
        user.id,
        id,
        "agent.mcp.updated",
        serde_json::json!({
            "can_use_mcp": req.can_use_mcp,
            "server_names": req.servers.as_ref().map(|m| m.keys().collect::<Vec<_>>()),
        }),
    )
    .await;
    info!(agent_id = %id, "mcp config patched");
    Ok(Json(view(&state, &updated).await))
}

#[derive(Debug, Serialize)]
pub struct Connect {
    /// Where to send the owner's browser.
    pub authorization_url: String,
}

/// `POST /agents/:id/mcp/:server/oauth`: start connecting a server whose
/// `auth` is OAuth. Only the agent's own server, as it runs, at a host its
/// network policy lets it reach. The provider sends the browser back to the
/// origin the dashboard was opened at.
pub async fn connect(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, server)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Connect>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let policy =
        crate::policy_resolver::for_session(&state.db, &agent, state.trusted_proxies.is_some())
            .await;
    let Some(cfg) = policy.mcp_servers.get(&server) else {
        return Err(ApiError::BadRequest(format!(
            "{server:?} is not one of the agent's MCP servers (with a team role, the role must \
             list it, and the gateway's operator must allow remote servers at its host)"
        )));
    };
    if !matches!(cfg.auth, McpAuth::Oauth(_)) || !cfg.is_remote() {
        return Err(ApiError::BadRequest(format!(
            "MCP server {server:?} does not use OAuth"
        )));
    }
    if !crate::secrets::own_mcp_servers(&agent.config, &policy).contains(&server) {
        return Err(ApiError::BadRequest(format!(
            "MCP server {server:?} runs with the settings of the owner's role, which do not get \
             the owner's tokens; define it the same in the agent's MCP settings"
        )));
    }
    let host = cfg.remote_host().unwrap_or_default();
    let net = &policy.network_policy;
    if !(policy.permissions.can_access_network && net.egress_allowed) {
        return Err(ApiError::BadRequest(
            "the agent has no network access, so it could not reach the server".into(),
        ));
    }
    if net.is_blocked(&host) {
        return Err(ApiError::BadRequest(format!(
            "{host} is on the agent's blocked domains"
        )));
    }
    let origin = redirect_origin(&state, &headers)?;
    let redirect_uri = format!("{origin}/api{}", crate::mcp_oauth::CALLBACK_PATH);
    let secrets = state
        .secrets
        .values(agent.id)
        .await
        .map_err(ApiError::Internal)?;
    let authorization_url = state
        .secrets
        .oauth()
        .start(&agent, user.id, &server, cfg, &redirect_uri, &secrets)
        .await
        .map_err(oauth_error)?;
    info!(agent_id = %agent.id, server, "MCP server OAuth started");
    Ok(Json(Connect { authorization_url }))
}

/// The origin the dashboard was opened at, from the request's `Origin`: its
/// own (the gateway's dashboard, or a proxy in front of it), one of
/// `allowed_origins`, or, without `trust_header`, a loopback one (the
/// development server).
fn redirect_origin(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_end_matches('/');
    let Ok(url) = reqwest::Url::parse(origin) else {
        return Err(ApiError::BadRequest(
            "connect from the dashboard: the request has no Origin to send the browser back to"
                .into(),
        ));
    };
    let loopback = state.trusted_proxies.is_none()
        && match url.host() {
            Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
    let listed = state
        .allowed_origins
        .load()
        .iter()
        .any(|o| o.trim_end_matches('/') == origin);
    let web = matches!(url.scheme(), "http" | "https") && url.path() == "/";
    if web && (crate::dashboard::same_origin(origin, headers) || listed || loopback) {
        Ok(origin.to_string())
    } else {
        Err(ApiError::BadRequest(format!(
            "the dashboard's origin {origin} is not this gateway's, nor in allowed_origins"
        )))
    }
}

fn oauth_error(e: OAuthError) -> ApiError {
    match e {
        OAuthError::NoKey => ApiError::Conflict(e.to_string()),
        OAuthError::Internal(why) => ApiError::Internal(why),
        other => ApiError::BadRequest(other.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /mcp/oauth/callback`: the provider sent the owner's browser back.
/// The tokens are kept, and the browser goes on to the agent's settings.
pub async fn callback(
    State(state): State<AppState>,
    user: AuthedUser,
    Query(query): Query<Callback>,
) -> Response {
    let oauth = state.secrets.oauth();
    let Some(key) = query.state.as_deref() else {
        return page("The authorization server sent no state back; connect the server again.");
    };
    if let Some(error) = &query.error {
        let why = query
            .error_description
            .as_deref()
            .map_or_else(|| error.clone(), |d| format!("{error}: {d}"));
        return match oauth.abandon(user.id, key) {
            Some((agent_id, server)) => back(
                agent_id,
                &server,
                Err(format!("the provider refused: {why}")),
            ),
            None => page("This connect is unknown or expired; connect the server again."),
        };
    }
    let Some(code) = query.code.as_deref() else {
        return page("The authorization server sent no code back; connect the server again.");
    };
    let target = oauth.target(user.id, key);
    match oauth.finish(user.id, key, code, query.iss.as_deref()).await {
        Ok((agent_id, server)) => {
            audit::record_agent_action(
                &state.db,
                user.id,
                agent_id,
                "agent.mcp.oauth.connected",
                serde_json::json!({ "server": server }),
            )
            .await;
            state.secrets.push(&state.registry, agent_id).await;
            back(agent_id, &server, Ok(()))
        }
        Err(e) => {
            warn!(error = %e, "an MCP server's OAuth callback failed");
            match target {
                Some((agent_id, server)) => back(agent_id, &server, Err(e.to_string())),
                None => page(&e.to_string()),
            }
        }
    }
}

/// The browser goes on to the agent's settings, which say how it went.
fn back(agent_id: havn_core::AgentId, server: &str, outcome: Result<(), String>) -> Response {
    let mut url = format!(
        "/agent/settings?id={agent_id}&mcp_server={}",
        url_escape(server)
    );
    match outcome {
        Ok(()) => url.push_str("&mcp_oauth=connected"),
        Err(why) => {
            url.push_str("&mcp_oauth_error=");
            url.push_str(&url_escape(&why.chars().take(300).collect::<String>()));
        }
    }
    Redirect::to(&url).into_response()
}

fn url_escape(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// A short page for a callback that cannot go back to an agent.
fn page(text: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text.to_string(),
    )
        .into_response()
}

/// `DELETE /agents/:id/mcp/:server/oauth`: forget the server's tokens. The
/// grant stays at the provider until it is revoked there.
pub async fn disconnect(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, server)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let was = state
        .secrets
        .oauth()
        .disconnect(agent.id, &server)
        .await
        .map_err(oauth_error)?;
    if !was {
        return Err(ApiError::NotFound);
    }
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.mcp.oauth.disconnected",
        serde_json::json!({ "server": server }),
    )
    .await;
    state.secrets.push(&state.registry, agent.id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Pull the (can_use_mcp, mcp_servers) pair out of agent.config.
/// Both fields default to "off / empty" when missing or malformed —
/// matches the runtime's "default permissions = MCP off" stance.
fn read_mcp_from_config(config: &serde_json::Value) -> (bool, HashMap<String, McpServerConfig>) {
    let can_use_mcp = config
        .get("policy")
        .and_then(|p| p.get("permissions"))
        .and_then(|p| p.get("can_use_mcp"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let servers = config
        .get("policy")
        .and_then(|p| p.get("mcp_servers"))
        .map(|v| {
            serde_json::from_value::<HashMap<String, McpServerConfig>>(v.clone())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    (can_use_mcp, servers)
}

async fn list_installed_binaries() -> Vec<String> {
    let mut out = Vec::new();
    let mut entries = match fs::read_dir(MCP_SERVERS_DIR).await {
        Ok(e) => e,
        Err(e) => {
            // Missing dir is the normal case on a fresh box. Anything
            // else is operator error — log but don't break the GET.
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(error = %e, "could not read MCP_SERVERS_DIR");
            }
            return out;
        }
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(file_type) = entry.file_type().await {
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }
        }
        if let Some(name) = entry.file_name().to_str() {
            // Skip dotfiles + obvious non-binaries. Operators put
            // anything they want here; we lightly filter the common
            // editor noise.
            if name.starts_with('.')
                || std::path::Path::new(name)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            {
                continue;
            }
            out.push(name.to_string());
        }
    }
    out.sort();
    out
}

/// Same rules as `havn_runtime::mcp::validate_binary_name`. Keep in
/// sync — the runtime re-validates at spawn time, but doing it here
/// gives the dashboard a precise error.
fn validate_binary_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("empty binary name");
    }
    if name.contains('/') {
        return Err("binary must be a file name, not a path");
    }
    if name == "." || name == ".." || name.contains("..") {
        return Err("binary name must not contain '..'");
    }
    if name.bytes().any(|b| b.is_ascii_control()) {
        return Err("binary name contains control bytes");
    }
    Ok(())
}
