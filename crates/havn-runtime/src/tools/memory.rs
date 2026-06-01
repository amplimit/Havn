//! Typed memory tools (spec §9.4): `memory_remember`, `memory_search`,
//! `memory_forget`. Backed by the agent-side SQLite `memory` table with
//! `kind` + `source` + `ttl_days` + `archived_at`.
//!
//! From the user's seat: the agent saves facts about you in four
//! categories so the dashboard can show them grouped, and so the daily
//! aging pass knows which facts to expire (events / projects) versus
//! which to keep forever (identity / preferences). Old "memory_store"
//! wrote untyped key/value blobs; this surface replaces it.

use std::fmt::Write as _;

use async_trait::async_trait;
use havn_db::agent::conversations::{self, escape_fts_query};
use havn_db::agent::memory::{self, Kind, NewEntry, Source};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolResult};

const SEARCH_LIMIT: u32 = 10;

/// Cap on `value` length per row. 4 KB is generous for a single fact and
/// keeps any one row from dominating the FTS5 token budget.
const MAX_VALUE_BYTES: usize = 4096;

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

        // Truncate-with-marker rather than reject. Audit feedback: hard
        // rejection of a 4097-byte value is a worse UX than silently
        // capping with a sentinel — the agent can still write the
        // important prefix and the LLM will see in the result that it
        // got truncated.
        let (stored_value, truncated) = if value.len() > MAX_VALUE_BYTES {
            let mut head: String = value
                .chars()
                .take(MAX_VALUE_BYTES.saturating_sub(64))
                .collect();
            head.push_str("\n…(truncated to fit memory entry size cap)");
            (head, true)
        } else {
            (value.to_string(), false)
        };

        let row_id = match memory::remember(
            &ctx.agent_db,
            NewEntry {
                key,
                value: &stored_value,
                kind,
                source,
                ttl_days,
            },
        )
        .await
        {
            Ok(id) => id,
            Err(e) => return ToolResult::Error(format!("memory_remember failed: {e}")),
        };

        // Hybrid retrieval (spec §9.4 v0.7): if an embedder is wired,
        // compute the vector for the value and persist it pinned to
        // the id we just got back. By-id (not by-key) avoids the
        // race where a concurrent remember(same_key, different_value)
        // would archive our row + insert a new one between here and
        // the UPDATE — audit-fix from the v0.7 review.
        //
        // We embed `"<key>: <value>"` so the key contributes to the
        // semantic signature — "user.editor" + "vim" together is more
        // distinguishable than just "vim".
        //
        // Failure here is non-fatal: the row is already persisted,
        // so worst case the row gets BM25 only. Logged at warn level
        // so operators see persistent provider issues. The startup
        // backfill (`embedding::backfill`) heals any rows that miss
        // the live embed path.
        if let Some(emb) = &ctx.embedder {
            if !row_id.is_empty() {
                let embed_text = format!("{key}: {stored_value}");
                match emb.embed(&embed_text).await {
                    Ok(vec) => {
                        if let Err(e) =
                            memory::set_embedding_by_id(&ctx.agent_db, &row_id, &vec).await
                        {
                            tracing::warn!(key, error = %e, "set_embedding_by_id failed; row stored without vector");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(key, error = %e, "embedder failed; row stored without vector");
                    }
                }
            }
        }

        let suffix = if truncated {
            " (value was truncated to the memory entry size cap)"
        } else {
            ""
        };
        ToolResult::Ok(format!("remembered {key:?} as {}{suffix}", kind.as_str()))
    }
}

pub struct MemorySearchTool;

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }

    fn description(&self) -> &'static str {
        "Full-text search across active memory entries AND prior \
         conversation turns. Returns up to 10 hits ranked by relevance. \
         Pass `kinds: [...]` to restrict to specific memory categories \
         (e.g. only 'event' for 'what happened recently')."
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
        let q = escape_fts_query(raw_query);
        let conv_hits = match conversations::search(&ctx.agent_db, &q, SEARCH_LIMIT).await {
            Ok(hits) => hits,
            Err(e) => return ToolResult::Error(format!("conversation search failed: {e}")),
        };

        if memory_hits.is_empty() && conv_hits.is_empty() {
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
        if !conv_hits.is_empty() {
            out.push_str("# Conversation history\n\n");
            for turn in conv_hits {
                writeln!(
                    out,
                    "- [{}] {}: {}",
                    turn.created_at.format("%Y-%m-%d %H:%M"),
                    turn.role.as_str(),
                    truncate(&turn.content, 240),
                )
                .ok();
            }
        }
        ToolResult::Ok(out)
    }
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
        let http = reqwest::Client::new();
        let workspace = std::env::temp_dir().join(format!("havn-tools-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&workspace).await.expect("ws");
        ToolCtx {
            workspace_dir: workspace,
            agent_db: pool,
            http,
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
}
