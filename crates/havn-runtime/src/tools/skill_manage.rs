//! `skill_manage` tool — agent-driven skill creation / edit / removal
//! (spec §9.3, Phase 2).
//!
//! From the user's seat: when an agent figures out a non-trivial way to
//! do something — an effective git workflow, a code-review rubric the
//! user keeps reinforcing — it can package it as a skill so the *next*
//! time you ask, the workflow is already in the prompt instead of the
//! agent reinventing it. Skills are markdown files in
//! `workspace/skills/<name>/SKILL.md`; they round-trip through the
//! existing parser ([`crate::skills`]) so anything the agent creates is
//! also editable by hand.
//!
//! Gated by `policy.permissions.can_install_skills` at registry build
//! time, mirroring `can_use_shell` / `can_access_network`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::fs;
use tracing::{info, warn};
use uuid::Uuid;

use crate::skills::{self, MAX_BODY_BYTES, SkillSource, parse_skill};
use havn_db::agent::skills_index;

use super::{Tool, ToolCtx, ToolResult};

pub struct SkillManageTool;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    /// Create a brand-new skill. Fails if `<name>` already exists.
    Create {
        name: String,
        description: String,
        /// Markdown body, ≤ 100 KB (spec §9.3).
        body: String,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        triggers: Vec<String>,
    },
    /// Replace an existing skill's content. Fails if `<name>` doesn't exist.
    Update {
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        triggers: Option<Vec<String>>,
    },
    /// Remove a skill. Idempotent on missing names.
    Delete { name: String },
    /// List all workspace-created skills (name + description).
    List,
    /// Mark a skill as pinned. Pinned skills are immune to the
    /// curator (spec §9.5) — use when a skill is load-bearing and you
    /// don't want auto-archive or LLM consolidation to touch it.
    Pin { name: String },
    /// Reverse `pin`; the skill becomes curator-eligible again.
    Unpin { name: String },
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &'static str {
        "skill_manage"
    }

    fn description(&self) -> &'static str {
        "Create / update / delete / list workspace skills. Use when you've \
         worked out a non-trivial procedure that's likely to come up again \
         (a code-review checklist the user keeps reinforcing, a git workflow \
         that succeeded, a multi-step debugging method). Skills are markdown \
         files under workspace/skills/; the user can also edit them by hand. \
         Confirm with the user before creating or deleting unless the user \
         clearly asked you to."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "delete", "list", "pin", "unpin"],
                    "description": "Which action to perform. pin/unpin protect a skill from the curator (spec §9.5)."
                },
                "name": {
                    "type": "string",
                    "description": "kebab-case skill name (e.g. 'code-review'). Required for create/update/delete."
                },
                "description": {
                    "type": "string",
                    "description": "One-liner shown in the index. Required for create."
                },
                "body": {
                    "type": "string",
                    "description": "Markdown body, ≤ 100 KB. Required for create."
                },
                "version": { "type": "string" },
                "triggers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional retrieval hints (keywords)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let parsed: Action = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("invalid skill_manage input: {e}")),
        };
        match parsed {
            Action::Create {
                name,
                description,
                body,
                version,
                triggers,
            } => {
                create(
                    ctx,
                    &name,
                    &description,
                    &body,
                    version.as_deref(),
                    &triggers,
                )
                .await
            }
            Action::Update {
                name,
                description,
                body,
                version,
                triggers,
            } => {
                update(
                    ctx,
                    &name,
                    description.as_deref(),
                    body.as_deref(),
                    version.as_deref(),
                    triggers.as_deref(),
                )
                .await
            }
            Action::Delete { name } => delete(ctx, &name).await,
            Action::List => list(ctx).await,
            Action::Pin { name } => set_pinned(ctx, &name, true).await,
            Action::Unpin { name } => set_pinned(ctx, &name, false).await,
        }
    }
}

async fn set_pinned(ctx: &ToolCtx, name: &str, pinned: bool) -> ToolResult {
    if let Err(e) = validate_name(name) {
        return ToolResult::Error(e);
    }
    match skills_index::set_pinned(&ctx.agent_db, name, pinned).await {
        Ok(true) => ToolResult::Ok(format!(
            "{} skill {name:?}",
            if pinned { "pinned" } else { "unpinned" }
        )),
        Ok(false) => ToolResult::Error(format!("skill {name:?} not found in index")),
        Err(e) => ToolResult::Error(format!("set_pinned failed: {e}")),
    }
}

/// Reject any name that isn't safe to use as a directory component.
/// kebab-case lowercase + digits + hyphens + underscores only — same
/// shape the bundled skills use, also keeps the agent from constructing
/// path-traversal payloads via prompt injection.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must be non-empty".into());
    }
    if name.len() > 64 {
        return Err("name must be ≤ 64 characters".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err("name must be kebab-case: lowercase letters, digits, '-' or '_' only".into());
    }
    Ok(())
}

fn skill_dir(workspace: &Path, name: &str) -> PathBuf {
    workspace.join("skills").join(name)
}

fn skill_path(workspace: &Path, name: &str) -> PathBuf {
    skill_dir(workspace, name).join("SKILL.md")
}

fn render_skill_md(
    name: &str,
    description: &str,
    body: &str,
    version: Option<&str>,
    triggers: &[String],
) -> String {
    let mut out = String::with_capacity(256 + body.len());
    out.push_str("---\n");
    out.push_str(&format!("name: {name}\n"));
    out.push_str(&format!("description: {description}\n"));
    if let Some(v) = version {
        out.push_str(&format!("version: {v}\n"));
    }
    if !triggers.is_empty() {
        out.push_str("triggers: [");
        for (i, t) in triggers.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(t);
        }
        out.push_str("]\n");
    }
    out.push_str("metadata:\n");
    out.push_str("  havn:\n");
    out.push_str("    source: agent_created\n");
    out.push_str("---\n\n");
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

async fn create(
    ctx: &ToolCtx,
    name: &str,
    description: &str,
    body: &str,
    version: Option<&str>,
    triggers: &[String],
) -> ToolResult {
    if let Err(e) = validate_name(name) {
        return ToolResult::Error(e);
    }
    if description.trim().is_empty() {
        return ToolResult::Error("description must be non-empty".into());
    }
    if body.trim().is_empty() {
        return ToolResult::Error("body must be non-empty".into());
    }
    if body.len() > MAX_BODY_BYTES {
        return ToolResult::Error(format!(
            "body exceeds {MAX_BODY_BYTES} byte cap (got {})",
            body.len()
        ));
    }

    let dir = skill_dir(&ctx.workspace_dir, name);
    let path = skill_path(&ctx.workspace_dir, name);
    if fs::try_exists(&path).await.unwrap_or(false) {
        return ToolResult::Error(format!(
            "skill {name:?} already exists; use update to modify"
        ));
    }
    if let Err(e) = fs::create_dir_all(&dir).await {
        return ToolResult::Error(format!("creating skill dir: {e}"));
    }
    let content = render_skill_md(name, description, body, version, triggers);
    if let Err(e) = fs::write(&path, &content).await {
        return ToolResult::Error(format!("writing SKILL.md: {e}"));
    }
    if let Err(e) = reindex_one(&ctx.agent_db, &content).await {
        warn!(name, error = %e, "skill written but FTS index update failed");
    }
    embed_and_persist(ctx, name, description).await;
    info!(name, "skill created");
    ToolResult::Ok(format!(
        "created skill {name:?}. It will be retrievable on the next user turn."
    ))
}

async fn update(
    ctx: &ToolCtx,
    name: &str,
    description: Option<&str>,
    body: Option<&str>,
    version: Option<&str>,
    triggers: Option<&[String]>,
) -> ToolResult {
    if let Err(e) = validate_name(name) {
        return ToolResult::Error(e);
    }
    let path = skill_path(&ctx.workspace_dir, name);
    if !fs::try_exists(&path).await.unwrap_or(false) {
        return ToolResult::Error(format!("skill {name:?} does not exist"));
    }
    let existing = match fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => return ToolResult::Error(format!("reading existing SKILL.md: {e}")),
    };
    let parsed = match parse_skill(&existing, SkillSource::Workspace) {
        Ok(p) => p,
        Err(e) => return ToolResult::Error(format!("existing SKILL.md is malformed: {e}")),
    };
    let new_description = description.unwrap_or(&parsed.description);
    let new_body = body.unwrap_or(&parsed.body);
    let new_version = version.map(str::to_string).or(parsed.version);
    let new_triggers: Vec<String> = triggers.map(<[String]>::to_vec).unwrap_or(parsed.triggers);
    if new_body.len() > MAX_BODY_BYTES {
        return ToolResult::Error(format!(
            "body exceeds {MAX_BODY_BYTES} byte cap (got {})",
            new_body.len()
        ));
    }
    let content = render_skill_md(
        name,
        new_description,
        new_body,
        new_version.as_deref(),
        &new_triggers,
    );
    if let Err(e) = fs::write(&path, &content).await {
        return ToolResult::Error(format!("writing SKILL.md: {e}"));
    }
    if let Err(e) = reindex_one(&ctx.agent_db, &content).await {
        warn!(name, error = %e, "skill updated but FTS index update failed");
    }
    embed_and_persist(ctx, name, new_description).await;
    info!(name, "skill updated");
    ToolResult::Ok(format!("updated skill {name:?}"))
}

/// Compute and persist an embedding for the skill's name+description.
/// **Why name+description, not the body**: spec §13 Phase 3 design
/// note — `description` is what the author wrote as a one-line
/// summary, exactly the signal the retriever wants. Bodies are often
/// 100 KB of templates and reference links that dilute semantic
/// matching. Mirrors memory's `"<key>: <value>"` decision (signal-
/// dense, no boilerplate).
///
/// Failure is non-fatal: the row is already persisted, so worst case
/// retrieval falls back to FTS5 only. The startup backfill
/// (`embedding::backfill::skills_run_to_completion`) heals any rows
/// that miss the live embed path.
async fn embed_and_persist(ctx: &ToolCtx, name: &str, description: &str) {
    let Some(emb) = ctx.embedder.as_ref() else {
        return;
    };
    let text = format!("{name}: {description}");
    match emb.embed(&text).await {
        Ok(vec) => {
            if let Err(e) =
                havn_db::agent::skills_index::set_embedding(&ctx.agent_db, name, &vec).await
            {
                warn!(name, error = %e, "skills set_embedding failed; row stored without vector");
            }
        }
        Err(e) => {
            warn!(name, error = %e, "skills embedder failed; row stored without vector");
        }
    }
}

async fn delete(ctx: &ToolCtx, name: &str) -> ToolResult {
    if let Err(e) = validate_name(name) {
        return ToolResult::Error(e);
    }
    let dir = skill_dir(&ctx.workspace_dir, name);
    if !fs::try_exists(&dir).await.unwrap_or(false) {
        return ToolResult::Ok(format!("skill {name:?} did not exist (no-op)"));
    }
    if let Err(e) = fs::remove_dir_all(&dir).await {
        return ToolResult::Error(format!("removing skill dir: {e}"));
    }
    if let Err(e) = sqlx::query("DELETE FROM skills_index WHERE name = ?1")
        .bind(name)
        .execute(&ctx.agent_db)
        .await
    {
        warn!(name, error = %e, "skill files removed but FTS index entry remains");
    }
    info!(name, "skill deleted");
    ToolResult::Ok(format!("deleted skill {name:?}"))
}

async fn list(ctx: &ToolCtx) -> ToolResult {
    let workspace_skills = match skills::load_workspace(&ctx.workspace_dir).await {
        Ok(s) => s,
        Err(e) => return ToolResult::Error(format!("listing workspace skills: {e}")),
    };
    if workspace_skills.is_empty() {
        return ToolResult::Ok("(no workspace skills installed)".into());
    }
    use std::fmt::Write as _;
    let mut out = String::from("# Workspace skills\n\n");
    for s in workspace_skills {
        let _ = writeln!(out, "- `{}`: {}", s.name, s.description);
    }
    ToolResult::Ok(out)
}

/// Reindex one freshly-written SKILL.md into the FTS5 index. Avoids
/// re-loading the whole workspace on every edit.
async fn reindex_one(
    pool: &SqlitePool,
    content: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let parsed = parse_skill(content, SkillSource::Workspace)?;
    sqlx::query(
        "INSERT INTO skills_index (id, name, description, body) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(name) DO UPDATE SET \
           description = excluded.description, \
           body        = excluded.body",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&parsed.name)
    .bind(&parsed.description)
    .bind(&parsed.body)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::Policy;
    use havn_db::agent::connect_in_memory;
    use std::sync::Arc;

    async fn ctx() -> ToolCtx {
        let pool = connect_in_memory().await.expect("agent db");
        let workspace = std::env::temp_dir().join(format!("havn-skill-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).await.expect("ws");
        ToolCtx {
            workspace_dir: workspace,
            agent_db: pool,
            http: reqwest::Client::new(),
            policy: Arc::new(Policy::default()),
            embedder: None,
        }
    }

    #[test]
    fn validate_name_rejects_traversal() {
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("with space").is_err());
        assert!(validate_name("UPPER").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("good-name").is_ok());
        assert!(validate_name("good_name_2").is_ok());
    }

    #[tokio::test]
    async fn create_then_list() {
        let ctx = ctx().await;
        let tool = SkillManageTool;
        let r = tool
            .execute(
                &ctx,
                json!({
                    "action": "create",
                    "name": "my-flow",
                    "description": "a way of doing things",
                    "body": "# Title\n\nbody contents"
                }),
            )
            .await;
        assert!(!r.is_error(), "{}", r.text());

        let r = tool.execute(&ctx, json!({"action": "list"})).await;
        assert!(!r.is_error());
        assert!(r.text().contains("my-flow"), "{}", r.text());
    }

    #[tokio::test]
    async fn create_rejects_duplicate() {
        let ctx = ctx().await;
        let tool = SkillManageTool;
        let payload = json!({
            "action": "create",
            "name": "dup",
            "description": "d",
            "body": "b"
        });
        let _r = tool.execute(&ctx, payload.clone()).await;
        let r = tool.execute(&ctx, payload).await;
        assert!(r.is_error());
        assert!(r.text().contains("already exists"));
    }

    #[tokio::test]
    async fn update_preserves_unchanged_fields() {
        let ctx = ctx().await;
        let tool = SkillManageTool;
        tool.execute(
            &ctx,
            json!({
                "action": "create",
                "name": "u",
                "description": "orig desc",
                "body": "orig body",
                "triggers": ["a", "b"]
            }),
        )
        .await;
        let r = tool
            .execute(
                &ctx,
                json!({"action": "update", "name": "u", "body": "new body"}),
            )
            .await;
        assert!(!r.is_error(), "{}", r.text());
        let path = ctx.workspace_dir.join("skills/u/SKILL.md");
        let after = fs::read_to_string(path).await.expect("read");
        assert!(after.contains("orig desc"), "description preserved");
        assert!(after.contains("new body"), "body updated");
        assert!(after.contains("triggers: [a, b]"), "triggers preserved");
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let ctx = ctx().await;
        let tool = SkillManageTool;
        let r = tool
            .execute(&ctx, json!({"action": "delete", "name": "never-existed"}))
            .await;
        assert!(!r.is_error(), "{}", r.text());
        assert!(r.text().contains("no-op"));
    }

    #[tokio::test]
    async fn pin_then_unpin_round_trip() {
        let ctx = ctx().await;
        let tool = SkillManageTool;
        // Have to create the skill first so it's indexed.
        tool.execute(
            &ctx,
            json!({
                "action": "create",
                "name": "load-bearing",
                "description": "d",
                "body": "b"
            }),
        )
        .await;

        let r = tool
            .execute(&ctx, json!({"action": "pin", "name": "load-bearing"}))
            .await;
        assert!(!r.is_error(), "pin: {}", r.text());
        assert!(r.text().contains("pinned"));

        let r = tool
            .execute(&ctx, json!({"action": "unpin", "name": "load-bearing"}))
            .await;
        assert!(!r.is_error(), "unpin: {}", r.text());
        assert!(r.text().contains("unpinned"));
    }

    #[tokio::test]
    async fn pin_unknown_skill_errors() {
        let ctx = ctx().await;
        let tool = SkillManageTool;
        let r = tool
            .execute(&ctx, json!({"action": "pin", "name": "nope"}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("not found"));
    }

    #[tokio::test]
    async fn body_size_cap_enforced() {
        let ctx = ctx().await;
        let tool = SkillManageTool;
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let r = tool
            .execute(
                &ctx,
                json!({"action": "create", "name": "big", "description": "d", "body": big}),
            )
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("byte cap"));
    }
}
