//! Typed memory tools (spec §9.4): `memory_remember`, `memory_search`,
//! `memory_forget`. Backed by the agent-side SQLite `memory` table with
//! `kind` + `source` + `ttl_days` + `archived_at`. And
//! `conversation_search` over the past chats of the person talking
//! (issue #105).
//!
//! From the user's seat: the agent saves facts about you in four
//! categories so the dashboard can show them grouped, and so the daily
//! aging pass knows which facts to expire (events / projects) versus
//! which to keep forever (identity / preferences). Old "memory_store"
//! wrote untyped key/value blobs; this surface replaces it.

use std::fmt::Write as _;

use async_trait::async_trait;
use havn_db::agent::conversations::{self, ChatTurn};
use havn_db::agent::memory::{self, Kind, NewEntry, Source};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolResult};

tokio::task_local! {
    /// What the memory tools change in a reply's turn, for the dashboard
    /// to show under the reply (issue #105). Set around a user turn.
    pub static MEMORY_CHANGES: std::sync::Arc<std::sync::Mutex<Vec<Noted>>>;
}

/// A turn's net change to one key: the entry before its first change
/// and the one after its last.
#[derive(Debug, Clone)]
pub struct Noted {
    key: String,
    /// Row id, value and kind.
    before: Option<(String, String, Kind)>,
    after: Option<(String, String, Kind)>,
}

/// Note a change to `key` from `before` to `after` (row id, value, kind).
fn note_change(
    key: &str,
    before: Option<(String, String, Kind)>,
    after: Option<(String, String, Kind)>,
) {
    let _ = MEMORY_CHANGES.try_with(|changes| {
        let mut changes = changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(noted) = changes.iter_mut().find(|n| n.key == key) {
            noted.after = after;
        } else if changes.len() < havn_proto::MAX_MEMORY_CHANGES {
            changes.push(Noted {
                key: key.to_string(),
                before,
                after,
            });
        }
    });
}

/// What `noted` changed, as the dashboard shows it: a key left as it was
/// (an entry repeated, or saved then forgotten) is not shown.
pub fn changes(noted: Vec<Noted>) -> Vec<havn_proto::MemoryChange> {
    noted
        .into_iter()
        .filter_map(|n| {
            let same = match (&n.before, &n.after) {
                (Some(b), Some(a)) => b.0 == a.0,
                (None, None) => true,
                _ => false,
            };
            if same {
                return None;
            }
            let (action, value, kind) = match (&n.after, &n.before) {
                (Some((_, value, kind)), _) => (havn_proto::MemoryAction::Saved, value, kind),
                (None, Some((_, value, kind))) => (havn_proto::MemoryAction::Forgot, value, kind),
                (None, None) => return None,
            };
            Some(havn_proto::MemoryChange {
                action,
                key: n.key.clone(),
                value: value
                    .chars()
                    .take(havn_proto::MAX_MEMORY_CHANGE_CHARS)
                    .collect(),
                kind: kind.as_str().to_string(),
                before: n.before.as_ref().map(|b| b.0.clone()),
                after: n.after.as_ref().map(|a| a.0.clone()),
            })
        })
        .collect()
}

const SEARCH_LIMIT: u32 = 10;

/// Cap on `value` length per row. 4 KB is generous for a single fact and
/// keeps any one row from dominating the FTS5 token budget.
const MAX_VALUE_BYTES: usize = havn_proto::MAX_MEMORY_VALUE_BYTES;

pub struct MemoryRememberTool;

/// Whether this turn may change the agent's memory (issue #218): one
/// answering its owner, the heartbeat, or a task the owner scheduled.
/// Anything else (someone else's chat, their scheduled task, another
/// agent's question) may not.
fn may_change_memory() -> bool {
    super::gateway::FROM_OWNER
        .try_with(|owner| *owner)
        .unwrap_or(false)
}

const NOT_THE_OWNER: [&str; 2] = [
    "only your owner's conversations change your memory, and this one is with someone else: \
     nothing was saved or forgotten",
    "只有和所有者的对话才能改动你的记忆，而这个对话是和别人的：什么都没有保存或删除",
];

#[async_trait]
impl Tool for MemoryRememberTool {
    fn name(&self) -> &'static str {
        "memory_remember"
    }

    fn description(&self) -> &'static str {
        "Save a typed fact to long-term memory. Pick `kind` carefully: \
         'identity' for stable facts about the user (name, role); \
         'preference' for durable preferences and corrections; \
         'project' for facts about current work that may go stale; \
         'event' for time-stamped incidents (default 30-day expiry). \
         Set `source: 'user_told'` when the user said it directly, \
         'agent_inferred' when you guessed it. Replaces any existing \
         entry with the same key. The dashboard shows users exactly \
         what you've remembered — keep entries short and specific."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Short stable identifier (e.g., 'user.name', 'project.repo', 'event.2026-05-03.shipped-1.4')."
                },
                "value": {
                    "type": "string",
                    "description": "The fact itself, plain text, ≤ 4 KB."
                },
                "kind": {
                    "type": "string",
                    "enum": ["identity", "preference", "project", "event"],
                    "description": "Category — drives lifetime + dashboard grouping."
                },
                "source": {
                    "type": "string",
                    "enum": ["user_told", "agent_inferred"],
                    "default": "agent_inferred"
                },
                "ttl_days": {
                    "type": "integer",
                    "description": "Override default TTL. Omit to use the kind's default (identity/preference: never expire; project: 90; event: 30)."
                }
            },
            "required": ["key", "value", "kind"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        if !may_change_memory() {
            return ToolResult::Error(crate::lang().of(NOT_THE_OWNER).into());
        }
        let Some(key) = input.get("key").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: key".into());
        };
        let Some(value) = input.get("value").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: value".into());
        };
        let Some(kind_raw) = input.get("kind").and_then(Value::as_str) else {
            return ToolResult::Error(
                "missing required field: kind (identity|preference|project|event)".into(),
            );
        };
        let Some(kind) = Kind::parse(kind_raw) else {
            return ToolResult::Error(format!(
                "invalid kind {kind_raw:?}; expected one of identity|preference|project|event"
            ));
        };
        let source = input
            .get("source")
            .and_then(Value::as_str)
            .map_or(Source::AgentInferred, |s| {
                Source::parse(s).unwrap_or(Source::AgentInferred)
            });
        let ttl_days = input.get("ttl_days").and_then(Value::as_i64);

        if let Err(why) = havn_proto::check_memory_key(key) {
            return ToolResult::Error(why);
        }

        let (_, truncated) = match save(
            false,
            ctx,
            NewEntry {
                key,
                value,
                kind,
                source,
                ttl_days,
            },
        )
        .await
        {
            Ok(saved) => saved,
            Err(e) => return ToolResult::Error(format!("memory_remember failed: {e}")),
        };

        let suffix = if truncated {
            " (value was truncated to the memory entry size cap)"
        } else {
            ""
        };
        ToolResult::Ok(format!("remembered {key:?} as {}{suffix}", kind.as_str()))
    }
}

/// Save `new` as the active entry for its key, cutting a value over
/// [`MAX_VALUE_BYTES`] with a marker, and embed it when an embedder is
/// wired: the row's id, and whether the value was cut. With
/// `until_forgotten` it never expires (`new.ttl_days` is ignored). What
/// the `memory_remember` tool and the dashboard save go through.
pub async fn save(
    until_forgotten: bool,
    ctx: &ToolCtx,
    new: NewEntry<'_>,
) -> havn_db::Result<(String, bool)> {
    // Truncate-with-marker rather than reject: the agent can still write
    // the important prefix and sees in the result that it was cut.
    let (stored_value, truncated) = if new.value.len() > MAX_VALUE_BYTES {
        const MARKER: &str = "\n…(truncated to fit memory entry size cap)";
        let room = MAX_VALUE_BYTES - MARKER.len();
        let end = new
            .value
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|&end| end <= room)
            .last()
            .unwrap_or(0);
        (format!("{}{MARKER}", &new.value[..end]), true)
    } else {
        (new.value.to_string(), false)
    };
    let key = new.key;
    // The entry it replaces, if any, for the chat's note of the change.
    let before = memory::get(&ctx.agent_db, key)
        .await
        .ok()
        .flatten()
        .map(|e| (e.id, e.value, e.kind));
    let entry = NewEntry {
        key,
        value: &stored_value,
        kind: new.kind,
        source: new.source,
        ttl_days: new.ttl_days,
    };
    let row_id = if until_forgotten {
        memory::remember_until_forgotten(&ctx.agent_db, entry).await?
    } else {
        memory::remember(&ctx.agent_db, entry).await?
    };
    // Noted as soon as it is written: a turn stopped while it embeds
    // keeps the entry, and shows it.
    note_change(
        key,
        before,
        Some((row_id.clone(), stored_value.clone(), new.kind)),
    );

    // Hybrid retrieval (spec §9.4 v0.7): embed `"<key>: <value>"`, pinned
    // to the row id so a concurrent write to the same key cannot get this
    // vector. A failure leaves the row BM25-only until the startup
    // backfill heals it.
    if let Some(emb) = &ctx.embedder
        && !row_id.is_empty()
    {
        match emb.embed(&format!("{key}: {stored_value}")).await {
            Ok(vec) => {
                if let Err(e) = memory::set_embedding_by_id(&ctx.agent_db, &row_id, &vec).await {
                    tracing::warn!(key, error = %e, "set_embedding_by_id failed; row stored without vector");
                }
            }
            Err(e) => {
                tracing::warn!(key, error = %e, "embedder failed; row stored without vector");
            }
        }
    }
    Ok((row_id, truncated))
}

pub struct MemorySearchTool;

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }

    fn description(&self) -> &'static str {
        "Search active memory entries. Returns up to 10 hits ranked by \
         relevance. Pass `kinds: [...]` to restrict to specific memory \
         categories (e.g. only 'event' for 'what happened recently'). \
         For what was said in earlier chats, use conversation_search."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search terms." },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["identity", "preference", "project", "event"] },
                    "description": "Optional — restrict results to these memory kinds."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(raw_query) = input.get("query").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: query".into());
        };
        if raw_query.trim().is_empty() {
            return ToolResult::Error("query must be non-empty".into());
        }
        let kinds: Vec<Kind> = input
            .get("kinds")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().and_then(Kind::parse))
                    .collect()
            })
            .unwrap_or_default();
        // Hybrid search (spec §9.4 v0.7): combines BM25 + cosine
        // when an embedder is wired, falls back to FTS5-only when
        // not. Hybrid takes the RAW query (it escapes for FTS
        // internally; embedding uses natural language).
        let mem_source = crate::embedding::hybrid::MemorySource::new(&ctx.agent_db);
        let memory_hits = match crate::embedding::hybrid::search(
            &mem_source,
            &ctx.embedder,
            raw_query,
            &kinds,
            SEARCH_LIMIT,
            crate::embedding::hybrid::HybridParams::default(),
        )
        .await
        {
            Ok(hits) => hits,
            Err(e) => return ToolResult::Error(format!("memory search failed: {e}")),
        };
        if memory_hits.is_empty() {
            return ToolResult::Ok("(no matches)".into());
        }

        // In someone else's conversation, the owner sees this before the
        // model does (issue #255).
        let mut out = String::new();
        if !memory_hits.is_empty() {
            out.push_str("# Memory entries\n\n");
            for entry in memory_hits {
                writeln!(
                    out,
                    "- [{}] `{}`: {}",
                    entry.kind.as_str(),
                    entry.key,
                    entry.value
                )
                .ok();
            }
            out.push('\n');
        }
        ToolResult::Ok(out)
    }
}

/// Turns `conversation_search` shows before and after the one asked for.
const CONTEXT_TURNS: u32 = 3;
/// Characters of a found turn shown, around the first term it matched.
const EXCERPT_CHARS: usize = 300;
/// Characters of each turn shown around a found one.
const AROUND_CHARS: usize = 1200;

pub struct ConversationSearchTool;

#[async_trait]
impl Tool for ConversationSearchTool {
    fn name(&self) -> &'static str {
        "conversation_search"
    }

    fn description(&self) -> &'static str {
        "Search the earlier chats of the person you are talking to, \
         including turns no longer in the conversation you see, and their \
         titles. Every word of `query` must appear in a turn or a title; words of three or more \
         characters match anywhere inside text, so for Chinese or \
         Japanese give a few words rather than a whole sentence. Returns up \
         to 10 turns with their chat title, date and turn id. Pass `turn` \
         (an id from a result) instead to read the turns around it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Words to look for." },
                "turn": {
                    "type": "string",
                    "description": "A turn id from an earlier result, to read the turns around it."
                }
            }
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        // The chats of the scope the message came from (a dashboard user,
        // a channel sender): other people's chats with this agent stay
        // out of reach.
        let Ok(scope) = super::gateway::CONVERSATION.try_with(Clone::clone) else {
            return ToolResult::Error(
                "conversation_search works only while answering a chat message".into(),
            );
        };
        let current = super::gateway::CHAT.try_with(Clone::clone).ok();
        // In a group, what it finds is read by everyone there: the owner's
        // other chats stay out of it (issue #291).
        let in_group = super::gateway::IN_GROUP
            .try_with(|group| *group)
            .unwrap_or(false);
        let owner = !in_group
            && super::gateway::FROM_OWNER
                .try_with(|owner| *owner)
                .unwrap_or(false);
        if let Some(turn) = input.get("turn").and_then(Value::as_str) {
            return match conversations::around(&ctx.agent_db, &scope, owner, turn, CONTEXT_TURNS)
                .await
            {
                Ok(Some(turns)) => ToolResult::Ok(show_around(&turns, turn, current.as_deref())),
                Ok(None) => ToolResult::Error(format!("no turn {turn:?} in these chats")),
                Err(e) => ToolResult::Error(format!("conversation_search failed: {e}")),
            };
        }
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() {
            return ToolResult::Error("give `query` (words to look for) or `turn`".into());
        }
        let hits =
            match conversations::search_chats(&ctx.agent_db, &scope, owner, query, SEARCH_LIMIT)
                .await
            {
                Ok(hits) => hits,
                Err(e) => return ToolResult::Error(format!("conversation_search failed: {e}")),
            };
        let titled =
            match conversations::chats_titled(&ctx.agent_db, &scope, owner, query, SEARCH_LIMIT)
                .await
            {
                Ok(chats) => chats,
                Err(e) => return ToolResult::Error(format!("conversation_search failed: {e}")),
            };
        if hits.is_empty() && titled.is_empty() {
            return ToolResult::Ok("(no matches)".into());
        }
        let mut out = String::new();
        if !titled.is_empty() {
            out.push_str("Chats titled so:\n");
            for chat in &titled {
                let here = if current.as_deref() == Some(chat.id.as_str()) {
                    " (this chat)"
                } else {
                    ""
                };
                match &chat.first_turn {
                    Some(turn) => writeln!(
                        out,
                        "- chat {:?}{here}, last used {}: it starts at turn {turn}",
                        chat.title,
                        chat.updated_at.get(..10).unwrap_or(&chat.updated_at),
                    ),
                    None => writeln!(out, "- chat {:?}{here}: no messages yet", chat.title),
                }
                .ok();
            }
            if !hits.is_empty() {
                out.push_str("Turns:\n");
            }
        }
        for hit in &hits {
            writeln!(
                out,
                "- [{}] {} in {}{} (turn {}): {}",
                hit.created_at.format("%Y-%m-%d %H:%M"),
                hit.role.as_str(),
                chat_name(hit),
                if current.as_deref() == Some(hit.chat.as_str()) {
                    " (this chat)"
                } else {
                    ""
                },
                hit.id,
                excerpt(&hit.content, &terms, EXCERPT_CHARS),
            )
            .ok();
        }
        ToolResult::Ok(out)
    }
}

fn chat_name(turn: &ChatTurn) -> String {
    if turn.title.is_empty() {
        "an untitled chat".into()
    } else {
        format!("chat {:?}", turn.title)
    }
}

fn show_around(turns: &[ChatTurn], asked: &str, current: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(first) = turns.first() {
        let here = if current == Some(first.chat.as_str()) {
            " (this chat)"
        } else {
            ""
        };
        writeln!(out, "From {}{here}:", chat_name(first)).ok();
    }
    for turn in turns {
        writeln!(
            out,
            "{} [{}] {}: {}",
            if turn.id == asked { ">" } else { "-" },
            turn.created_at.format("%Y-%m-%d %H:%M"),
            turn.role.as_str(),
            truncate(&turn.content, AROUND_CHARS),
        )
        .ok();
    }
    out
}

/// At most `max` characters of `content`, around the first of `terms` it
/// contains (ignoring case).
pub(crate) fn excerpt(content: &str, terms: &[&str], max: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= max {
        return content.to_string();
    }
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let lower: Vec<char> = chars.iter().copied().map(fold).collect();
    let at = terms
        .iter()
        .filter_map(|term| {
            let term: Vec<char> = term.chars().map(fold).collect();
            if term.is_empty() {
                return None;
            }
            lower.windows(term.len()).position(|w| w == term.as_slice())
        })
        .min()
        .unwrap_or(0);
    let start = at.saturating_sub(max / 3).min(chars.len() - max);
    let mut out = String::new();
    if start > 0 {
        out.push('\u{2026}');
    }
    out.extend(&chars[start..start + max]);
    if start + max < chars.len() {
        out.push('\u{2026}');
    }
    out
}

pub struct MemoryForgetTool;

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &'static str {
        "memory_forget"
    }

    fn description(&self) -> &'static str {
        "Soft-delete a memory entry by key. The row stays in the table \
         (the dashboard still shows it as archived) so the user has an \
         audit trail. Use when the user says 'forget X' or when a fact \
         is clearly stale and contradicted."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        if !may_change_memory() {
            return ToolResult::Error(crate::lang().of(NOT_THE_OWNER).into());
        }
        let Some(key) = input.get("key").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: key".into());
        };
        // What it said, for the dashboard to offer it back.
        let before = memory::get(&ctx.agent_db, key).await.ok().flatten();
        match memory::forget(&ctx.agent_db, key).await {
            Ok(true) => {
                if let Some(entry) = before {
                    note_change(key, Some((entry.id, entry.value, entry.kind)), None);
                }
                ToolResult::Ok(format!("forgot {key:?}"))
            }
            Ok(false) => ToolResult::Ok(format!(
                "no active entry for {key:?} (already archived or never existed)"
            )),
            Err(e) => ToolResult::Error(format!("memory_forget failed: {e}")),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn only_the_owner_s_turns_change_memory() {
        assert!(!may_change_memory(), "unless the owner's, it may not");
        let refused = super::super::gateway::FROM_OWNER
            .scope(false, async { may_change_memory() })
            .await;
        assert!(!refused);
        let owner = super::super::gateway::FROM_OWNER
            .scope(true, async { may_change_memory() })
            .await;
        assert!(owner);
    }
    use havn_core::Policy;
    use havn_db::agent::connect_in_memory;
    use std::sync::Arc;

    async fn ctx() -> ToolCtx {
        let pool = connect_in_memory().await.expect("agent db");
        let workspace = std::env::temp_dir().join(format!("havn-tools-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&workspace).await.expect("ws");
        ToolCtx {
            workspace_dir: workspace,
            agent_db: pool,
            policy: Arc::new(Policy::default()),
            embedder: None,
        }
    }

    #[tokio::test]
    async fn remember_then_search_roundtrip() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
        let ctx = ctx().await;
        let store = MemoryRememberTool;
        let r = store
            .execute(
                &ctx,
                json!({"key": "user.name", "value": "Ada Lovelace", "kind": "identity", "source": "user_told"}),
            )
            .await;
        assert!(!r.is_error(), "remember: {}", r.text());

        let search = MemorySearchTool;
        let r = search.execute(&ctx, json!({"query": "Ada"})).await;
        assert!(!r.is_error(), "search: {}", r.text());
        let body = r.text();
        assert!(body.contains("user.name"), "{body}");
        assert!(body.contains("Ada Lovelace"), "{body}");
        assert!(body.contains("[identity]"), "kind tag missing: {body}");
                })
            .await;
    }

    #[tokio::test]
    async fn remember_rejects_missing_kind() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                let ctx = ctx().await;
                let store = MemoryRememberTool;
                let r = store.execute(&ctx, json!({"key": "k", "value": "v"})).await;
                assert!(r.is_error(), "{}", r.text());
                assert!(r.text().contains("kind"));
            })
            .await;
    }

    #[tokio::test]
    async fn remember_truncates_oversize_value_with_marker() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                // Audit-found: rejecting an oversize value forces the agent to
                // re-call with a smaller value, wasting a turn. Truncation +
                // explicit marker is gentler — the important prefix gets
                // stored, and the LLM is told it happened.
                let ctx = ctx().await;
                let store = MemoryRememberTool;
                let big = "x".repeat(8192);
                let r = store
                    .execute(
                        &ctx,
                        json!({"key": "k", "value": big, "kind": "preference"}),
                    )
                    .await;
                assert!(!r.is_error(), "should accept oversized: {}", r.text());
                assert!(
                    r.text().contains("truncated"),
                    "result should mention truncation: {}",
                    r.text()
                );
                // The stored value should be capped and end with the sentinel.
                let pool = &ctx.agent_db;
                let row = havn_db::agent::memory::get(pool, "k")
                    .await
                    .expect("get")
                    .expect("some");
                assert!(row.value.len() <= 4096);
                assert!(
                    row.value
                        .ends_with("(truncated to fit memory entry size cap)")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn search_filters_by_kind() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                let ctx = ctx().await;
                let store = MemoryRememberTool;
                store
                    .execute(
                        &ctx,
                        json!({"key": "p", "value": "user prefers vim", "kind": "preference"}),
                    )
                    .await;
                store
                    .execute(
                        &ctx,
                        json!({"key": "e", "value": "user shipped a release", "kind": "event"}),
                    )
                    .await;
                let search = MemorySearchTool;
                let r = search
                    .execute(&ctx, json!({"query": "user", "kinds": ["preference"]}))
                    .await;
                assert!(!r.is_error());
                let body = r.text();
                assert!(body.contains("[preference]"), "{body}");
                assert!(!body.contains("[event]"), "{body}");
            })
            .await;
    }

    #[tokio::test]
    async fn forget_soft_deletes_then_search_excludes() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                let ctx = ctx().await;
                let store = MemoryRememberTool;
                store
                    .execute(
                        &ctx,
                        json!({"key": "k1", "value": "secret", "kind": "preference"}),
                    )
                    .await;
                let forget = MemoryForgetTool;
                let r = forget.execute(&ctx, json!({"key": "k1"})).await;
                assert!(!r.is_error(), "{}", r.text());

                let search = MemorySearchTool;
                let r = search.execute(&ctx, json!({"query": "secret"})).await;
                assert_eq!(r.text(), "(no matches)");
            })
            .await;
    }

    #[tokio::test]
    async fn forget_idempotent_on_unknown_key() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                let ctx = ctx().await;
                let forget = MemoryForgetTool;
                let r = forget.execute(&ctx, json!({"key": "never-existed"})).await;
                assert!(!r.is_error());
                assert!(r.text().contains("no active entry"));
            })
            .await;
    }

    #[tokio::test]
    async fn search_no_matches_returns_friendly_message() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                let ctx = ctx().await;
                let search = MemorySearchTool;
                let r = search
                    .execute(&ctx, json!({"query": "nothing matches this"}))
                    .await;
                assert!(!r.is_error());
                assert_eq!(r.text(), "(no matches)");
            })
            .await;
    }

    const CHAT_A: &str = "00000000-0000-4000-8000-00000000000a";
    const CHAT_B: &str = "00000000-0000-4000-8000-00000000000b";

    async fn chat_turn(
        ctx: &ToolCtx,
        scope: &str,
        chat: &str,
        role: conversations::Role,
        text: &str,
    ) -> String {
        conversations::chat_for(&ctx.agent_db, scope, Some(chat), text)
            .await
            .expect("chat");
        conversations::record_in_chat(&ctx.agent_db, chat, role, text)
            .await
            .expect("record")
            .expect("chat")
    }

    async fn search_as(ctx: &ToolCtx, scope: &str, chat: &str, input: Value) -> ToolResult {
        crate::tools::gateway::CONVERSATION
            .scope(
                scope.to_string(),
                crate::tools::gateway::CHAT
                    .scope(chat.to_string(), ConversationSearchTool.execute(ctx, input)),
            )
            .await
    }

    #[tokio::test]
    async fn conversation_search_finds_this_persons_earlier_chats_only() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                use conversations::Role;
                let ctx = ctx().await;
                let asked =
                    chat_turn(&ctx, "alice", CHAT_A, Role::User, "我们下周去京都旅行").await;
                chat_turn(
                    &ctx,
                    "alice",
                    CHAT_A,
                    Role::Assistant,
                    "好的，京都的红叶季很美",
                )
                .await;
                chat_turn(&ctx, "bob", CHAT_B, Role::User, "京都的预算是多少").await;

                let r = search_as(&ctx, "alice", "elsewhere", json!({"query": "京都"})).await;
                assert!(!r.is_error(), "{}", r.text());
                assert!(r.text().contains("下周去京都旅行"), "{}", r.text());
                assert!(r.text().contains("红叶季"), "{}", r.text());
                assert!(
                    !r.text().contains("预算"),
                    "another person's chat: {}",
                    r.text()
                );
                assert!(
                    r.text().contains(&format!("(turn {asked})")),
                    "{}",
                    r.text()
                );

                let r = search_as(&ctx, "alice", CHAT_A, json!({"turn": asked})).await;
                assert!(r.text().contains("(this chat)"), "{}", r.text());
                assert!(r.text().contains("> ["), "{}", r.text());
                assert!(r.text().contains("红叶季"), "{}", r.text());
                let r = search_as(&ctx, "bob", CHAT_B, json!({"turn": asked})).await;
                assert!(r.is_error(), "{}", r.text());

                let r = ConversationSearchTool
                    .execute(&ctx, json!({"query": "京都"}))
                    .await;
                assert!(r.is_error(), "outside a chat: {}", r.text());
            })
            .await;
    }

    #[tokio::test]
    async fn the_owner_s_search_in_a_group_keeps_to_that_group() {
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                use conversations::Role;
                let ctx = ctx().await;
                let private =
                    chat_turn(&ctx, "dm", CHAT_A, Role::User, "我的体检报告在抽屉里").await;
                conversations::note_owner_scope(&ctx.agent_db, "dm")
                    .await
                    .expect("note");
                conversations::note_owner_scope(&ctx.agent_db, "web")
                    .await
                    .expect("note");
                chat_turn(&ctx, "group", CHAT_B, Role::User, "体检报告什么时候出").await;

                // From another chat of theirs, the owner finds their own.
                let r = search_as(&ctx, "web", "elsewhere", json!({"query": "体检报告"})).await;
                assert!(r.text().contains("抽屉"), "{}", r.text());

                let in_group = |input: Value| {
                    super::super::gateway::IN_GROUP
                        .scope(true, search_as(&ctx, "group", CHAT_B, input))
                };
                let r = in_group(json!({"query": "体检报告"})).await;
                assert!(
                    r.text().contains("什么时候出"),
                    "the group's own: {}",
                    r.text()
                );
                assert!(
                    !r.text().contains("抽屉"),
                    "a private chat, in a group: {}",
                    r.text()
                );
                let r = in_group(json!({"turn": private})).await;
                assert!(r.is_error(), "{}", r.text());
            })
            .await;
    }

    #[tokio::test]
    async fn a_value_over_the_cap_is_cut_by_bytes() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                let ctx = ctx().await;
                let new = |value| NewEntry {
                    key: "k",
                    value,
                    kind: Kind::Preference,
                    source: Source::UserTold,
                    ttl_days: None,
                };
                // 1500 three-byte characters: 4500 bytes, over the cap.
                let long = "字".repeat(1500);
                let (_, cut) = save(false, &ctx, new(&long)).await.expect("save");
                assert!(cut);
                let row = memory::get(&ctx.agent_db, "k")
                    .await
                    .expect("get")
                    .expect("row");
                assert!(row.value.len() <= MAX_VALUE_BYTES, "{}", row.value.len());
                assert!(row.value.ends_with("size cap)"));
                // 1300 of them fit: kept whole, without a marker.
                let fits = "字".repeat(1300);
                let (_, cut) = save(true, &ctx, new(&fits)).await.expect("save");
                assert!(!cut);
                let row = memory::get(&ctx.agent_db, "k")
                    .await
                    .expect("get")
                    .expect("row");
                assert_eq!(row.value, fits);
                assert_eq!(row.ttl_days, None);
            })
            .await;
    }

    #[tokio::test]
    async fn a_turn_notes_its_net_change_to_each_key() {
        // Writes are the owner's turns' (issue #218).
        super::super::gateway::FROM_OWNER
            .scope(true, async {
                let ctx = ctx().await;
                let noted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let remember = |key: &'static str, value: &'static str| {
                    let ctx = &ctx;
                    async move {
                        MemoryRememberTool
                            .execute(
                                ctx,
                                json!({"key": key, "value": value, "kind": "preference"}),
                            )
                            .await
                    }
                };
                // Before the turn: an entry the turn repeats, and one it replaces.
                remember("user.name", "Ana").await;
                remember("pref.editor", "vim").await;
                let vim = memory::get(&ctx.agent_db, "pref.editor")
                    .await
                    .unwrap()
                    .unwrap()
                    .id;
                MEMORY_CHANGES
                    .scope(std::sync::Arc::clone(&noted), async {
                        remember("user.name", "Ana").await;
                        remember("pref.editor", "emacs").await;
                        remember("pref.editor", "helix").await;
                        remember("pref.font", "Iosevka").await;
                        MemoryForgetTool
                            .execute(&ctx, json!({"key": "pref.font"}))
                            .await;
                        remember("pref.theme", "dark").await;
                        MemoryForgetTool
                            .execute(&ctx, json!({"key": "pref.theme"}))
                            .await;
                        MemoryForgetTool
                            .execute(&ctx, json!({"key": "user.name"}))
                            .await;
                    })
                    .await;
                let shown = changes(std::mem::take(&mut *noted.lock().unwrap()));
                let by_key = |k: &str| shown.iter().find(|c| c.key == k).cloned();
                let editor = by_key("pref.editor").expect("editor");
                assert_eq!(editor.action, havn_proto::MemoryAction::Saved);
                assert_eq!(editor.value, "helix");
                assert_eq!(editor.before.as_deref(), Some(vim.as_str()));
                let name = by_key("user.name").expect("name");
                assert_eq!(name.action, havn_proto::MemoryAction::Forgot);
                assert_eq!(name.value, "Ana");
                assert!(name.after.is_none());
                assert!(
                    by_key("pref.font").is_none(),
                    "saved then forgotten: {shown:?}"
                );
                assert!(
                    by_key("pref.theme").is_none(),
                    "saved then forgotten: {shown:?}"
                );
                assert_eq!(shown.len(), 2, "{shown:?}");
                assert!(remember("..", "x").await.is_error());
            })
            .await;
    }

    #[test]
    fn an_excerpt_is_cut_around_the_first_match() {
        let text = format!("{}Needle here{}", "a".repeat(500), "b".repeat(500));
        let cut = excerpt(&text, &["needle"], 30);
        assert!(
            cut.starts_with('\u{2026}') && cut.ends_with('\u{2026}'),
            "{cut}"
        );
        assert!(cut.contains("Needle here"), "{cut}");
        assert_eq!(excerpt("short", &["x"], 30), "short");
    }
}
