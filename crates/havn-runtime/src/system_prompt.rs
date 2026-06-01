//! Bootstrap files + frozen system prompt assembly (spec §5.3, §9.4).
//!
//! [`BootstrapFiles`] holds the trimmed contents of every bootstrap file
//! the agent owns. Each field is `None` when the file is missing or
//! whitespace-only — per spec §5.3 "Empty files are skipped silently."
//!
//! v0.6 reduces the bootstrap set from four files to three:
//! - `SYSTEM.md` — persona + identity (was `SOUL.md` + `IDENTITY.md`,
//!   merged because the distinction was confusing and the two were
//!   always edited together).
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
//! Existing workspaces from earlier drafts may still have
//! `SOUL.md` / `IDENTITY.md` / `MEMORY.md` on disk. The loader
//! tolerates them: if `SYSTEM.md` is absent but `SOUL.md` and/or
//! `IDENTITY.md` exist, the loader concatenates IDENTITY then SOUL
//! into the SYSTEM slot (preserving the old order). `MEMORY.md` is
//! ignored — the curator and dialectic infer have already migrated
//! the user's content into the typed memory table. We log a warning
//! at startup pointing the operator at the migration so they can
//! merge the file by hand and delete it.
//!
//! ## Frozen-prompt invariant
//!
//! The assembled prompt is pinned into `Arc<SystemPrompt>` for the
//! runtime process's lifetime — mid-session edits to SYSTEM.md /
//! USER.md do **not** retroactively change the running session.
//! `HEARTBEAT.md` is the deliberate exception: re-read on every
//! heartbeat tick (spec §9.6), so users can edit and the next tick
//! observes.

use std::path::Path;

use tracing::warn;

const SYSTEM: &str = "SYSTEM.md";
const USER: &str = "USER.md";
const HEARTBEAT: &str = "HEARTBEAT.md";

// Legacy filenames the loader still tolerates for migration. Read
// once, consolidated into `system`. Operator gets a startup warning.
const LEGACY_SOUL: &str = "SOUL.md";
const LEGACY_IDENTITY: &str = "IDENTITY.md";
const LEGACY_MEMORY: &str = "MEMORY.md";

/// Snapshot of every bootstrap file relevant to a session.
#[derive(Debug, Clone, Default)]
pub struct BootstrapFiles {
    /// Persona + identity, merged into SYSTEM.md in v0.6.
    pub system: Option<String>,
    pub user: Option<String>,
    pub heartbeat: Option<String>,
}

impl BootstrapFiles {
    pub async fn load(workspace: &Path) -> std::io::Result<Self> {
        let canonical = read_optional(workspace, SYSTEM).await?;
        let system = if canonical.is_some() {
            canonical
        } else {
            // Legacy fallback: build SYSTEM-shaped content from any
            // SOUL.md / IDENTITY.md still on disk. Order matches the
            // pre-v0.6 prompt assembly (IDENTITY → SOUL).
            let legacy_identity = read_optional(workspace, LEGACY_IDENTITY).await?;
            let legacy_soul = read_optional(workspace, LEGACY_SOUL).await?;
            if legacy_identity.is_some() || legacy_soul.is_some() {
                warn!(
                    workspace = %workspace.display(),
                    "found legacy SOUL.md / IDENTITY.md; consider merging into SYSTEM.md \
                     (spec §5.3 v0.6) and deleting the originals. The runtime will keep \
                     reading them in the meantime."
                );
            }
            match (legacy_identity, legacy_soul) {
                (None, None) => None,
                (Some(i), None) => Some(i),
                (None, Some(s)) => Some(s),
                (Some(i), Some(s)) => Some(format!("{i}\n\n{s}")),
            }
        };

        // Warn about MEMORY.md once but ignore its content — the typed
        // memory table is authoritative now.
        if read_optional(workspace, LEGACY_MEMORY).await?.is_some() {
            warn!(
                workspace = %workspace.display(),
                "found legacy MEMORY.md — its content is no longer auto-loaded. The \
                 typed memory table (§9.4) is the source of truth. Migrate any facts \
                 you want preserved into typed memory rows and delete MEMORY.md."
            );
        }

        Ok(Self {
            system,
            user: read_optional(workspace, USER).await?,
            heartbeat: read_optional(workspace, HEARTBEAT).await?,
        })
    }

    /// Build the system prompt covering every present section. Order:
    /// SYSTEM → USER → HEARTBEAT. Typed-memory injections (recent
    /// events + relevant skills) are appended per-call by the runtime
    /// in `main::augment_with_skills`; the base assembled here stays
    /// frozen for the session.
    pub fn system_prompt(&self) -> SystemPrompt {
        SystemPrompt::from_sections(&[
            (SYSTEM, self.system.as_deref()),
            (USER, self.user.as_deref()),
            (HEARTBEAT, self.heartbeat.as_deref()),
        ])
    }

    /// Build a "cron-mode" system prompt — `skip_memory: true` per
    /// spec §8.5. Cron contexts inherit the agent's persona/identity
    /// (SYSTEM) but skip USER.md and HEARTBEAT.md (those are for
    /// interactive use; cron is fresh-context).
    pub fn cron_system_prompt(&self) -> SystemPrompt {
        SystemPrompt::from_sections(&[(SYSTEM, self.system.as_deref())])
    }
}

#[derive(Debug, Clone, Default)]
pub struct SystemPrompt {
    text: Option<String>,
}

impl SystemPrompt {
    fn from_sections(sections: &[(&str, Option<&str>)]) -> Self {
        let mut chunks: Vec<String> = Vec::new();
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
        assert!(bf.system_prompt().as_text().is_none());
    }

    #[tokio::test]
    async fn whitespace_only_files_are_skipped() {
        let dir = tempdir();
        tokio::fs::write(dir.join(SYSTEM), "   \n\n\t\n")
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
    async fn order_is_system_user_heartbeat() {
        let dir = tempdir();
        tokio::fs::write(dir.join(SYSTEM), "system-content")
            .await
            .expect("write");
        tokio::fs::write(dir.join(USER), "user-content")
            .await
            .expect("write");
        tokio::fs::write(dir.join(HEARTBEAT), "heartbeat-content")
            .await
            .expect("write");

        let bf = BootstrapFiles::load(&dir).await.expect("load");
        let prompt = bf.system_prompt();
        let text = prompt.as_text().expect("some");
        let system_pos = text.find("system-content").unwrap();
        let user_pos = text.find("user-content").unwrap();
        let heartbeat_pos = text.find("heartbeat-content").unwrap();
        assert!(system_pos < user_pos);
        assert!(user_pos < heartbeat_pos);
    }

    #[tokio::test]
    async fn cron_prompt_keeps_only_system() {
        let dir = tempdir();
        tokio::fs::write(dir.join(SYSTEM), "you are alice; concise")
            .await
            .expect("write");
        tokio::fs::write(dir.join(USER), "user is bob")
            .await
            .expect("write");
        tokio::fs::write(dir.join(HEARTBEAT), "check email")
            .await
            .expect("write");

        let bf = BootstrapFiles::load(&dir).await.expect("load");
        let cron = bf.cron_system_prompt();
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
    async fn legacy_identity_only_loads_as_system() {
        let dir = tempdir();
        tokio::fs::write(dir.join(LEGACY_IDENTITY), "you are alice")
            .await
            .expect("write");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert_eq!(bf.system.as_deref(), Some("you are alice"));
    }

    #[tokio::test]
    async fn legacy_soul_only_loads_as_system() {
        let dir = tempdir();
        tokio::fs::write(dir.join(LEGACY_SOUL), "concise")
            .await
            .expect("write");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert_eq!(bf.system.as_deref(), Some("concise"));
    }

    #[tokio::test]
    async fn legacy_identity_and_soul_concat_in_order() {
        let dir = tempdir();
        tokio::fs::write(dir.join(LEGACY_IDENTITY), "alice")
            .await
            .expect("w");
        tokio::fs::write(dir.join(LEGACY_SOUL), "concise")
            .await
            .expect("w");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        let s = bf.system.as_deref().unwrap();
        assert!(s.find("alice").unwrap() < s.find("concise").unwrap());
    }

    #[tokio::test]
    async fn canonical_system_wins_over_legacy() {
        // If both new and old files exist, canonical is preferred —
        // the operator presumably already migrated and forgot to
        // delete the originals.
        let dir = tempdir();
        tokio::fs::write(dir.join(SYSTEM), "canonical")
            .await
            .expect("w");
        tokio::fs::write(dir.join(LEGACY_IDENTITY), "legacy")
            .await
            .expect("w");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert_eq!(bf.system.as_deref(), Some("canonical"));
    }
}
