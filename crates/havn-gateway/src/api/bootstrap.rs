//! `/agents/:id/bootstrap` — read & write the per-agent bootstrap
//! markdown files (spec §5.3, §9.4 layer 2).
//!
//! These are the three plain-text files that drive the agent's
//! persona + ambient state:
//!
//! | File          | What it holds                                    | When edits take effect             |
//! | ------------- | ------------------------------------------------ | ---------------------------------- |
//! | `SOUL.md`     | Persona + identity (tone, values, name, purpose) | Next agent restart (frozen prompt) |
//! | `USER.md`     | Durable facts about the user the agent should always have in scope | Next agent restart (frozen prompt) |
//! | `HEARTBEAT.md`| Plain-language instructions for the periodic self-tick (§9.6) | Next heartbeat tick (re-read fresh — the deliberate exception to the frozen-prompt invariant) |
//!
//! The files live at `<workspace>/<NAME>.md`. Pre-Phase 3 the only
//! way to edit them was ssh + vim; this module exposes a small
//! HTTP surface so the dashboard can offer a textarea per file.
//!
//! Auth is owner-only via `agents::resolve_agent` (same as the
//! settings PATCH path). Writes log an audit entry. Empty / missing
//! files return `null` so the dashboard can render a placeholder
//! rather than a misleading empty-string.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use std::path::Path as StdPath;
use tokio::fs;
use tracing::info;

use crate::agent_files::WorkspaceDir;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;

const SOUL_MD: &str = "SOUL.md";
const USER_MD: &str = "USER.md";
const HEARTBEAT_MD: &str = "HEARTBEAT.md";
/// v0.6 filename — read as fallback when SOUL.md is absent.
const LEGACY_SYSTEM_MD: &str = "SYSTEM.md";

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
    ///
    /// Backed by `SOUL.md` on disk (v0.7); API field stays `system`
    /// for backward compat with existing dashboard code.
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
    if !workspace.exists() {
        return Ok(Json(BootstrapView::default()));
    }
    let view = blocking(move || read_view(&open_workspace(&workspace)?)).await?;
    Ok(Json(view))
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
    for (name, body) in [
        (SOUL_MD, &req.system),
        (USER_MD, &req.user),
        (HEARTBEAT_MD, &req.heartbeat),
    ] {
        if body.is_some() {
            touched.push(name);
        }
    }
    let ws_path = workspace.clone();
    let view = blocking(move || {
        let ws = open_workspace(&ws_path)?;
        for (name, body) in [
            (SOUL_MD, &req.system),
            (USER_MD, &req.user),
            (HEARTBEAT_MD, &req.heartbeat),
        ] {
            if let Some(body) = body {
                write_or_clear(&ws, name, body)?;
            }
        }
        // Re-read so the dashboard sees what's actually on disk now —
        // `Some("")` PATCHes round-trip to `None` (file removed), which
        // matches the runtime's "empty = no section" semantics.
        read_view(&ws)
    })
    .await?;
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
    Ok(Json(view))
}

/// Read the persona file: try SOUL.md first, fall back to SYSTEM.md for
/// v0.6 backward compat.
fn read_soul_with_fallback(workspace: &WorkspaceDir) -> Result<Option<String>, ApiError> {
    let soul = read_optional(workspace, SOUL_MD)?;
    if soul.is_some() {
        return Ok(soul);
    }
    read_optional(workspace, LEGACY_SYSTEM_MD)
}

fn read_optional(workspace: &WorkspaceDir, name: &str) -> Result<Option<String>, ApiError> {
    let text = workspace
        .read_text(name, MAX_BOOTSTRAP_BYTES as u64)
        .map_err(|e| ApiError::Internal(format!("reading {name}: {e}")))?;
    Ok(text.filter(|s| !s.trim().is_empty()))
}

fn write_or_clear(workspace: &WorkspaceDir, name: &str, content: &str) -> Result<(), ApiError> {
    if content.len() > MAX_BOOTSTRAP_BYTES {
        return Err(ApiError::BadRequest(format!(
            "{name} body exceeds {MAX_BOOTSTRAP_BYTES} byte cap (got {})",
            content.len()
        )));
    }
    if content.trim().is_empty() {
        // Empty body → remove file. Runtime treats absent + empty
        // identically; persisting an empty file would leave behind
        // something the agent's I/O has to skip on every session.
        workspace
            .remove(name)
            .map_err(|e| ApiError::Internal(format!("removing {name}: {e}")))
    } else {
        workspace
            .write_text(name, content)
            .map_err(|e| ApiError::Internal(format!("writing {name}: {e}")))
    }
}

fn read_view(workspace: &WorkspaceDir) -> Result<BootstrapView, ApiError> {
    Ok(BootstrapView {
        system: read_soul_with_fallback(workspace)?,
        user: read_optional(workspace, USER_MD)?,
        heartbeat: read_optional(workspace, HEARTBEAT_MD)?,
    })
}

/// Open the agent's workspace without following agent-controlled paths.
fn open_workspace(path: &StdPath) -> Result<WorkspaceDir, ApiError> {
    WorkspaceDir::open(path).map_err(|e| ApiError::Internal(format!("opening workspace: {e}")))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ApiError::Internal(format!("workspace task: {e}")))?
}
