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
) -> Result<Option<String>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let head: Option<(Option<String>,)> = sqlx::query_as("SELECT head FROM chats WHERE id = ?1")
        .bind(chat)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((parent,)) = head else {
        return Ok(None);
    };
    let id = insert_turn(&mut tx, chat, role, content, parent.as_deref()).await?;
    set_head(&mut tx, chat, Some(&id)).await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// Record `reply` to turn `parent` of chat `chat`. The chat continues from
/// it when its head is still `parent`; when the user moved to another
/// branch meanwhile, the reply stays on its own. `None` when the chat was
/// deleted.
pub async fn record_reply(
    pool: &SqlitePool,
    chat: &str,
    parent: &str,
    reply: &str,
) -> Result<Option<String>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let head: Option<(Option<String>,)> = sqlx::query_as("SELECT head FROM chats WHERE id = ?1")
        .bind(chat)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((head,)) = head else {
        return Ok(None);
    };
    let id = insert_turn(&mut tx, chat, Role::Assistant, reply, Some(parent)).await?;
    if head.as_deref() == Some(parent) {
        set_head(&mut tx, chat, Some(&id)).await?;
    }
    tx.commit().await?;
    Ok(Some(id))
}

async fn insert_turn(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    chat: &str,
    role: Role,
    content: &str,
    parent: Option<&str>,
) -> Result<String> {
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
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

async fn set_head(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    chat: &str,
    head: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE chats SET head = ?2 WHERE id = ?1")
        .bind(chat)
        .bind(head)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A turn's role and parent, when it is one of chat `chat`'s.
async fn turn_of(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    chat: &str,
    turn: &str,
) -> Result<Option<(Role, Option<String>)>> {
    let found: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT role, parent_id FROM conversations WHERE id = ?1 AND channel_id = ?2",
    )
    .bind(turn)
    .bind(chat)
    .fetch_optional(&mut **tx)
    .await?;
    found
        .map(|(role, parent)| Ok((Role::parse(&role)?, parent)))
        .transpose()
}

/// Where a message branches off its chat (issue #129).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rewind<'a> {
    /// `text` as a new version of user turn `turn`: it follows what that
    /// one followed.
    Edit { turn: &'a str, text: &'a str },
    /// Another reply to the user turn before reply `.0`, or before the
    /// chat's head when `None`: a new version of the reply that followed
    /// that user turn.
    Regenerate(Option<&'a str>),
}

/// Branch chat `chat` off for `rewind`, in one step: its head becomes the
/// user turn to answer (the new version, for an edit). That turn's id, or
/// `None`, and nothing changed, when the turn named is not one of the
/// chat's that can be, or no user turn precedes it.
pub async fn rewind(pool: &SqlitePool, chat: &str, rewind: Rewind<'_>) -> Result<Option<String>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let head: Option<(Option<String>,)> = sqlx::query_as("SELECT head FROM chats WHERE id = ?1")
        .bind(chat)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((head,)) = head else {
        return Ok(None);
    };
    let answer = match rewind {
        Rewind::Edit { turn, text } => match turn_of(&mut tx, chat, turn).await? {
            Some((Role::User, parent)) => {
                Some(insert_turn(&mut tx, chat, Role::User, text, parent.as_deref()).await?)
            }
            _ => None,
        },
        Rewind::Regenerate(start) => {
            // The nearest user turn at or before the reply named (or the
            // head): a scheduled task's result in between is passed over.
            let mut at = match start {
                Some(turn) => match turn_of(&mut tx, chat, turn).await? {
                    Some((Role::Assistant, parent)) => parent,
                    _ => None,
                },
                None => head,
            };
            let mut user = None;
            while let Some(turn) = at {
                match turn_of(&mut tx, chat, &turn).await? {
                    Some((Role::User, _)) => {
                        user = Some(turn);
                        break;
                    }
                    Some((_, parent)) => at = parent,
                    None => break,
                }
            }
            user
        }
    };
    let Some(answer) = answer else {
        return Ok(None);
    };
    set_head(&mut tx, chat, Some(&answer)).await?;
    tx.commit().await?;
    Ok(Some(answer))
}

/// The last `limit` turns of chat `chat`'s branch, oldest first: the path
/// to its head. Their `versions` are left empty; see [`with_versions`].
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
    rows.into_iter()
        .map(|r| {
            Ok(BranchTurn {
                id: r.id,
                parent_id: r.parent_id,
                role: Role::parse(&r.role)?,
                content: r.content,
                created_at: r.created_at,
                versions: Vec::new(),
            })
        })
        .collect()
}

/// `turns` of chat `chat` with their versions: the turns that follow the
/// same one, oldest first.
pub async fn with_versions(
    pool: &SqlitePool,
    chat: &str,
    mut turns: Vec<BranchTurn>,
) -> Result<Vec<BranchTurn>> {
    let mut parents: Vec<&str> = turns
        .iter()
        .filter_map(|t| t.parent_id.as_deref())
        .collect();
    parents.sort_unstable();
    parents.dedup();
    let marks = vec!["?"; parents.len()].join(", ");
    let sql = format!(
        "SELECT id, parent_id FROM conversations WHERE channel_id = ? \
         AND (parent_id IS NULL OR parent_id IN ({marks})) ORDER BY created_at, rowid"
    );
    let mut query = sqlx::query_as::<_, (String, Option<String>)>(&sql).bind(chat);
    for parent in &parents {
        query = query.bind(*parent);
    }
    let children = query.fetch_all(pool).await?;
    for turn in &mut turns {
        turn.versions = children
            .iter()
            .filter(|(_, p)| *p == turn.parent_id)
            .map(|(id, _)| id.clone())
            .collect();
    }
    Ok(turns)
}

/// Continue chat `chat` from `turn`'s branch: its head becomes the newest
/// turn under `turn`. False when `turn` is not one of the chat's.
pub async fn switch_branch(pool: &SqlitePool, chat: &str, turn: &str) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if turn_of(&mut tx, chat, turn).await?.is_none() {
        return Ok(false);
    }
    let mut head = turn.to_string();
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
    set_head(&mut tx, chat, Some(&head)).await?;
    tx.commit().await?;
    Ok(true)
}

/// A turn of a chat's branch, with the versions it has: the turns that
/// follow the same one, itself included, oldest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchTurn {
    pub id: String,
    pub parent_id: Option<String>,
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
            record_in_chat(&pool, &new, Role::Assistant, "late reply")
                .await
                .expect("db")
                .is_none(),
            "a deleted chat gets no more turns"
        );
        assert!(recent(&pool, &new, 10).await.expect("recent").is_empty());
        assert!(
            record_in_chat(&pool, &old, Role::Assistant, "ok")
                .await
                .expect("db")
                .is_some()
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

    async fn versions(pool: &SqlitePool, chat: &str) -> Vec<usize> {
        let turns = branch(pool, chat, 100).await.expect("branch");
        with_versions(pool, chat, turns)
            .await
            .expect("versions")
            .iter()
            .map(|t| t.versions.len())
            .collect()
    }

    #[tokio::test]
    async fn edits_and_regenerated_replies_branch_off() {
        let pool = connect_in_memory().await.expect("connect");
        let chat = chat_for(&pool, "s", None, "q1").await.expect("chat");
        let mut ids = Vec::new();
        for (role, text) in [
            (Role::User, "q1"),
            (Role::Assistant, "a1"),
            (Role::User, "q2"),
            (Role::Assistant, "a2"),
        ] {
            ids.push(
                record_in_chat(&pool, &chat, role, text)
                    .await
                    .expect("record")
                    .expect("chat"),
            );
        }
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2", "a2"]);
        let (q2, a2) = (ids[2].clone(), ids[3].clone());

        // Regenerate the last reply: answered again from q2, a sibling of a2.
        let to = rewind(&pool, &chat, Rewind::Regenerate(None))
            .await
            .expect("db");
        assert_eq!(to.as_deref(), Some(q2.as_str()));
        record_reply(&pool, &chat, &q2, "a2'")
            .await
            .expect("record");
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2", "a2'"]);
        assert_eq!(versions(&pool, &chat).await, [1, 1, 1, 2]);

        // Edit q2: a new version after a1, recorded with the rewind.
        let edited = rewind(
            &pool,
            &chat,
            Rewind::Edit {
                turn: &q2,
                text: "q2 edited",
            },
        )
        .await
        .expect("db")
        .expect("edited");
        record_reply(&pool, &chat, &edited, "a3")
            .await
            .expect("record");
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2 edited", "a3"]);
        assert_eq!(versions(&pool, &chat).await, [1, 1, 2, 1]);
        assert_eq!(
            branch(&pool, &chat, 2).await.expect("branch").len(),
            2,
            "limited"
        );

        // Back to the first version of q2: its newest reply.
        assert!(switch_branch(&pool, &chat, &q2).await.expect("db"));
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2", "a2'"]);

        // A reply finishing after the user moved elsewhere stays on its own.
        record_reply(&pool, &chat, &edited, "late")
            .await
            .expect("record");
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2", "a2'"]);

        // Turns of another chat, or of the wrong role, change nothing.
        let other = chat_for(&pool, "t", None, "x").await.expect("chat");
        assert_eq!(
            rewind(
                &pool,
                &other,
                Rewind::Edit {
                    turn: &q2,
                    text: "x"
                }
            )
            .await
            .expect("db"),
            None
        );
        assert_eq!(
            rewind(
                &pool,
                &chat,
                Rewind::Edit {
                    turn: &a2,
                    text: "x"
                }
            )
            .await
            .expect("db"),
            None
        );
        assert_eq!(
            rewind(&pool, &chat, Rewind::Regenerate(Some(&q2)))
                .await
                .expect("db"),
            None
        );
        assert!(!switch_branch(&pool, &other, &q2).await.expect("db"));
        assert_eq!(said(&pool, &chat).await, ["q1", "a1", "q2", "a2'"]);
    }

    #[tokio::test]
    async fn regenerating_passes_over_task_results() {
        let pool = connect_in_memory().await.expect("connect");
        let chat = chat_for(&pool, "s", None, "q").await.expect("chat");
        let q = record_in_chat(&pool, &chat, Role::User, "q")
            .await
            .expect("db")
            .expect("chat");
        let a = record_reply(&pool, &chat, &q, "a")
            .await
            .expect("db")
            .expect("chat");
        // A scheduled task's result after the reply.
        record_in_chat(&pool, &chat, Role::Assistant, "task result")
            .await
            .expect("db");
        let to = rewind(&pool, &chat, Rewind::Regenerate(None))
            .await
            .expect("db");
        assert_eq!(
            to.as_deref(),
            Some(q.as_str()),
            "the last user message is answered again"
        );
        let to = rewind(&pool, &chat, Rewind::Regenerate(Some(&a)))
            .await
            .expect("db");
        assert_eq!(to.as_deref(), Some(q.as_str()));
        // A chat with no user turn has nothing to answer, and stays as it is.
        let empty = chat_for(&pool, "t", None, "x").await.expect("chat");
        record_in_chat(&pool, &empty, Role::Assistant, "only a result")
            .await
            .expect("db");
        assert_eq!(
            rewind(&pool, &empty, Rewind::Regenerate(None))
                .await
                .expect("db"),
            None
        );
        assert_eq!(said(&pool, &empty).await, ["only a result"]);
    }

    #[tokio::test]
    async fn editing_the_first_message_starts_a_new_root() {
        let pool = connect_in_memory().await.expect("connect");
        let chat = chat_for(&pool, "s", None, "hi").await.expect("chat");
        let first = record_in_chat(&pool, &chat, Role::User, "hi")
            .await
            .expect("db")
            .expect("chat");
        record_reply(&pool, &chat, &first, "hello")
            .await
            .expect("db");
        let hey = rewind(
            &pool,
            &chat,
            Rewind::Edit {
                turn: &first,
                text: "hey",
            },
        )
        .await
        .expect("db")
        .expect("edited");
        assert_eq!(said(&pool, &chat).await, ["hey"]);
        assert_eq!(versions(&pool, &chat).await, [2]);
        // A user turn with no reply yet is answered.
        let to = rewind(&pool, &chat, Rewind::Regenerate(None))
            .await
            .expect("db");
        assert_eq!(to, Some(hey));
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
