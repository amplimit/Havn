//! `/agents/:id/bootstrap` — read & write the per-agent bootstrap
//! markdown files (spec §5.3, §9.4 layer 2).
//!
//! These are the three plain-text files that drive the agent's
//! persona + ambient state:
//!
//! | File          | What it holds                                    | When edits take effect             |
//! | ------------- | ------------------------------------------------ | ---------------------------------- |
//! | `SYSTEM.md`   | Persona + identity (tone, values, name, purpose) | Next agent restart (frozen prompt) |
//! | `USER.md`     | Durable facts about the user the agent should always have in scope | Next agent restart (frozen prompt) |
//! | `HEARTBEAT.md`| Plain-language instructions for the periodic self-tick (§9.6) | Next heartbeat tick (re-read fresh — the deliberate exception to the frozen-prompt invariant) |
//!
//! The files live at `<workspace>/<NAME>.md`. Pre-Phase 3 the only
//! way to edit them was ssh + vim; this module exposes a small
//! HTTP surface so the dashboard can offer a textarea per file.
//!
//! Auth is owner-only via `agents::ensure_owner` (same as the
//! settings PATCH path). Writes log an audit entry. Empty / missing
//! files return `null` so the dashboard can render a placeholder
//! rather than a misleading empty-string.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use std::path::Path as StdPath;
use tokio::fs;
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;

const SYSTEM_MD: &str = "SYSTEM.md";
const USER_MD: &str = "USER.md";
const HEARTBEAT_MD: &str = "HEARTBEAT.md";

/// Hard cap matching the runtime side's per-skill body cap (§9.3) —
/// bootstrap files don't need to be bigger; system prompts that
/// large overwhelm the model anyway. Catches a copy-paste accident
/// before the gateway writes it.
const MAX_BOOTSTRAP_BYTES: usize = 100 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BootstrapView {
    /// `None` when the file doesn't exist on disk OR is empty after
    /// trim — the runtime treats both the same (skips the section in
    /// the assembled prompt). The dashboard renders an empty
    /// textarea for both cases.
    pub system: Option<String>,
    pub user: Option<String>,
    pub heartbeat: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct BootstrapPatch {
    /// Each field is optional: omit to leave the file untouched.
    /// `Some("")` is treated as "delete the file" — empty bootstrap
    /// matches "no SYSTEM.md exists" semantics on the runtime side.
    pub system: Option<String>,
    pub user: Option<String>,
    pub heartbeat: Option<String>,
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<BootstrapView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let id = agent.id;
    let workspace = crate::api::agents::workspace_for(&state, id);
    Ok(Json(BootstrapView {
        system: read_optional(&workspace, SYSTEM_MD).await?,
        user: read_optional(&workspace, USER_MD).await?,
        heartbeat: read_optional(&workspace, HEARTBEAT_MD).await?,
    }))
}

pub async fn put(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<BootstrapPatch>,
) -> Result<Json<BootstrapView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let id = agent.id;
    let workspace = crate::api::agents::workspace_for(&state, id);
    if !workspace.exists() {
        // The spawner creates the workspace at agent CREATE time,
        // so a missing dir means something is structurally wrong;
        // still, fall back to creating it so a hand-deleted dir
        // doesn't permanently brick the dashboard.
        fs::create_dir_all(&workspace)
            .await
            .map_err(|e| ApiError::Internal(format!("creating workspace: {e}")))?;
    }
    let mut touched: Vec<&'static str> = Vec::new();
    if let Some(s) = &req.system {
        write_or_clear(&workspace, SYSTEM_MD, s).await?;
        touched.push(SYSTEM_MD);
    }
    if let Some(s) = &req.user {
        write_or_clear(&workspace, USER_MD, s).await?;
        touched.push(USER_MD);
    }
    if let Some(s) = &req.heartbeat {
        write_or_clear(&workspace, HEARTBEAT_MD, s).await?;
        touched.push(HEARTBEAT_MD);
    }
    if !touched.is_empty() {
        audit::record_user_action(
            &state.db,
            user.id,
            "agent.bootstrap.updated",
            serde_json::json!({"agent_id": id.to_string(), "files": touched}),
        )
        .await;
        info!(agent_id = %id, files = ?touched, "bootstrap files updated");
    }
    // Re-read so the dashboard sees what's actually on disk now —
    // `Some("")` PATCHes round-trip to `None` (file removed), which
    // matches the runtime's "empty = no section" semantics.
    Ok(Json(BootstrapView {
        system: read_optional(&workspace, SYSTEM_MD).await?,
        user: read_optional(&workspace, USER_MD).await?,
        heartbeat: read_optional(&workspace, HEARTBEAT_MD).await?,
    }))
}

async fn read_optional(workspace: &StdPath, name: &str) -> Result<Option<String>, ApiError> {
    let path = workspace.join(name);
    match fs::read_to_string(&path).await {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(s))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ApiError::Internal(format!("reading {name}: {e}"))),
    }
}

async fn write_or_clear(workspace: &StdPath, name: &str, content: &str) -> Result<(), ApiError> {
    if content.len() > MAX_BOOTSTRAP_BYTES {
        return Err(ApiError::BadRequest(format!(
            "{name} body exceeds {MAX_BOOTSTRAP_BYTES} byte cap (got {})",
            content.len()
        )));
    }
    let path = workspace.join(name);
    if content.trim().is_empty() {
        // Empty body → remove file. Runtime treats absent + empty
        // identically; persisting an empty file would leave behind
        // something the agent's I/O has to skip on every session.
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ApiError::Internal(format!("removing {name}: {e}"))),
        }
    } else {
        fs::write(&path, content)
            .await
            .map_err(|e| ApiError::Internal(format!("writing {name}: {e}")))
    }
}
