//! Bootstrap files + frozen system prompt assembly (spec §5.3, §9.4).
//!
//! [`BootstrapFiles`] holds the trimmed contents of every bootstrap file
//! the agent owns. Each field is `None` when the file is missing or
//! whitespace-only — per spec §5.3 "Empty files are skipped silently."
//!
//! v0.7 renames the persona file from `SYSTEM.md` back to `SOUL.md`:
//! - `SOUL.md` — persona + identity (tone, values, name, purpose).
//! - `USER.md` — what the agent should always know about its user.
//! - `HEARTBEAT.md` — re-read on every heartbeat tick (§9.6).
//!
//! `MEMORY.md` from earlier drafts is gone: the typed memory table
//! (§9.4) is the source of truth for the agent's accumulated facts;
//! the runtime auto-injects active rows into the system prompt at
//! every LLM call. Maintaining a parallel markdown file was double-
//! bookkeeping.
//!
//! ## Backward compatibility
//!
//! Existing workspaces from v0.6 may still have `SYSTEM.md` on disk.
//! The loader tolerates it: if `SOUL.md` is absent but `SYSTEM.md`
//! exists, the loader uses `SYSTEM.md` as the persona and logs a
//! deprecation warning. Even older workspaces with separate
//! `SOUL.md` / `IDENTITY.md` use the IDENTITY-then-SOUL concat
//! fallback.
//!
//! ## Platform context
//!
//! A runtime-generated preamble describing the havn platform, the
//! agent's identity, and available tools is prepended before the
//! bootstrap file sections. This gives the LLM situational awareness
//! without requiring the user to manually document these facts in
//! SOUL.md.
//!
//! ## Frozen-prompt invariant
//!
//! The assembled prompt is pinned into `Arc<SystemPrompt>` for the
//! runtime process's lifetime — mid-session edits to SOUL.md /
//! USER.md do **not** retroactively change the running session.
//! `HEARTBEAT.md` is the deliberate exception: re-read on every
//! heartbeat tick (spec §9.6), so users can edit and the next tick
//! observes.

use std::path::Path;

use tracing::warn;

const SOUL: &str = "SOUL.md";
const USER: &str = "USER.md";
const HEARTBEAT: &str = "HEARTBEAT.md";

// v0.6 filename — the loader still tolerates for migration.
const LEGACY_SYSTEM: &str = "SYSTEM.md";
// Even older filenames the loader tolerates.
const LEGACY_IDENTITY: &str = "IDENTITY.md";
const LEGACY_MEMORY: &str = "MEMORY.md";

/// Build the platform-context preamble injected before bootstrap files.
///
/// This gives the LLM situational awareness about the havn platform,
/// the agent's identity, and available tools — without requiring the
/// user to duplicate this information in SOUL.md.
pub fn platform_context(agent_name: &str, agent_id: &str, tool_names: &[String]) -> String {
    let mut ctx = format!(
        "You are an autonomous agent running on havn, a self-hosted agent platform.\n\
         \n\
         Agent: {agent_name} ({agent_id})\n\
         \n\
         You have access to these tools:"
    );
    for name in tool_names {
        ctx.push_str(&format!("\n- {name}"));
    }
    ctx.push_str(
        "\n\n\
         Memory system: You can store and recall facts using memory_remember, memory_search, \
         and memory_forget. Facts are categorized by kind (identity, preference, project, event) \
         and source (user_told, agent_inferred). Facts are aged by recall freshness \
         \u{2014} things you use stay; things you forget fade. The user can see and delete \
         your memories via the dashboard.\n\
         \n\
         Skills: When you solve a complex task successfully, use skill_manage to save the \
         procedure as a reusable skill. Skills are retrieved automatically when relevant to \
         future conversations.\n\
         \n\
         When the user edits your configuration (SOUL.md, USER.md) via the dashboard, changes \
         take effect on your next session \u{2014} not mid-conversation.",
    );
    ctx
}

/// Snapshot of every bootstrap file relevant to a session.
#[derive(Debug, Clone, Default)]
pub struct BootstrapFiles {
    /// Persona + identity, loaded from SOUL.md (or SYSTEM.md / legacy
    /// SOUL.md+IDENTITY.md fallback).
    pub system: Option<String>,
    pub user: Option<String>,
    pub heartbeat: Option<String>,
}

impl BootstrapFiles {
    pub async fn load(workspace: &Path) -> std::io::Result<Self> {
        // Primary: SOUL.md
        let canonical = read_optional(workspace, SOUL).await?;
        let system = if canonical.is_some() {
            canonical
        } else {
            // First fallback: SYSTEM.md (v0.6 name)
            let legacy_system = read_optional(workspace, LEGACY_SYSTEM).await?;
            if legacy_system.is_some() {
                warn!(
                    workspace = %workspace.display(),
                    "found SYSTEM.md but no SOUL.md; SYSTEM.md is deprecated \u{2014} rename it \
                     to SOUL.md. The runtime will keep reading it in the meantime."
                );
                legacy_system
            } else {
                // Second fallback: legacy SOUL.md + IDENTITY.md concat
                // (pre-v0.6 workspaces). Order matches the pre-v0.6
                // prompt assembly (IDENTITY -> SOUL).
                let legacy_identity = read_optional(workspace, LEGACY_IDENTITY).await?;
                // Re-read SOUL.md is not needed — we already checked the
                // canonical path above; the legacy fallback here only
                // fires when SOUL.md didn't exist at line 1. But the
                // legacy path was for workspaces that had SOUL.md +
                // IDENTITY.md *before* v0.6 collapsed them. Since we
                // already checked SOUL.md above (it was None), there's
                // nothing to concat.
                if legacy_identity.is_some() {
                    warn!(
                        workspace = %workspace.display(),
                        "found legacy IDENTITY.md; consider merging into SOUL.md and \
                         deleting the original. The runtime will keep reading it in the \
                         meantime."
                    );
                }
                legacy_identity
            }
        };

        // Warn about MEMORY.md once but ignore its content — the typed
        // memory table is authoritative now.
        if read_optional(workspace, LEGACY_MEMORY).await?.is_some() {
            warn!(
                workspace = %workspace.display(),
                "found legacy MEMORY.md \u{2014} its content is no longer auto-loaded. The \
                 typed memory table (\u{00a7}9.4) is the source of truth. Migrate any facts \
                 you want preserved into typed memory rows and delete MEMORY.md."
            );
        }

        Ok(Self {
            system,
            user: read_optional(workspace, USER).await?,
            heartbeat: read_optional(workspace, HEARTBEAT).await?,
        })
    }

    /// Build the system prompt covering every present section.
    ///
    /// When `platform_ctx` is `Some`, it is prepended before the
    /// bootstrap file sections. Order:
    ///   platform context -> SOUL -> USER -> HEARTBEAT
    ///
    /// Typed-memory injections (recent events + relevant skills) are
    /// appended per-call by the runtime in `main::augment_with_skills`;
    /// the base assembled here stays frozen for the session.
    pub fn system_prompt(&self, platform_ctx: Option<&str>) -> SystemPrompt {
        SystemPrompt::from_sections_with_preamble(
            platform_ctx,
            &[
                (SOUL, self.system.as_deref()),
                (USER, self.user.as_deref()),
                (HEARTBEAT, self.heartbeat.as_deref()),
            ],
        )
    }

    /// Build a "cron-mode" system prompt — `skip_memory: true` per
    /// spec §8.5. Cron contexts inherit the agent's persona/identity
    /// (SOUL) but skip USER.md and HEARTBEAT.md (those are for
    /// interactive use; cron is fresh-context).
    pub fn cron_system_prompt(&self, platform_ctx: Option<&str>) -> SystemPrompt {
        SystemPrompt::from_sections_with_preamble(platform_ctx, &[(SOUL, self.system.as_deref())])
    }
}

#[derive(Debug, Clone, Default)]
pub struct SystemPrompt {
    text: Option<String>,
}

impl SystemPrompt {
    fn from_sections_with_preamble(
        preamble: Option<&str>,
        sections: &[(&str, Option<&str>)],
    ) -> Self {
        let mut chunks: Vec<String> = Vec::new();
        if let Some(pre) = preamble {
            if !pre.is_empty() {
                chunks.push(pre.to_string());
            }
        }
        for (name, body) in sections {
            if let Some(body) = body
                && !body.is_empty()
            {
                chunks.push(format!("# {name}\n\n{body}"));
            }
        }
        let text = if chunks.is_empty() {
            None
        } else {
            Some(chunks.join("\n\n---\n\n"))
        };
        Self { text }
    }

    pub fn as_text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Produce a one-off augmented copy with `extra` appended after a
    /// horizontal rule. Used per-turn to splice retrieved skills /
    /// recent events into the LLM call without mutating the frozen
    /// base (spec §9.4 invariant).
    pub fn augmented(&self, extra: &str) -> Self {
        let text = match self.text.as_deref() {
            Some(base) => format!("{base}\n\n---\n\n{extra}"),
            None => extra.to_string(),
        };
        Self { text: Some(text) }
    }
}

async fn read_optional(workspace: &Path, fname: &str) -> std::io::Result<Option<String>> {
    let path = workspace.join(fname);
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let trimmed = content.trim();
            Ok(if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("havn-sp-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    #[tokio::test]
    async fn empty_workspace_yields_all_none() {
        let dir = tempdir();
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert!(bf.system.is_none());
        assert!(bf.user.is_none());
        assert!(bf.heartbeat.is_none());
        assert!(bf.system_prompt(None).as_text().is_none());
    }

    #[tokio::test]
    async fn whitespace_only_files_are_skipped() {
        let dir = tempdir();
        tokio::fs::write(dir.join(SOUL), "   \n\n\t\n")
            .await
            .expect("write");
        tokio::fs::write(dir.join(USER), "real")
            .await
            .expect("write");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert!(bf.system.is_none());
        assert_eq!(bf.user.as_deref(), Some("real"));
    }

    #[tokio::test]
    async fn order_is_soul_user_heartbeat() {
        let dir = tempdir();
        tokio::fs::write(dir.join(SOUL), "soul-content")
            .await
            .expect("write");
        tokio::fs::write(dir.join(USER), "user-content")
            .await
            .expect("write");
        tokio::fs::write(dir.join(HEARTBEAT), "heartbeat-content")
            .await
            .expect("write");

        let bf = BootstrapFiles::load(&dir).await.expect("load");
        let prompt = bf.system_prompt(None);
        let text = prompt.as_text().expect("some");
        let soul_pos = text.find("soul-content").unwrap();
        let user_pos = text.find("user-content").unwrap();
        let heartbeat_pos = text.find("heartbeat-content").unwrap();
        assert!(soul_pos < user_pos);
        assert!(user_pos < heartbeat_pos);
    }

    #[tokio::test]
    async fn cron_prompt_keeps_only_soul() {
        let dir = tempdir();
        tokio::fs::write(dir.join(SOUL), "you are alice; concise")
            .await
            .expect("write");
        tokio::fs::write(dir.join(USER), "user is bob")
            .await
            .expect("write");
        tokio::fs::write(dir.join(HEARTBEAT), "check email")
            .await
            .expect("write");

        let bf = BootstrapFiles::load(&dir).await.expect("load");
        let cron = bf.cron_system_prompt(None);
        let text = cron.as_text().expect("some");
        assert!(text.contains("you are alice"));
        assert!(!text.contains("user is bob"), "cron should skip USER.md");
        assert!(
            !text.contains("check email"),
            "cron should skip HEARTBEAT.md"
        );
    }

    // ---- legacy compat -------------------------------------------------

    #[tokio::test]
    async fn legacy_system_md_loads_as_soul() {
        let dir = tempdir();
        tokio::fs::write(dir.join(LEGACY_SYSTEM), "persona from v0.6")
            .await
            .expect("write");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert_eq!(bf.system.as_deref(), Some("persona from v0.6"));
    }

    #[tokio::test]
    async fn legacy_identity_only_loads_as_system() {
        let dir = tempdir();
        tokio::fs::write(dir.join(LEGACY_IDENTITY), "you are alice")
            .await
            .expect("write");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert_eq!(bf.system.as_deref(), Some("you are alice"));
    }

    #[tokio::test]
    async fn canonical_soul_wins_over_legacy() {
        // If both new and old files exist, SOUL.md is preferred —
        // the operator presumably already migrated and forgot to
        // delete the originals.
        let dir = tempdir();
        tokio::fs::write(dir.join(SOUL), "canonical")
            .await
            .expect("w");
        tokio::fs::write(dir.join(LEGACY_SYSTEM), "legacy system")
            .await
            .expect("w");
        tokio::fs::write(dir.join(LEGACY_IDENTITY), "legacy identity")
            .await
            .expect("w");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert_eq!(bf.system.as_deref(), Some("canonical"));
    }

    #[tokio::test]
    async fn soul_md_wins_over_system_md() {
        let dir = tempdir();
        tokio::fs::write(dir.join(SOUL), "new soul")
            .await
            .expect("w");
        tokio::fs::write(dir.join(LEGACY_SYSTEM), "old system")
            .await
            .expect("w");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert_eq!(bf.system.as_deref(), Some("new soul"));
    }

    // ---- platform context ----------------------------------------------

    #[test]
    fn platform_context_produces_expected_output() {
        let ctx = platform_context(
            "dev assistant",
            "agt-123",
            &[
                "memory_remember".into(),
                "memory_search".into(),
                "shell".into(),
            ],
        );
        assert!(ctx.contains("havn, a self-hosted agent platform"));
        assert!(ctx.contains("Agent: dev assistant (agt-123)"));
        assert!(ctx.contains("- memory_remember"));
        assert!(ctx.contains("- memory_search"));
        assert!(ctx.contains("- shell"));
        assert!(ctx.contains("memory_remember, memory_search, and memory_forget"));
        assert!(ctx.contains("skill_manage"));
        assert!(ctx.contains("SOUL.md"));
    }

    #[test]
    fn assembly_order_is_platform_then_soul_then_user_then_heartbeat() {
        let preamble = platform_context("test", "id-1", &["shell".into()]);
        let prompt = SystemPrompt::from_sections_with_preamble(
            Some(&preamble),
            &[
                (SOUL, Some("soul-body")),
                (USER, Some("user-body")),
                (HEARTBEAT, Some("hb-body")),
            ],
        );
        let text = prompt.as_text().expect("some");
        let plat_pos = text.find("havn, a self-hosted").unwrap();
        let soul_pos = text.find("soul-body").unwrap();
        let user_pos = text.find("user-body").unwrap();
        let hb_pos = text.find("hb-body").unwrap();
        assert!(plat_pos < soul_pos, "platform context must precede SOUL");
        assert!(soul_pos < user_pos, "SOUL must precede USER");
        assert!(user_pos < hb_pos, "USER must precede HEARTBEAT");
    }

    #[test]
    fn platform_context_with_no_tools() {
        let ctx = platform_context("agent", "id-1", &[]);
        assert!(ctx.contains("You have access to these tools:"));
        // No tool bullet points, but the header is still there.
        assert!(!ctx.contains("\n- "));
    }
}
