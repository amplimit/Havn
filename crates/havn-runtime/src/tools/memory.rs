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

const SEARCH_LIMIT: u32 = 10;

/// Cap on `value` length per row. 4 KB is generous for a single fact and
/// keeps any one row from dominating the FTS5 token budget.
const MAX_VALUE_BYTES: usize = havn_proto::MAX_MEMORY_VALUE_BYTES;

pub struct MemoryRememberTool;

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

        if key.is_empty() {
            return ToolResult::Error("key must be non-empty".into());
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
         including turns no longer in the conversation you see. Every word \
         of `query` must appear in a turn; words of three or more \
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
        if let Some(turn) = input.get("turn").and_then(Value::as_str) {
            return match conversations::around(&ctx.agent_db, &scope, turn, CONTEXT_TURNS).await {
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
            match conversations::search_chats(&ctx.agent_db, &scope, query, SEARCH_LIMIT).await {
                Ok(hits) => hits,
                Err(e) => return ToolResult::Error(format!("conversation_search failed: {e}")),
            };
        if hits.is_empty() {
            return ToolResult::Ok("(no matches)".into());
        }
        let mut out = String::new();
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
fn excerpt(content: &str, terms: &[&str], max: usize) -> String {
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
        let Some(key) = input.get("key").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: key".into());
        };
        match memory::forget(&ctx.agent_db, key).await {
            Ok(true) => ToolResult::Ok(format!("forgot {key:?}")),
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
    }

    #[tokio::test]
    async fn remember_rejects_missing_kind() {
        let ctx = ctx().await;
        let store = MemoryRememberTool;
        let r = store.execute(&ctx, json!({"key": "k", "value": "v"})).await;
        assert!(r.is_error(), "{}", r.text());
        assert!(r.text().contains("kind"));
    }

    #[tokio::test]
    async fn remember_truncates_oversize_value_with_marker() {
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
    }

    #[tokio::test]
    async fn search_filters_by_kind() {
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
    }

    #[tokio::test]
    async fn forget_soft_deletes_then_search_excludes() {
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
    }

    #[tokio::test]
    async fn forget_idempotent_on_unknown_key() {
        let ctx = ctx().await;
        let forget = MemoryForgetTool;
        let r = forget.execute(&ctx, json!({"key": "never-existed"})).await;
        assert!(!r.is_error());
        assert!(r.text().contains("no active entry"));
    }

    #[tokio::test]
    async fn search_no_matches_returns_friendly_message() {
        let ctx = ctx().await;
        let search = MemorySearchTool;
        let r = search
            .execute(&ctx, json!({"query": "nothing matches this"}))
            .await;
        assert!(!r.is_error());
        assert_eq!(r.text(), "(no matches)");
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
        use conversations::Role;
        let ctx = ctx().await;
        let asked = chat_turn(&ctx, "alice", CHAT_A, Role::User, "我们下周去京都旅行").await;
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
    }

    #[tokio::test]
    async fn a_value_over_the_cap_is_cut_by_bytes() {
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
