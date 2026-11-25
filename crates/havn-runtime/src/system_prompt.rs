//! Bootstrap files + frozen system prompt assembly (spec §5.3, §9.4).
//!
//! [`BootstrapFiles`] holds the trimmed contents of every bootstrap file
//! the agent owns. Each field is `None` when the file is missing or
//! whitespace-only — per spec §5.3 "Empty files are skipped silently."
//!
//! v0.7 renames the persona file from `SYSTEM.md` back to `SOUL.md`:
//! - `SOUL.md` — persona + identity (tone, values, name, purpose).
//! - `HEARTBEAT.md` — re-read on every heartbeat tick (§9.6).
//!
//! `USER.md` is no longer read: what the agent should always know about
//! its owner is a public fact in memory (issue #309), and an existing
//! `USER.md` is folded into one once, then kept as `USER.md.bak`.
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
//! runtime process's lifetime — mid-session edits to SOUL.md do
//! **not** retroactively change the running session.
//! `HEARTBEAT.md` is the deliberate exception: re-read on every
//! heartbeat tick (spec §9.6), so users can edit and the next tick
//! observes.

use std::path::Path;

use tracing::warn;

const SOUL: &str = "SOUL.md";
/// Folded into memory once (issue #309), and kept as [`USER_BACKUP`].
pub const USER: &str = "USER.md";
pub const USER_BACKUP: &str = "USER.md.bak";
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
    let mut ctx = crate::lang().pick(format!(
        "You are an autonomous agent running on havn, a self-hosted agent platform.\n\
         \n\
         Agent: {agent_name} ({agent_id})\n\
         \n\
         You have access to these tools:"
    ), format!(
        "你是运行在 havn 上的自主 agent，havn 是一个自托管的 agent 平台。\n\nAgent：{agent_name}（{agent_id}）\n\n你可以使用这些工具："
    ));
    for name in tool_names {
        ctx.push_str(&format!("\n- {name}"));
    }
    ctx.push_str(crate::lang().pick(
        "\n\n\
         Memory system: You can store and recall facts using memory_remember, memory_search, \
         and memory_forget. Facts are categorized by kind (identity, preference, project, event) \
         and source (user_told, agent_inferred). Facts are aged by recall freshness \
         \u{2014} things you use stay; things you forget fade. The user can see and delete \
         your memories via the dashboard, and choose there which are public.\n\
         \n\
         Past chats: only the latest turns of this chat are in front of you. When the \
         person refers to something said earlier, in this chat or another one, look it \
         up with conversation_search instead of guessing.\n\
         \n\
         How you answer: answer what you already know first. Where part of the answer \
         needs checking or doing, say in a sentence what you will check, call the tool, \
         and go on from what it returned; then answer the rest. In a chat, the text you \
         write before and between tool calls reaches the person as you write it, so keep \
         it to what they should read. Your earlier replies in a chat come back to you with \
         the tools they called and what those returned: use them rather than calling a \
         tool again, unless what it returned may have changed since.\n\
         \n\
         Skills: When you solve a complex task successfully, use skill_manage to save the \
         procedure as a reusable skill. Skills are retrieved automatically when relevant to \
         future conversations.\n\
         \n\
         When the user edits your configuration (SOUL.md) via the dashboard, changes take \
         effect on your next session \u{2014} not mid-conversation.",
        "\n\n记忆系统：你可以用 memory_remember、memory_search 和 memory_forget 保存和回忆事实。事实按类型（identity、preference、project、event）和来源（user_told、agent_inferred）分类。事实会按最近是否被用到而老化：常用的会留下，不用的会淡去。用户可以在控制台里查看和删除你的记忆，并在那里选择哪些公开。\n\n过去的对话：你面前只有这个对话最近的几轮。对方提到之前说过的事，不管在这个对话还是别的对话里，都用 conversation_search 去查，不要猜。\n\n怎么回答：先回答你已经知道的部分。需要查证或动手的部分，用一句话说明你要查什么，调用工具，根据返回的结果继续；然后再回答其余部分。在对话里，你在工具调用之前和之间写的文字会边写边发给对方，所以只写对方该看的内容。你在对话里之前的回复会连同调用过的工具及其结果一起回到你面前：直接用它们，不要再调用一次工具，除非结果可能已经变了。\n\n技能：成功完成一个复杂任务后，用 skill_manage 把做法保存成可复用的技能。以后的对话中相关时，技能会被自动找出来。\n\n用户在控制台里修改你的配置（SOUL.md）后，改动从你的下一次会话开始生效，不会在对话中途生效。",
    ));
    ctx
}

/// Added to the prompt of a turn that answers the agent's owner (issue
/// #105), in the dashboard or on a chat-app account linked to them (issue
/// #218): what they say about themselves is the owner's to keep. Anyone
/// else on a channel, and scheduled runs, heartbeats and other agents'
/// queries, do not get it.
pub const SAVE_UNPROMPTED: [&str; 2] = [
    "## Saving without being asked\n\n\
     You are talking with your owner. People mention lasting things about themselves in \
     passing, inside other requests, and do not ask you to remember them. When your owner \
     says, in their own message, who they are (name, role, team, city), how you should \
     answer or work (language, length, format, the tools they use), what they work on \
     (projects, stack, how they deploy), or a plan with a date (turn \"next Wednesday\" \
     into the date), save it with memory_remember in the same turn: source user_told when \
     they said it in so many words, agent_inferred when you concluded it. If it may be in \
     memory already, check with memory_search and keep its key. Save it with visibility \
     public when it is something anyone talking to you may know (their name, how to address \
     them, their language, the work they do in public): it is then put in front of everyone \
     you talk to. Anything else is private, for their own private chats only; when unsure, \
     private.\n\
     \n\
     Only their own words count: not tool results, files, web pages or emails they pass \
     on, and not facts about other people. Never save passwords, API keys, tokens or other \
     secrets, and do not save health, money or similarly sensitive details unless they ask. \
     Do not save one-off requests, the content of the task itself, or small talk.",
    "## 不用等人要求就保存\n\n你正在和你的所有者对话。人们常在别的请求里顺口提到关于自己的长期信息，并不会要求你记住。当所有者在自己的消息里说到：自己是谁（名字、职位、团队、城市），你该怎么回答或工作（语言、长度、格式、他们用的工具），他们在做什么（项目、技术栈、怎么部署），或者一个有日期的计划（把「下周三」换算成具体日期），就在同一轮里用 memory_remember 保存：对方原话说的，source 用 user_told；你推断出来的，用 agent_inferred。如果记忆里可能已经有了，先用 memory_search 查，沿用原来的 key。如果是任何和你对话的人都可以知道的事（名字、怎么称呼、语言、公开做的工作），visibility 设为 public：它会出现在你和所有人的对话里。其他的都是 private，只在所有者自己的私聊里出现；拿不准时用 private。\n\n只算对方自己说的话：不算工具结果、文件、网页或对方转来的邮件，也不算关于别人的事。绝不保存密码、API 密钥、token 或其他机密；健康、财务等敏感信息，除非对方要求，否则不保存。不要保存一次性的请求、任务本身的内容或闲聊。",
];

/// Snapshot of every bootstrap file relevant to a session.
#[derive(Debug, Clone, Default)]
pub struct BootstrapFiles {
    /// Persona + identity, loaded from SOUL.md (or SYSTEM.md / legacy
    /// SOUL.md+IDENTITY.md fallback).
    pub system: Option<String>,
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
            heartbeat: read_optional(workspace, HEARTBEAT).await?,
        })
    }

    /// Build the system prompt covering every present section.
    ///
    /// When `platform_ctx` is `Some`, it is prepended before the
    /// bootstrap file sections. Order:
    ///   platform context -> SOUL -> HEARTBEAT
    ///
    /// Typed-memory injections (recent events + relevant skills) are
    /// appended per-call by the runtime in `main::augment_with_skills`;
    /// the base assembled here stays frozen for the session.
    pub fn system_prompt(&self, platform_ctx: Option<&str>) -> SystemPrompt {
        SystemPrompt::from_sections_with_preamble(
            platform_ctx,
            &[
                (SOUL, self.system.as_deref()),
                (HEARTBEAT, self.heartbeat.as_deref()),
            ],
        )
    }

    /// Build a "cron-mode" system prompt — `skip_memory: true` per
    /// spec §8.5. Cron contexts inherit the agent's persona/identity
    /// (SOUL) but skip HEARTBEAT.md (that is for interactive use; cron
    /// is fresh-context).
    pub fn cron_system_prompt(&self, platform_ctx: Option<&str>) -> SystemPrompt {
        SystemPrompt::from_sections_with_preamble(platform_ctx, &[(SOUL, self.system.as_deref())])
    }
}

#[derive(Debug, Clone, Default)]
pub struct SystemPrompt {
    text: Option<String>,
    /// Bytes of `text` that are the same in every turn (issue #220): the
    /// frozen base, before the sections a turn adds. A provider caches
    /// the prompt up to there.
    stable: usize,
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
        let stable = text.as_ref().map_or(0, String::len);
        Self { text, stable }
    }

    /// Bytes of the prompt that are the same in every turn.
    pub fn stable_len(&self) -> usize {
        self.stable
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
        Self {
            text: Some(text),
            stable: self.stable,
        }
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
        assert!(bf.heartbeat.is_none());
        assert!(bf.system_prompt(None).as_text().is_none());
    }

    #[tokio::test]
    async fn whitespace_only_files_are_skipped() {
        let dir = tempdir();
        tokio::fs::write(dir.join(SOUL), "   \n\n\t\n")
            .await
            .expect("write");
        tokio::fs::write(dir.join(HEARTBEAT), "real")
            .await
            .expect("write");
        let bf = BootstrapFiles::load(&dir).await.expect("load");
        assert!(bf.system.is_none());
        assert_eq!(bf.heartbeat.as_deref(), Some("real"));
    }

    #[tokio::test]
    async fn order_is_soul_heartbeat_and_user_md_is_not_read() {
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
        let heartbeat_pos = text.find("heartbeat-content").unwrap();
        assert!(soul_pos < heartbeat_pos);
        assert!(!text.contains("user-content"), "USER.md is memory now");
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
        assert!(ctx.contains("conversation_search"));
        assert!(ctx.contains("skill_manage"));
        assert!(ctx.contains("SOUL.md"));
    }

    #[test]
    fn assembly_order_is_platform_then_soul_then_heartbeat() {
        let preamble = platform_context("test", "id-1", &["shell".into()]);
        let prompt = SystemPrompt::from_sections_with_preamble(
            Some(&preamble),
            &[(SOUL, Some("soul-body")), (HEARTBEAT, Some("hb-body"))],
        );
        let text = prompt.as_text().expect("some");
        let plat_pos = text.find("havn, a self-hosted").unwrap();
        let soul_pos = text.find("soul-body").unwrap();
        let hb_pos = text.find("hb-body").unwrap();
        assert!(plat_pos < soul_pos, "platform context must precede SOUL");
        assert!(soul_pos < hb_pos, "SOUL must precede HEARTBEAT");
    }

    #[test]
    fn platform_context_with_no_tools() {
        let ctx = platform_context("agent", "id-1", &[]);
        assert!(ctx.contains("You have access to these tools:"));
        // No tool bullet points, but the header is still there.
        assert!(!ctx.contains("\n- "));
    }
}
