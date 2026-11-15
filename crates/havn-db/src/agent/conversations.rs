//! Conversations repository — agent-local message log.
//!
//! Persists every turn of every channel in the agent's `agent.db`. Driven by
//! the runtime's inbound handler: user turn on receive, assistant turn after
//! the LLM response. `recent` retrieves the recency window for context build.
//! `search` runs the FTS5 mirror for the `memory_search` tool.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::{DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "system" => Self::System,
            "tool" => Self::Tool,
            other => {
                return Err(DbError::InvalidValue {
                    column: "conversations.role",
                    message: format!("unknown role {other:?}"),
                });
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub role: Role,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

pub async fn record(pool: &SqlitePool, channel_id: &str, role: Role, content: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO conversations (id, channel_id, role, content) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(channel_id)
    .bind(role.as_str())
    .bind(content)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a turn of chat `chat`, unless the chat was deleted meanwhile:
/// false then, and nothing is written.
pub async fn record_in_chat(
    pool: &SqlitePool,
    chat: &str,
    role: Role,
    content: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let head: Option<(Option<String>,)> = sqlx::query_as("SELECT head FROM chats WHERE id = ?1")
        .bind(chat)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((parent,)) = head else {
        return Ok(false);
    };
    let id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO conversations (id, channel_id, role, content, parent_id) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(&id)
    .bind(chat)
    .bind(role.as_str())
    .bind(content)
    .bind(parent)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE chats SET head = ?2 WHERE id = ?1")
        .bind(chat)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Where a message branches off its chat (issue #129).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rewind<'a> {
    /// A new version of user turn `.0`: it follows what that one followed.
    Edit(&'a str),
    /// Another reply in place of assistant turn `.0`, or of the last
    /// reply when `None`.
    Regenerate(Option<&'a str>),
}

/// Move chat `chat`'s head back for `rewind`, so the next turns branch
/// off there. `Some(true)` when the message's own user turn is to be
/// recorded (an edit), `Some(false)` when not (a regenerated reply);
/// `None` when the turn named is not one of the chat's that can be.
pub async fn rewind(pool: &SqlitePool, chat: &str, rewind: Rewind<'_>) -> Result<Option<bool>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let head: Option<(Option<String>,)> = sqlx::query_as("SELECT head FROM chats WHERE id = ?1")
        .bind(chat)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((head,)) = head else {
        return Ok(None);
    };
    let (turn, wanted, records_user) = match rewind {
        Rewind::Edit(turn) => (Some(turn.to_string()), Role::User, true),
        Rewind::Regenerate(Some(turn)) => (Some(turn.to_string()), Role::Assistant, false),
        Rewind::Regenerate(None) => (head, Role::Assistant, false),
    };
    let Some(turn) = turn else {
        return Ok(None);
    };
    let found: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT role, parent_id FROM conversations WHERE id = ?1 AND channel_id = ?2",
    )
    .bind(&turn)
    .bind(chat)
    .fetch_optional(&mut *tx)
    .await?;
    let new_head = match found {
        Some((role, parent)) if Role::parse(&role)? == wanted => parent,
        // A reply that never came: answer the user turn the branch ends with.
        Some((role, _))
            if rewind == Rewind::Regenerate(None) && Role::parse(&role)? == Role::User =>
        {
            Some(turn)
        }
        _ => return Ok(None),
    };
    sqlx::query("UPDATE chats SET head = ?2 WHERE id = ?1")
        .bind(chat)
        .bind(new_head)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(records_user))
}

/// The last `limit` turns of chat `chat`'s branch, oldest first: the path
/// to its head.
pub async fn branch(pool: &SqlitePool, chat: &str, limit: u32) -> Result<Vec<BranchTurn>> {
    let rows: Vec<BranchRow> = sqlx::query_as::<_, BranchRow>(
        "WITH RECURSIVE path(id, parent_id, role, content, created_at, depth) AS ( \
             SELECT c.id, c.parent_id, c.role, c.content, c.created_at, 1 \
             FROM conversations c JOIN chats h ON h.head = c.id \
             WHERE h.id = ?1 \
             UNION ALL \
             SELECT c.id, c.parent_id, c.role, c.content, c.created_at, p.depth + 1 \
             FROM conversations c JOIN path p ON c.id = p.parent_id \
             WHERE p.depth < ?2 \
         ) \
         SELECT id, parent_id, role, content, created_at FROM path ORDER BY depth DESC",
    )
    .bind(chat)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    // The versions of each turn: the children of its parent.
    let all: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, parent_id FROM conversations WHERE channel_id = ?1 \
         ORDER BY created_at, rowid",
    )
    .bind(chat)
    .fetch_all(pool)
    .await?;
    let siblings = |parent: &Option<String>| -> Vec<String> {
        all.iter()
            .filter(|(_, p)| p == parent)
            .map(|(id, _)| id.clone())
            .collect()
    };
    rows.into_iter()
        .map(|r| {
            Ok(BranchTurn {
                versions: siblings(&r.parent_id),
                id: r.id,
                role: Role::parse(&r.role)?,
                content: r.content,
                created_at: r.created_at,
            })
        })
        .collect()
}

/// Continue chat `chat` from `turn`'s branch: its head becomes the newest
/// turn under `turn`. False when `turn` is not one of the chat's.
pub async fn switch_branch(pool: &SqlitePool, chat: &str, turn: &str) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let found: Option<(String,)> =
        sqlx::query_as("SELECT id FROM conversations WHERE id = ?1 AND channel_id = ?2")
            .bind(turn)
            .bind(chat)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((mut head,)) = found else {
        return Ok(false);
    };
    loop {
        let child: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM conversations WHERE parent_id = ?1 \
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )
        .bind(&head)
        .fetch_optional(&mut *tx)
        .await?;
        match child {
            Some((id,)) => head = id,
            None => break,
        }
    }
    sqlx::query("UPDATE chats SET head = ?2 WHERE id = ?1")
        .bind(chat)
        .bind(&head)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// A turn of a chat's branch, with the versions it has: the turns that
/// follow the same one, itself included, oldest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchTurn {
    pub id: String,
    pub role: Role,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub versions: Vec<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct BranchRow {
    id: String,
    parent_id: Option<String>,
    role: String,
    content: String,
    created_at: DateTime<Utc>,
}

/// Fetch the most recent `limit` turns for `channel_id` in chronological order
/// (oldest first). Used to build the messages array for the next LLM call.
pub async fn recent(pool: &SqlitePool, channel_id: &str, limit: u32) -> Result<Vec<Turn>> {
    let rows: Vec<TurnRow> = sqlx::query_as::<_, TurnRow>(
        "SELECT role, content, created_at FROM conversations \
         WHERE channel_id = ?1 \
         ORDER BY created_at DESC, rowid DESC LIMIT ?2",
    )
    .bind(channel_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut turns: Vec<Turn> = rows
        .into_iter()
        .rev()
        .map(Turn::try_from)
        .collect::<Result<_>>()?;
    turns.shrink_to_fit();
    Ok(turns)
}

/// Full-text search over conversation content. `query` is a `SQLite` FTS5
/// MATCH expression (use `escape_fts_query` to escape free-form user input).
pub async fn search(pool: &SqlitePool, query: &str, limit: u32) -> Result<Vec<Turn>> {
    let rows: Vec<TurnRow> = sqlx::query_as::<_, TurnRow>(
        "SELECT c.role, c.content, c.created_at \
         FROM conversations_fts f JOIN conversations c ON c.rowid = f.rowid \
         WHERE conversations_fts MATCH ?1 \
         ORDER BY rank LIMIT ?2",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(Turn::try_from).collect()
}

/// One conversation of a scope (issue #126); its turns are those whose
/// `channel_id` is its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    /// New messages without a chosen chat continue this one.
    pub active: bool,
}

/// Longest chat title kept.
pub const MAX_TITLE_CHARS: usize = 80;

/// A chat title from `text`: its first line, at most [`MAX_TITLE_CHARS`].
#[must_use]
pub fn title_from(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default();
    line.trim().chars().take(MAX_TITLE_CHARS).collect()
}

/// The chat of `scope` a message continues: `requested` when it is a chat
/// of `scope`, or a new one by that id (a UUID the dashboard chose for a
/// new chat); otherwise the scope's active chat, or a new one. The chat
/// becomes the scope's active one, is titled after `text` if it has no
/// title, and moves to the top of the list.
pub async fn chat_for(
    pool: &SqlitePool,
    scope: &str,
    requested: Option<&str>,
    text: &str,
) -> Result<String> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut chosen: Option<String> = None;
    if let Some(id) = requested {
        let found: Option<(String,)> = sqlx::query_as("SELECT scope FROM chats WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        match found {
            Some((of,)) if of == scope => chosen = Some(id.to_string()),
            // Another scope's chat stays its own.
            Some(_) => {}
            None if Uuid::parse_str(id).is_ok() => {
                sqlx::query("INSERT INTO chats (id, scope) VALUES (?1, ?2)")
                    .bind(id)
                    .bind(scope)
                    .execute(&mut *tx)
                    .await?;
                chosen = Some(id.to_string());
            }
            None => {}
        }
    }
    let id = match chosen {
        Some(id) => id,
        None => {
            let active: Option<(String,)> =
                sqlx::query_as("SELECT chat_id FROM active_chats WHERE scope = ?1")
                    .bind(scope)
                    .fetch_optional(&mut *tx)
                    .await?;
            match active {
                Some((id,)) => id,
                None => {
                    let id = Uuid::now_v7().to_string();
                    sqlx::query("INSERT INTO chats (id, scope) VALUES (?1, ?2)")
                        .bind(&id)
                        .bind(scope)
                        .execute(&mut *tx)
                        .await?;
                    id
                }
            }
        }
    };
    sqlx::query(
        "UPDATE chats SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         title = CASE WHEN title = '' THEN ?2 ELSE title END WHERE id = ?1",
    )
    .bind(&id)
    .bind(title_from(text))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO active_chats (scope, chat_id) VALUES (?1, ?2) \
         ON CONFLICT (scope) DO UPDATE SET chat_id = excluded.chat_id",
    )
    .bind(scope)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// The chat `requested` when it is one of `scope`, else the scope's active
/// chat, if it has one. Creates nothing.
pub async fn existing_chat(
    pool: &SqlitePool,
    scope: &str,
    requested: Option<&str>,
) -> Result<Option<String>> {
    if let Some(id) = requested {
        let found: Option<(String,)> =
            sqlx::query_as("SELECT id FROM chats WHERE id = ?1 AND scope = ?2")
                .bind(id)
                .bind(scope)
                .fetch_optional(pool)
                .await?;
        return Ok(found.map(|(id,)| id));
    }
    let active: Option<(String,)> =
        sqlx::query_as("SELECT chat_id FROM active_chats WHERE scope = ?1")
            .bind(scope)
            .fetch_optional(pool)
            .await?;
    Ok(active.map(|(id,)| id))
}

/// End the active chat of `scope`: its next message starts a new one. The
/// old chat stays in the list.
pub async fn leave_active(pool: &SqlitePool, scope: &str) -> Result<()> {
    sqlx::query("DELETE FROM active_chats WHERE scope = ?1")
        .bind(scope)
        .execute(pool)
        .await?;
    Ok(())
}

/// The chats of `scope`, most recently used first.
pub async fn chats(pool: &SqlitePool, scope: &str, limit: u32) -> Result<Vec<Chat>> {
    Ok(sqlx::query_as::<_, Chat>(
        "SELECT c.id, c.title, c.created_at, c.updated_at, \
                (a.chat_id IS NOT NULL) AS active \
         FROM chats c LEFT JOIN active_chats a ON a.chat_id = c.id AND a.scope = c.scope \
         WHERE c.scope = ?1 \
         ORDER BY c.updated_at DESC, c.rowid DESC LIMIT ?2",
    )
    .bind(scope)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Rename chat `id` of `scope`; false when it is not one.
pub async fn rename_chat(pool: &SqlitePool, scope: &str, id: &str, title: &str) -> Result<bool> {
    let title: String = title.trim().chars().take(MAX_TITLE_CHARS).collect();
    let res = sqlx::query("UPDATE chats SET title = ?3 WHERE id = ?1 AND scope = ?2")
        .bind(id)
        .bind(scope)
        .bind(title)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Delete chat `id` of `scope` with its history; false when it is not one.
pub async fn delete_chat(pool: &SqlitePool, scope: &str, id: &str) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let res = sqlx::query("DELETE FROM chats WHERE id = ?1 AND scope = ?2")
        .bind(id)
        .bind(scope)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM active_chats WHERE chat_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM conversations WHERE channel_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Wrap a free-form user query so it can be passed to FTS5 MATCH safely.
///
/// Treats every whitespace-separated word as a literal phrase; quotes are
/// escaped. Tokens are joined with `OR` rather than the FTS5 default `AND`
/// so that retrieval returns rows where *any* term matches — the relevance
/// `rank` ordering is what surfaces the best hit. AND-style "all terms must
/// match" semantics are too strict for free-form prompts which often mix a
/// few keyword-like words with chatty filler.
pub fn escape_fts_query(input: &str) -> String {
    let phrases: Vec<String> = input
        .split_whitespace()
        .map(|tok| {
            let escaped = tok.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect();
    phrases.join(" OR ")
}

#[derive(Debug, sqlx::FromRow)]
struct TurnRow {
    role: String,
    content: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<TurnRow> for Turn {
    type Error = DbError;
    fn try_from(r: TurnRow) -> Result<Self> {
        Ok(Self {
            role: Role::parse(&r.role)?,
            content: r.content,
            created_at: r.created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::agent::connect_in_memory;

    #[tokio::test]
    async fn messages_continue_the_active_chat_until_another_is_chosen() {
        let pool = connect_in_memory().await.expect("connect");
        let first = chat_for(&pool, "scope", None, "\n  Plan the trip to Kyoto\nmore")
            .await
            .expect("chat");
        assert_eq!(
            chat_for(&pool, "scope", None, "and?").await.expect("chat"),
            first
        );
        let chosen = Uuid::now_v7().to_string();
        assert_eq!(
            chat_for(&pool, "scope", Some(&chosen), "a new topic")
                .await
                .expect("chat"),
            chosen
        );
        // The chosen one is active now, and both are listed, newest first.
        assert_eq!(
            chat_for(&pool, "scope", None, "x").await.expect("chat"),
            chosen
        );
        let listed = chats(&pool, "scope", 10).await.expect("list");
        let ids: Vec<&str> = listed.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [chosen.as_str(), first.as_str()]);
        assert_eq!(listed[1].title, "Plan the trip to Kyoto");
        assert_eq!((listed[0].active, listed[1].active), (true, false));
        // Back to the first by choosing it.
        assert_eq!(
            chat_for(&pool, "scope", Some(&first), "back")
                .await
                .expect("chat"),
            first
        );
    }

    #[tokio::test]
    async fn a_scope_cannot_use_another_scopes_chat() {
        let pool = connect_in_memory().await.expect("connect");
        let theirs = chat_for(&pool, "alice", None, "secret")
            .await
            .expect("chat");
        record(&pool, &theirs, Role::User, "secret")
            .await
            .expect("record");
        let mine = chat_for(&pool, "bob", Some(&theirs), "hi")
            .await
            .expect("chat");
        assert_ne!(mine, theirs);
        assert_eq!(
            existing_chat(&pool, "bob", Some(&theirs))
                .await
                .expect("db"),
            None
        );
        assert!(!rename_chat(&pool, "bob", &theirs, "x").await.expect("db"));
        assert!(!delete_chat(&pool, "bob", &theirs).await.expect("db"));
        assert_eq!(recent(&pool, &theirs, 10).await.expect("recent").len(), 1);
        // Ids that are not UUIDs do not make chats.
        let other = chat_for(&pool, "bob", Some("cron:job"), "hi")
            .await
            .expect("chat");
        assert_eq!(other, mine);
    }

    #[tokio::test]
    async fn leaving_and_deleting_chats() {
        let pool = connect_in_memory().await.expect("connect");
        let old = chat_for(&pool, "s", None, "old").await.expect("chat");
        record(&pool, &old, Role::User, "old")
            .await
            .expect("record");
        leave_active(&pool, "s").await.expect("leave");
        assert_eq!(existing_chat(&pool, "s", None).await.expect("db"), None);
        let new = chat_for(&pool, "s", None, "new").await.expect("chat");
        assert_ne!(new, old);
        assert!(
            rename_chat(&pool, "s", &old, "  Renamed  ")
                .await
                .expect("db")
        );
        assert!(delete_chat(&pool, "s", &new).await.expect("db"));
        assert!(
            !record_in_chat(&pool, &new, Role::Assistant, "late reply")
                .await
                .expect("db"),
            "a deleted chat gets no more turns"
        );
        assert!(recent(&pool, &new, 10).await.expect("recent").is_empty());
        assert!(
            record_in_chat(&pool, &old, Role::Assistant, "ok")
                .await
                .expect("db")
        );
        assert_eq!(recent(&pool, &old, 10).await.expect("recent").len(), 2);

        let listed = chats(&pool, "s", 10).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "Renamed");
        // Deleting the active chat leaves the scope without one.
        assert_eq!(existing_chat(&pool, "s", None).await.expect("db"), None);
    }

    async fn said(pool: &SqlitePool, chat: &str) -> Vec<String> {
        branch(pool, chat, 100)
            .await
            .expect("branch")
            .into_iter()
            .map(|t| t.content)
            .collect()
    }

    #[tokio::test]
    async fn edits_and_regenerated_replies_branch_off() {
        let pool = connect_in_memory().await.expect("connect");
        let chat = chat_for(&pool, "s", None, "q1").await.expect("chat");
        for (role, text) in [
            (Role::User, "q1"),
            (Role::Assistant, "a1"),
            (Role::User, "q2"),
            (Role::Assistant, "a2"),
        ] {
            assert!(
                record_in_chat(&pool, &chat, role, text)
                    .await
                    .expect("record")
            );
        }
        let turns = branch(&pool, &chat, 100).await.expect("branch");
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2", "a2"]);
        let (q2, a2) = (turns[2].id.clone(), turns[3].id.clone());

        // Regenerate the last reply: a sibling of a2.
        assert_eq!(
            rewind(&pool, &chat, Rewind::Regenerate(None))
                .await
                .expect("db"),
            Some(false)
        );
        record_in_chat(&pool, &chat, Role::Assistant, "a2'")
            .await
            .expect("record");
        let turns = branch(&pool, &chat, 100).await.expect("branch");
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2", "a2'"]);
        assert_eq!(turns[3].versions.len(), 2);
        assert_eq!(turns[3].versions[0], a2);

        // Edit q2: a new branch after a1.
        assert_eq!(
            rewind(&pool, &chat, Rewind::Edit(&q2)).await.expect("db"),
            Some(true)
        );
        record_in_chat(&pool, &chat, Role::User, "q2 edited")
            .await
            .expect("record");
        record_in_chat(&pool, &chat, Role::Assistant, "a3")
            .await
            .expect("record");
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2 edited", "a3"]);
        assert_eq!(
            branch(&pool, &chat, 2).await.expect("branch").len(),
            2,
            "limited"
        );

        // Back to the first version of q2: its newest reply.
        assert!(switch_branch(&pool, &chat, &q2).await.expect("db"));
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2", "a2'"]);

        // Turns of another chat, or of the wrong role, cannot be used.
        let other = chat_for(&pool, "t", None, "x").await.expect("chat");
        assert_eq!(
            rewind(&pool, &other, Rewind::Edit(&q2)).await.expect("db"),
            None
        );
        assert_eq!(
            rewind(&pool, &chat, Rewind::Edit(&a2)).await.expect("db"),
            None
        );
        assert!(!switch_branch(&pool, &other, &q2).await.expect("db"));
    }

    #[tokio::test]
    async fn editing_the_first_message_starts_a_new_root() {
        let pool = connect_in_memory().await.expect("connect");
        let chat = chat_for(&pool, "s", None, "hi").await.expect("chat");
        record_in_chat(&pool, &chat, Role::User, "hi")
            .await
            .expect("record");
        record_in_chat(&pool, &chat, Role::Assistant, "hello")
            .await
            .expect("record");
        let first = branch(&pool, &chat, 10).await.expect("branch")[0]
            .id
            .clone();
        assert_eq!(
            rewind(&pool, &chat, Rewind::Edit(&first))
                .await
                .expect("db"),
            Some(true)
        );
        assert!(said(&pool, &chat).await.is_empty());
        record_in_chat(&pool, &chat, Role::User, "hey")
            .await
            .expect("record");
        let turns = branch(&pool, &chat, 10).await.expect("branch");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].versions.len(), 2);
        // A regenerate for a user turn with no reply answers it.
        assert_eq!(
            rewind(&pool, &chat, Rewind::Regenerate(None))
                .await
                .expect("db"),
            Some(false)
        );
        assert_eq!(said(&pool, &chat).await, ["hey"]);
    }

    #[tokio::test]
    async fn record_and_recent_chronological() {
        let pool = connect_in_memory().await.expect("connect");
        for (role, content) in [
            (Role::User, "hi"),
            (Role::Assistant, "hello"),
            (Role::User, "what's 2+2?"),
            (Role::Assistant, "4"),
        ] {
            record(&pool, "ch1", role, content).await.expect("record");
        }
        let recent_turns = recent(&pool, "ch1", 10).await.expect("recent");
        let contents: Vec<&str> = recent_turns.iter().map(|t| t.content.as_str()).collect();
        assert_eq!(contents, vec!["hi", "hello", "what's 2+2?", "4"]);
    }

    #[tokio::test]
    async fn recent_respects_channel_isolation() {
        let pool = connect_in_memory().await.expect("connect");
        record(&pool, "alpha", Role::User, "alpha-1")
            .await
            .expect("rec");
        record(&pool, "beta", Role::User, "beta-1")
            .await
            .expect("rec");
        let alpha = recent(&pool, "alpha", 10).await.expect("recent");
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].content, "alpha-1");
    }

    #[tokio::test]
    async fn fts_search_finds_relevant_turn() {
        let pool = connect_in_memory().await.expect("connect");
        record(&pool, "c", Role::User, "the quick brown fox")
            .await
            .expect("rec");
        record(&pool, "c", Role::User, "lazy dog jumps")
            .await
            .expect("rec");
        let q = escape_fts_query("brown fox");
        let hits = search(&pool, &q, 5).await.expect("search");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("brown fox"));
    }

    #[test]
    fn escape_fts_query_handles_quotes() {
        assert_eq!(
            escape_fts_query("hello \"world\""),
            "\"hello\" OR \"\"\"world\"\"\""
        );
    }

    #[test]
    fn escape_fts_query_single_token_has_no_or() {
        assert_eq!(escape_fts_query("only"), "\"only\"");
    }
}
