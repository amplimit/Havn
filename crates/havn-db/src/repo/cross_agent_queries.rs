//! `cross_agent_queries` — audit row per cross-agent `AgentQuery`
//! (spec §4.4 v0.7). One INSERT after each query finishes, regardless of
//! outcome. The per-LLM-call usage is already covered by
//! `credential_usages` (with `LlmRequest.billing_user_id` attributing
//! token spend to the caller's owner).

use havn_core::{AgentId, UserId};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::Result;

/// Hard cap on the prompt excerpt we persist. The full prompt lives in
/// proxy logs; this column is only there to identify *which* query a row
/// describes. 4 KiB matches the cap on cron job prompts.
const PROMPT_EXCERPT_MAX: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Ok,
    Error,
    Timeout,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewQuery<'a> {
    pub caller_agent_id: AgentId,
    pub target_agent_id: AgentId,
    pub caller_user_id: UserId,
    /// Caller-supplied prompt. Truncated to [`PROMPT_EXCERPT_MAX`] before
    /// the INSERT so we never persist arbitrary-size payloads.
    pub prompt: &'a str,
    pub outcome: Outcome,
    pub error_message: Option<&'a str>,
    pub include_transcript: bool,
    pub started_at_rfc3339: &'a str,
    pub finished_at_rfc3339: &'a str,
}

pub async fn record(pool: &SqlitePool, q: NewQuery<'_>) -> Result<()> {
    let excerpt = if q.prompt.len() > PROMPT_EXCERPT_MAX {
        // Slice on a UTF-8 char boundary by walking back from the cap.
        // `prompt.is_char_boundary(PROMPT_EXCERPT_MAX)` may be false; back
        // up to the nearest one. Worst case we drop a few extra bytes —
        // fine for an audit excerpt.
        let mut end = PROMPT_EXCERPT_MAX;
        while end > 0 && !q.prompt.is_char_boundary(end) {
            end -= 1;
        }
        &q.prompt[..end]
    } else {
        q.prompt
    };

    sqlx::query(
        "INSERT INTO cross_agent_queries \
         (id, caller_agent_id, target_agent_id, caller_user_id, prompt_excerpt, \
          outcome, error_message, include_transcript, started_at, finished_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(q.caller_agent_id.to_string())
    .bind(q.target_agent_id.to_string())
    .bind(q.caller_user_id.to_string())
    .bind(excerpt)
    .bind(q.outcome.as_str())
    .bind(q.error_message)
    .bind(i64::from(q.include_transcript))
    .bind(q.started_at_rfc3339)
    .bind(q.finished_at_rfc3339)
    .execute(pool)
    .await?;
    Ok(())
}
