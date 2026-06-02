//! `/agents/:id/mcp` — read & write the agent's MCP configuration
//! (spec §13 Phase 3).
//!
//! This endpoint pair gives the dashboard a focused window into the
//! agent's `policy.permissions.can_use_mcp` flag + `policy.mcp_servers`
//! map without making the operator hand-edit the SQLite row. Both
//! fields live as JSON inside `agent.config.policy`; the per-session
//! resolver (`policy_resolver::override_for`) reads them at every
//! agent start (spec §6.3 / §9.4 frozen-prompt invariant — edits
//! land on the next restart).
//!
//! Writes never touch unrelated policy keys (allowed_models,
//! resource_limits, …) — we read agent.config, splice in only the
//! `policy.permissions.can_use_mcp` and `policy.mcp_servers` fields,
//! and write back. Owner-only auth via `agents::ensure_owner`.
//!
//! `available_binaries` lists names under `/usr/share/havn/mcp-servers/`
//! at GET time so the dashboard can present a dropdown of binaries
//! the operator already pre-installed (matches the spec §13 bonus
//! decision: "agent self can't acquire new MCP servers — only
//! whitelisted ones already on the host").

use axum::Json;
use axum::extract::{Path, State};
use havn_core::McpServerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::fs;
use tracing::{info, warn};

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;

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
    let (can_use_mcp, servers) = read_mcp_from_config(&agent.config);
    let available_binaries = list_installed_binaries().await;
    Ok(Json(McpView {
        can_use_mcp,
        servers,
        available_binaries,
    }))
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
            validate_binary_name(&cfg.binary)
                .map_err(|e| ApiError::BadRequest(format!("server {name:?}: {e}")))?;
        }
    }

    // Splice only `policy.permissions.can_use_mcp` + `policy.mcp_servers`
    // into agent.config; leave every other policy key untouched. Build
    // the new config object by surgical mutation.
    let mut config = agent.config.as_object().cloned().unwrap_or_default();
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
    let new_config = serde_json::Value::Object(config);

    let updated = havn_db::repo::agents::patch(&state.db, id, None, Some(&new_config)).await?;
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

    let (can_use_mcp, servers) = read_mcp_from_config(&updated.config);
    Ok(Json(McpView {
        can_use_mcp,
        servers,
        available_binaries: list_installed_binaries().await,
    }))
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
