//! Credential usage records — every gateway-side LLM call writes one row here.
//!
//! Drives:
//! - per-user / per-credential / per-agent usage rollups for the dashboard
//! - daily-token budget enforcement: a call first reserves its worst-case
//!   tokens with [`reserve`], then [`settle`] records the actual usage or
//!   [`release`] drops the reservation
//!
//! v0.6 dropped the `estimated_usd` column (spec §7.3): havn doesn't
//! maintain a model pricing table. Operators that need $ visibility
//! compute it from token counts in their own analytics pipeline.

use havn_core::{AgentId, CredentialId, UserId};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::Result;

/// Daily token and per-minute request caps one LLM call must fit in.
/// `0` disables a cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    /// `limits.max_tokens_per_day` of the credential.
    pub credential_tokens: i64,
    /// `limits.per_user.max_tokens_per_day` of the credential.
    pub user_tokens: i64,
    /// `limits.per_user.max_requests_per_minute` of the credential.
    pub user_requests_per_minute: i64,
    /// Policy `budget.max_tokens_per_day` of the calling agent, across
    /// all credentials.
    pub agent_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct Reservation<'a> {
    pub credential_id: CredentialId,
    pub user_id: UserId,
    pub agent_id: Option<AgentId>,
    pub provider: &'a str,
    pub model: &'a str,
    /// Upper estimate of the request's input tokens.
    pub tokens_in: i64,
    /// The request's `max_tokens`. The reservation grants less when that
    /// is all the caps leave, but never less than `min_tokens_out`.
    pub max_tokens_out: i64,
    pub min_tokens_out: i64,
    pub caps: Caps,
    /// RFC 3339 starts of the daily and per-minute windows, in the
    /// `created_at` format so they compare lexicographically.
    pub day_start: &'a str,
    pub minute_start: &'a str,
}

/// A usage row holding a request's worst-case tokens until
/// [`settle`] replaces them with the actual usage or [`release`] drops it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserved {
    pub id: String,
    /// Output tokens granted: the request's `max_tokens` must not exceed it.
    pub tokens_out: i64,
}

/// Reserve `tokens_in` plus up to `max_tokens_out` against every cap in
/// `caps`, or return `None` when they leave less than `min_tokens_out`
/// (or the per-minute request cap is reached).
///
/// One `INSERT ... SELECT` statement: SQLite takes the write lock before
/// the statement reads the current totals, so concurrent reservations
/// are serialized and cannot overspend a cap together.
pub async fn reserve(pool: &SqlitePool, r: Reservation<'_>) -> Result<Option<Reserved>> {
    let id = Uuid::now_v7().to_string();
    let tokens_out: Option<i64> = sqlx::query_scalar(
        "WITH used AS ( \
           SELECT \
             (SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM credential_usages \
               WHERE credential_id = ?2 AND created_at >= ?10) AS credential_tokens, \
             (SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM credential_usages \
               WHERE credential_id = ?2 AND user_id = ?3 AND created_at >= ?10) AS user_tokens, \
             (SELECT COUNT(*) FROM credential_usages \
               WHERE credential_id = ?2 AND user_id = ?3 AND created_at >= ?11) AS user_requests, \
             (SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM credential_usages \
               WHERE agent_id = ?4 AND created_at >= ?10) AS agent_tokens \
         ), allowance AS ( \
           SELECT MIN(?8, \
                      CASE WHEN ?12 > 0 THEN ?12 - credential_tokens - ?7 ELSE ?8 END, \
                      CASE WHEN ?13 > 0 THEN ?13 - user_tokens - ?7 ELSE ?8 END, \
                      CASE WHEN ?15 > 0 THEN ?15 - agent_tokens - ?7 ELSE ?8 END) AS tokens_out, \
                  user_requests \
           FROM used \
         ) \
         INSERT INTO credential_usages \
           (id, credential_id, user_id, agent_id, provider, model, tokens_in, tokens_out) \
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, tokens_out FROM allowance \
         WHERE tokens_out >= ?9 AND (?14 <= 0 OR user_requests < ?14) \
         RETURNING tokens_out",
    )
    .bind(&id)
    .bind(r.credential_id.to_string())
    .bind(r.user_id.to_string())
    .bind(r.agent_id.map(|a| a.to_string()))
    .bind(r.provider)
    .bind(r.model)
    .bind(r.tokens_in)
    .bind(r.max_tokens_out)
    .bind(r.min_tokens_out)
    .bind(r.day_start)
    .bind(r.minute_start)
    .bind(r.caps.credential_tokens)
    .bind(r.caps.user_tokens)
    .bind(r.caps.user_requests_per_minute)
    .bind(r.caps.agent_tokens)
    .fetch_optional(pool)
    .await?;
    Ok(tokens_out.map(|tokens_out| Reserved { id, tokens_out }))
}

/// Replace a reservation with the usage the provider reported.
pub async fn settle(
    pool: &SqlitePool,
    id: &str,
    model: &str,
    tokens_in: i64,
    tokens_out: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE credential_usages SET model = ?2, tokens_in = ?3, tokens_out = ?4 WHERE id = ?1",
    )
    .bind(id)
    .bind(model)
    .bind(tokens_in)
    .bind(tokens_out)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop a reservation whose request did not reach the provider or failed.
pub async fn release(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM credential_usages WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Current totals for the windows [`reserve`] checks. Not atomic with a
/// reservation; used to explain a refused one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub credential_tokens: i64,
    pub user_tokens: i64,
    pub user_requests: i64,
    pub agent_tokens: i64,
}

pub async fn totals(
    pool: &SqlitePool,
    credential_id: CredentialId,
    user_id: UserId,
    agent_id: Option<AgentId>,
    day_start: &str,
    minute_start: &str,
) -> Result<Totals> {
    let (credential_tokens, user_tokens, user_requests, agent_tokens): (i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT \
               (SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM credential_usages \
                 WHERE credential_id = ?1 AND created_at >= ?4), \
               (SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM credential_usages \
                 WHERE credential_id = ?1 AND user_id = ?2 AND created_at >= ?4), \
               (SELECT COUNT(*) FROM credential_usages \
                 WHERE credential_id = ?1 AND user_id = ?2 AND created_at >= ?5), \
               (SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM credential_usages \
                 WHERE agent_id = ?3 AND created_at >= ?4)",
        )
        .bind(credential_id.to_string())
        .bind(user_id.to_string())
        .bind(agent_id.map(|a| a.to_string()))
        .bind(day_start)
        .bind(minute_start)
        .fetch_one(pool)
        .await?;
    Ok(Totals {
        credential_tokens,
        user_tokens,
        user_requests,
        agent_tokens,
    })
}

/// Per-user token totals within a team since `since_rfc3339`. Drives
/// the dashboard's `/teams/:id/usage` view. Joined to `users.display_name`
/// and `agents.team_id` so we get one row per user with their total —
/// no N+1 lookups in the handler.
#[derive(Debug, Clone)]
pub struct TeamUserUsage {
    pub user_id: UserId,
    pub display_name: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub call_count: i64,
}

/// Rollup query: for every member of `team_id` that's used a credential
/// since `since_rfc3339`, return their token totals. Includes calls
/// against either user-scoped or team-scoped credentials, joined via
/// agents.team_id (so a member's calls on their personal agent only
/// count when that agent is associated to the team).
///
/// Spec §10.3 admin view: "aggregate CPU / memory / token usage per
/// user / credential". Token usage is the live one.
pub async fn list_team_usage(
    pool: &SqlitePool,
    team_id: havn_core::TeamId,
    since_rfc3339: &str,
) -> Result<Vec<TeamUserUsage>> {
    let rows: Vec<UsageRow> = sqlx::query_as::<_, UsageRow>(
        "SELECT u.id AS user_id, \
                u.display_name AS display_name, \
                COALESCE(SUM(cu.tokens_in), 0)  AS tokens_in, \
                COALESCE(SUM(cu.tokens_out), 0) AS tokens_out, \
                COUNT(cu.id)                    AS call_count \
         FROM team_memberships m \
         JOIN users u ON u.id = m.user_id \
         LEFT JOIN credential_usages cu \
              ON cu.user_id = m.user_id \
             AND cu.created_at >= ?2 \
         WHERE m.team_id = ?1 \
         GROUP BY u.id, u.display_name \
         ORDER BY (tokens_in + tokens_out) DESC, u.display_name",
    )
    .bind(team_id.to_string())
    .bind(since_rfc3339)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(TeamUserUsage {
                user_id: UserId::from_uuid(crate::repo::parse_db_uuid(
                    &r.user_id,
                    "credential_usages.user_id",
                )?),
                display_name: r.display_name,
                tokens_in: r.tokens_in,
                tokens_out: r.tokens_out,
                call_count: r.call_count,
            })
        })
        .collect()
}

#[derive(Debug, sqlx::FromRow)]
struct UsageRow {
    user_id: String,
    display_name: String,
    tokens_in: i64,
    tokens_out: i64,
    call_count: i64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;
    use crate::repo::agents::{NewAgent, create as create_agent};
    use crate::repo::credentials::{CredentialScope, NewCredential, create as create_cred};
    use crate::repo::users::{NewUser, create as create_user};

    const DAY: &str = "2000-01-01T00:00:00.000Z";
    const MINUTE: &str = "2000-01-01T00:00:00.000Z";

    struct Fixture {
        credential_id: CredentialId,
        user_id: UserId,
        agent_id: AgentId,
    }

    async fn fixture(pool: &SqlitePool) -> Fixture {
        let user_id = create_user(pool, NewUser { display_name: "u" })
            .await
            .expect("user")
            .id;
        let agent_id = create_agent(
            pool,
            NewAgent {
                owner_id: user_id,
                team_id: None,
                name: "a",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent")
        .id;
        let credential_id = create_cred(
            pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &user_id.to_string(),
                provider: "anthropic",
                name: None,
                api_key_ciphertext: b"k",
                priority: 1,
                limits: serde_json::json!({}),
            },
        )
        .await
        .expect("credential")
        .id;
        Fixture {
            credential_id,
            user_id,
            agent_id,
        }
    }

    fn reservation(f: &Fixture, tokens_in: i64, max_out: i64, caps: Caps) -> Reservation<'static> {
        Reservation {
            credential_id: f.credential_id,
            user_id: f.user_id,
            agent_id: Some(f.agent_id),
            provider: "anthropic",
            model: "claude-sonnet-4-6",
            tokens_in,
            max_tokens_out: max_out,
            min_tokens_out: max_out.min(10),
            caps,
            day_start: DAY,
            minute_start: MINUTE,
        }
    }

    async fn total(pool: &SqlitePool, f: &Fixture) -> Totals {
        totals(
            pool,
            f.credential_id,
            f.user_id,
            Some(f.agent_id),
            DAY,
            MINUTE,
        )
        .await
        .expect("totals")
    }

    #[tokio::test]
    async fn reservation_is_settled_or_released() {
        let pool = connect_in_memory().await.expect("connect");
        let f = fixture(&pool).await;
        let r = reserve(&pool, reservation(&f, 10, 100, Caps::default()))
            .await
            .expect("reserve")
            .expect("granted");
        assert_eq!(r.tokens_out, 100);
        assert_eq!(total(&pool, &f).await.agent_tokens, 110);

        settle(&pool, &r.id, "claude-sonnet-4-6-20260101", 7, 3)
            .await
            .expect("settle");
        let t = total(&pool, &f).await;
        assert_eq!(
            (t.credential_tokens, t.agent_tokens, t.user_requests),
            (10, 10, 1)
        );
        let (model, agent): (String, Option<String>) =
            sqlx::query_as("SELECT model, agent_id FROM credential_usages WHERE id = ?1")
                .bind(&r.id)
                .fetch_one(&pool)
                .await
                .expect("row");
        assert_eq!(model, "claude-sonnet-4-6-20260101");
        assert_eq!(agent, Some(f.agent_id.to_string()));

        let r = reserve(&pool, reservation(&f, 10, 100, Caps::default()))
            .await
            .expect("reserve")
            .expect("granted");
        release(&pool, &r.id).await.expect("release");
        assert_eq!(total(&pool, &f).await.credential_tokens, 10);
    }

    #[tokio::test]
    async fn deleting_an_agent_keeps_its_usage() {
        let pool = connect_in_memory().await.expect("connect");
        let f = fixture(&pool).await;
        let r = reserve(&pool, reservation(&f, 10, 100, Caps::default()))
            .await
            .expect("reserve")
            .expect("granted");
        settle(&pool, &r.id, "m", 40, 60).await.expect("settle");
        crate::repo::agents::delete(&pool, f.agent_id)
            .await
            .expect("delete agent");
        let t = total(&pool, &f).await;
        assert_eq!((t.credential_tokens, t.user_tokens), (100, 100));
        let agent: Option<String> =
            sqlx::query_scalar("SELECT agent_id FROM credential_usages WHERE id = ?1")
                .bind(&r.id)
                .fetch_one(&pool)
                .await
                .expect("row");
        assert_eq!(agent, None);
    }

    #[tokio::test]
    async fn each_cap_limits_the_grant() {
        let pool = connect_in_memory().await.expect("connect");
        let f = fixture(&pool).await;
        for caps in [
            Caps {
                credential_tokens: 60,
                ..Caps::default()
            },
            Caps {
                user_tokens: 60,
                ..Caps::default()
            },
            Caps {
                agent_tokens: 60,
                ..Caps::default()
            },
        ] {
            // 60 - 10 input leaves 50 of the 100 output tokens asked for.
            let r = reserve(&pool, reservation(&f, 10, 100, caps))
                .await
                .expect("reserve")
                .expect("granted");
            assert_eq!(r.tokens_out, 50, "{caps:?}");
            // The first reservation holds the whole cap.
            assert!(
                reserve(&pool, reservation(&f, 10, 100, caps))
                    .await
                    .expect("reserve")
                    .is_none(),
                "{caps:?}"
            );
            release(&pool, &r.id).await.expect("release");
        }
    }

    #[tokio::test]
    async fn per_minute_request_cap_counts_reservations() {
        let pool = connect_in_memory().await.expect("connect");
        let f = fixture(&pool).await;
        let caps = Caps {
            user_requests_per_minute: 2,
            ..Caps::default()
        };
        for _ in 0..2 {
            reserve(&pool, reservation(&f, 1, 1, caps))
                .await
                .expect("reserve")
                .expect("granted");
        }
        assert!(
            reserve(&pool, reservation(&f, 1, 1, caps))
                .await
                .expect("reserve")
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reservations_cannot_overspend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::connect(dir.path().join("havn.db"))
            .await
            .expect("connect");
        let f = fixture(&pool).await;
        let caps = Caps {
            credential_tokens: 1_000,
            ..Caps::default()
        };
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let pool = pool.clone();
            let r = reservation(&f, 50, 100, caps);
            tasks.spawn(async move { reserve(&pool, r).await.expect("reserve") });
        }
        let mut granted = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Some(r) = result.expect("join") {
                granted.push(r.tokens_out);
            }
        }
        granted.sort_unstable();
        // Six full reservations of 50 + 100 tokens, then one with the 50
        // output tokens that remain; everything after that is refused.
        assert_eq!(granted, vec![50, 100, 100, 100, 100, 100, 100]);
        assert_eq!(total(&pool, &f).await.credential_tokens, 1_000);
        pool.close().await;
    }
}
