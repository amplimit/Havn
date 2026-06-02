//! Per-agent workspace directory management.
//!
//! Each agent owns a directory at `<workspace_root>/<agent_id>/workspace/`
//! containing the bootstrap files defined in spec §5.3 plus a `skills/`
//! subdirectory. Files are created empty — the runtime silently skips
//! empty bootstrap files at session start (spec §5.3: "Empty files are
//! skipped silently"). A `README.md` explains the layout to anyone who
//! opens the workspace by hand.

use std::path::Path;

const README: &str = "# This agent's workspace

This directory contains files the agent reads at every session start:

- `SOUL.md` — Persona + identity (tone, values, communication style, name, purpose).
- `USER.md` — What the agent should always know about you.
- `HEARTBEAT.md` — Periodic self-tick instructions (spec §9.6).

All three are optional and user-editable. Empty files are silently skipped.
Mid-session edits do not retroactively change a running session — see the
frozen-prompt invariant in spec §9.4. (`HEARTBEAT.md` is the deliberate
exception: re-read on every heartbeat tick.)

`skills/` holds workspace-installed and agent-created skills.
`agent.db` is the agent's per-workspace SQLite (conversations, typed memory,
skill index). The dashboard's /memory and /skills pages read this file
directly via a read-only handle.

Earlier drafts of havn shipped four bootstrap files (SOUL, IDENTITY, USER,
MEMORY, HEARTBEAT). v0.6 collapsed them into SYSTEM.md; v0.7 renamed it to
SOUL.md. The runtime still tolerates the old filenames for existing workspaces.
";

const BOOTSTRAP_FILES: &[&str] = &["SOUL.md", "USER.md", "HEARTBEAT.md"];

/// Create the workspace directory and all bootstrap files if they don't already exist.
/// Idempotent — safe to call on an existing workspace.
pub async fn ensure(workspace_dir: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(workspace_dir).await?;
    tokio::fs::create_dir_all(workspace_dir.join("skills")).await?;

    let readme_path = workspace_dir.join("README.md");
    if !tokio::fs::try_exists(&readme_path).await? {
        tokio::fs::write(&readme_path, README).await?;
    }

    for name in BOOTSTRAP_FILES {
        let path = workspace_dir.join(name);
        if !tokio::fs::try_exists(&path).await? {
            tokio::fs::write(&path, "").await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn creates_full_layout() {
        let dir = tempdir();
        let workspace = dir.join("workspace");
        ensure(&workspace).await.expect("create");
        for name in BOOTSTRAP_FILES {
            assert!(workspace.join(name).exists(), "{name} missing");
        }
        assert!(workspace.join("README.md").exists());
        assert!(workspace.join("skills").is_dir());
    }

    #[tokio::test]
    async fn idempotent_does_not_overwrite_user_content() {
        let dir = tempdir();
        let workspace = dir.join("workspace");
        ensure(&workspace).await.expect("first");

        let soul = workspace.join("SOUL.md");
        tokio::fs::write(&soul, "user content")
            .await
            .expect("write");

        ensure(&workspace).await.expect("second");
        let after = tokio::fs::read_to_string(&soul).await.expect("read");
        assert_eq!(after, "user content");
    }

    #[tokio::test]
    async fn does_not_create_legacy_bootstrap_files() {
        // v0.7 creates SOUL.md (not SYSTEM.md). Legacy filenames like
        // IDENTITY.md / MEMORY.md / SYSTEM.md are not created — the
        // runtime's BootstrapFiles loader still reads them when present.
        let dir = tempdir();
        let workspace = dir.join("workspace");
        ensure(&workspace).await.expect("create");
        for legacy in ["SYSTEM.md", "IDENTITY.md", "MEMORY.md"] {
            assert!(
                !workspace.join(legacy).exists(),
                "{legacy} should not be created in v0.7"
            );
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("havn-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }
}
