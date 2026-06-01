//! Cron jobs + cron runs (spec §8.5).
//!
//! Phase 1 schedule grammar — kept narrow on purpose so callers can extend
//! it later without breaking stored values:
//!
//! - `interval:<N>` — recurring every N seconds.
//! - `at:<RFC 3339 timestamp>` — one-shot at the given instant; the job
//!   self-disables once it runs.
//!
//! After every run [`advance_after_run`] computes the next `next_run_at`
//! (or disables the job for the one-shot case), so the scheduler only ever
//! has to call `list_due(now)` to find what to fire.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use havn_core::{AgentId, CronJobId};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Recurring; every `seconds` from `next_run_at`.
    Interval { seconds: i64 },
    /// One-shot at the given instant. `enabled` is cleared once it fires.
    At { instant: DateTime<Utc> },
}

impl Schedule {
    pub fn encode(&self) -> String {
        match self {
            Self::Interval { seconds } => format!("interval:{seconds}"),
            Self::At { instant } => format!("at:{}", instant.to_rfc3339()),
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        if let Some(rest) = raw.strip_prefix("interval:") {
            let seconds: i64 = rest.parse().map_err(|_| DbError::InvalidValue {
                column: "cron_jobs.schedule",
                message: format!("interval value not a number: {rest:?}"),
            })?;
            if seconds < 1 {
                return Err(DbError::InvalidValue {
                    column: "cron_jobs.schedule",
                    message: "interval must be >= 1 second".into(),
                });
            }
            return Ok(Self::Interval { seconds });
        }
        if let Some(rest) = raw.strip_prefix("at:") {
            let instant = DateTime::parse_from_rfc3339(rest)
                .map_err(|e| DbError::InvalidValue {
                    column: "cron_jobs.schedule",
                    message: format!("at value not RFC3339: {e}"),
                })?
                .with_timezone(&Utc);
            return Ok(Self::At { instant });
        }
        Err(DbError::InvalidValue {
            column: "cron_jobs.schedule",
            message: format!("unknown schedule grammar {raw:?}"),
        })
    }

    /// Compute the run-after-this `next_run_at`. Returns `None` for one-shot
    /// schedules — the job is finished and should be disabled.
    pub fn advance_from(&self, last_fire: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::Interval { seconds } => Some(last_fire + ChronoDuration::seconds(*seconds)),
            Self::At { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CronJob {
    pub id: CronJobId,
    pub agent_id: AgentId,
    pub name: String,
    pub prompt: String,
    pub schedule: Schedule,
    pub next_run_at: DateTime<Utc>,
    pub wake_check: Option<String>,
    pub deliver: String,
    pub enabled_toolsets: Vec<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewCronJob<'a> {
    pub agent_id: AgentId,
    pub name: &'a str,
    pub prompt: &'a str,
    pub schedule: &'a Schedule,
    pub next_run_at: DateTime<Utc>,
    pub wake_check: Option<&'a str>,
    pub deliver: &'a str,
    pub enabled_toolsets: &'a [String],
}

pub async fn create(pool: &SqlitePool, new: NewCronJob<'_>) -> Result<CronJob> {
    let id = CronJobId::new();
    let toolsets =
        serde_json::to_string(new.enabled_toolsets).map_err(|e| DbError::InvalidValue {
            column: "cron_jobs.enabled_toolsets",
            message: e.to_string(),
        })?;
    sqlx::query(
        "INSERT INTO cron_jobs (id, agent_id, name, prompt, schedule, next_run_at, wake_check, \
                                deliver, enabled_toolsets) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(id.to_string())
    .bind(new.agent_id.to_string())
    .bind(new.name)
    .bind(new.prompt)
    .bind(new.schedule.encode())
    .bind(new.next_run_at.to_rfc3339())
    .bind(new.wake_check)
    .bind(new.deliver)
    .bind(toolsets)
    .execute(pool)
    .await?;
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

pub async fn find_by_id(pool: &SqlitePool, id: CronJobId) -> Result<Option<CronJob>> {
    let row: Option<CronJobRow> = sqlx::query_as::<_, CronJobRow>(SELECT_FIELDS_SQL)
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    row.map(CronJob::try_from).transpose()
}

const SELECT_FIELDS_SQL: &str = "SELECT id, agent_id, name, prompt, schedule, next_run_at, \
                                        wake_check, deliver, enabled_toolsets, enabled, created_at \
                                 FROM cron_jobs WHERE id = ?1";

pub async fn list_for_agent(pool: &SqlitePool, agent_id: AgentId) -> Result<Vec<CronJob>> {
    let rows: Vec<CronJobRow> = sqlx::query_as::<_, CronJobRow>(
        "SELECT id, agent_id, name, prompt, schedule, next_run_at, wake_check, deliver, \
                enabled_toolsets, enabled, created_at \
         FROM cron_jobs WHERE agent_id = ?1 ORDER BY created_at",
    )
    .bind(agent_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(CronJob::try_from).collect()
}

/// Jobs whose `next_run_at <= now` and that are still enabled. Bounded result
/// set to keep one busy tenant from monopolising a tick.
pub async fn list_due(pool: &SqlitePool, now: DateTime<Utc>, limit: u32) -> Result<Vec<CronJob>> {
    let rows: Vec<CronJobRow> = sqlx::query_as::<_, CronJobRow>(
        "SELECT id, agent_id, name, prompt, schedule, next_run_at, wake_check, deliver, \
                enabled_toolsets, enabled, created_at \
         FROM cron_jobs \
         WHERE enabled = 1 AND next_run_at <= ?1 \
         ORDER BY next_run_at LIMIT ?2",
    )
    .bind(now.to_rfc3339())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(CronJob::try_from).collect()
}

/// Apply the post-run state transition: advance `next_run_at` for recurring
/// schedules, or set `enabled = 0` for completed one-shot jobs.
pub async fn advance_after_run(
    pool: &SqlitePool,
    id: CronJobId,
    schedule: &Schedule,
    last_fire: DateTime<Utc>,
) -> Result<()> {
    if let Some(next) = schedule.advance_from(last_fire) {
        sqlx::query("UPDATE cron_jobs SET next_run_at = ?1 WHERE id = ?2")
            .bind(next.to_rfc3339())
            .bind(id.to_string())
            .execute(pool)
            .await?;
    } else {
        sqlx::query("UPDATE cron_jobs SET enabled = 0 WHERE id = ?1")
            .bind(id.to_string())
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn set_enabled(pool: &SqlitePool, id: CronJobId, enabled: bool) -> Result<()> {
    let res = sqlx::query("UPDATE cron_jobs SET enabled = ?1 WHERE id = ?2")
        .bind(i32::from(enabled))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: CronJobId) -> Result<()> {
    let res = sqlx::query("DELETE FROM cron_jobs WHERE id = ?1")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct CronJobRow {
    id: String,
    agent_id: String,
    name: String,
    prompt: String,
    schedule: String,
    next_run_at: DateTime<Utc>,
    wake_check: Option<String>,
    deliver: String,
    enabled_toolsets: String,
    enabled: i32,
    created_at: DateTime<Utc>,
}

impl TryFrom<CronJobRow> for CronJob {
    type Error = DbError;
    fn try_from(r: CronJobRow) -> Result<Self> {
        let toolsets: Vec<String> =
            serde_json::from_str(&r.enabled_toolsets).map_err(|e| DbError::InvalidValue {
                column: "cron_jobs.enabled_toolsets",
                message: e.to_string(),
            })?;
        Ok(Self {
            id: CronJobId::from_uuid(parse_db_uuid(&r.id, "cron_jobs.id")?),
            agent_id: AgentId::from_uuid(parse_db_uuid(&r.agent_id, "cron_jobs.agent_id")?),
            name: r.name,
            prompt: r.prompt,
            schedule: Schedule::parse(&r.schedule)?,
            next_run_at: r.next_run_at,
            wake_check: r.wake_check,
            deliver: r.deliver,
            enabled_toolsets: toolsets,
            enabled: r.enabled != 0,
            created_at: r.created_at,
        })
    }
}

// ---- cron_runs ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Success,
    Skipped,
    Error,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Skipped => "skipped",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CronRun {
    pub job_id: CronJobId,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub output_path: String,
    pub silent: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewCronRun<'a> {
    pub job_id: CronJobId,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub output_path: &'a str,
    pub silent: bool,
    pub error: Option<&'a str>,
}

pub async fn record_run(pool: &SqlitePool, new: NewCronRun<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO cron_runs (id, job_id, started_at, finished_at, output_path, silent, error) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(new.job_id.to_string())
    .bind(new.started_at.to_rfc3339())
    .bind(new.finished_at.map(|t| t.to_rfc3339()))
    .bind(new.output_path)
    .bind(i32::from(new.silent))
    .bind(new.error)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_runs_for_job(
    pool: &SqlitePool,
    job_id: CronJobId,
    limit: u32,
) -> Result<Vec<CronRun>> {
    let rows: Vec<CronRunRow> = sqlx::query_as::<_, CronRunRow>(
        "SELECT job_id, started_at, finished_at, output_path, silent, error \
         FROM cron_runs WHERE job_id = ?1 ORDER BY started_at DESC LIMIT ?2",
    )
    .bind(job_id.to_string())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(CronRun::try_from).collect()
}

#[derive(Debug, sqlx::FromRow)]
struct CronRunRow {
    job_id: String,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    output_path: String,
    silent: i32,
    error: Option<String>,
}

impl TryFrom<CronRunRow> for CronRun {
    type Error = DbError;
    fn try_from(r: CronRunRow) -> Result<Self> {
        Ok(Self {
            job_id: CronJobId::from_uuid(parse_db_uuid(&r.job_id, "cron_runs.job_id")?),
            started_at: r.started_at,
            finished_at: r.finished_at,
            output_path: r.output_path,
            silent: r.silent != 0,
            error: r.error,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;
    use crate::repo::agents::{NewAgent, create as create_agent};
    use crate::repo::users::{NewUser, create as create_user};

    async fn fixture() -> (SqlitePool, AgentId) {
        let pool = connect_in_memory().await.expect("db");
        let user = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user");
        let agent = create_agent(
            &pool,
            NewAgent {
                owner_id: user.id,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");
        (pool, agent.id)
    }

    #[test]
    fn schedule_parse_round_trip() {
        for s in [
            Schedule::Interval { seconds: 300 },
            Schedule::At {
                instant: DateTime::parse_from_rfc3339("2026-12-01T09:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            },
        ] {
            let encoded = s.encode();
            let parsed = Schedule::parse(&encoded).expect("parse");
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn schedule_rejects_zero_interval() {
        let err = Schedule::parse("interval:0").expect_err("should fail");
        assert!(err.to_string().contains(">= 1"));
    }

    #[test]
    fn schedule_rejects_bad_at_format() {
        assert!(Schedule::parse("at:not-a-date").is_err());
    }

    #[test]
    fn advance_interval_adds_seconds() {
        let s = Schedule::Interval { seconds: 90 };
        let now = DateTime::parse_from_rfc3339("2026-05-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = s.advance_from(now).unwrap();
        let expected = DateTime::parse_from_rfc3339("2026-05-01T12:01:30Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(next, expected);
    }

    #[test]
    fn advance_at_returns_none() {
        let s = Schedule::At {
            instant: DateTime::parse_from_rfc3339("2026-05-01T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        assert!(s.advance_from(Utc::now()).is_none());
    }

    #[tokio::test]
    async fn create_then_find() {
        let (pool, agent_id) = fixture().await;
        let now = Utc::now();
        let job = create(
            &pool,
            NewCronJob {
                agent_id,
                name: "morning email",
                prompt: "summarise unread emails",
                schedule: &Schedule::Interval { seconds: 3600 },
                next_run_at: now,
                wake_check: None,
                deliver: "broadcast",
                enabled_toolsets: &[],
            },
        )
        .await
        .expect("create");
        assert_eq!(job.name, "morning email");
        let found = find_by_id(&pool, job.id)
            .await
            .expect("find")
            .expect("some");
        assert_eq!(found.id, job.id);
    }

    #[tokio::test]
    async fn list_due_filters_by_time_and_enabled() {
        let (pool, agent_id) = fixture().await;
        let past = Utc::now() - ChronoDuration::seconds(60);
        let future = Utc::now() + ChronoDuration::seconds(3600);

        let due = create(
            &pool,
            NewCronJob {
                agent_id,
                name: "due",
                prompt: "p",
                schedule: &Schedule::Interval { seconds: 60 },
                next_run_at: past,
                wake_check: None,
                deliver: "broadcast",
                enabled_toolsets: &[],
            },
        )
        .await
        .unwrap();
        let _not_yet = create(
            &pool,
            NewCronJob {
                agent_id,
                name: "later",
                prompt: "p",
                schedule: &Schedule::Interval { seconds: 60 },
                next_run_at: future,
                wake_check: None,
                deliver: "broadcast",
                enabled_toolsets: &[],
            },
        )
        .await
        .unwrap();

        let hits = list_due(&pool, Utc::now(), 100).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, due.id);
    }

    #[tokio::test]
    async fn advance_after_run_updates_recurring_disables_one_shot() {
        let (pool, agent_id) = fixture().await;
        let now = Utc::now();

        let recurring = create(
            &pool,
            NewCronJob {
                agent_id,
                name: "rec",
                prompt: "p",
                schedule: &Schedule::Interval { seconds: 60 },
                next_run_at: now,
                wake_check: None,
                deliver: "broadcast",
                enabled_toolsets: &[],
            },
        )
        .await
        .unwrap();
        advance_after_run(&pool, recurring.id, &recurring.schedule, now)
            .await
            .unwrap();
        let after = find_by_id(&pool, recurring.id).await.unwrap().unwrap();
        assert!(after.enabled);
        assert!(after.next_run_at > now);

        let one_shot = create(
            &pool,
            NewCronJob {
                agent_id,
                name: "one",
                prompt: "p",
                schedule: &Schedule::At { instant: now },
                next_run_at: now,
                wake_check: None,
                deliver: "broadcast",
                enabled_toolsets: &[],
            },
        )
        .await
        .unwrap();
        advance_after_run(&pool, one_shot.id, &one_shot.schedule, now)
            .await
            .unwrap();
        let after = find_by_id(&pool, one_shot.id).await.unwrap().unwrap();
        assert!(!after.enabled, "one-shot should be disabled after firing");
    }

    #[tokio::test]
    async fn record_run_then_list() {
        let (pool, agent_id) = fixture().await;
        let job = create(
            &pool,
            NewCronJob {
                agent_id,
                name: "n",
                prompt: "p",
                schedule: &Schedule::Interval { seconds: 60 },
                next_run_at: Utc::now(),
                wake_check: None,
                deliver: "broadcast",
                enabled_toolsets: &[],
            },
        )
        .await
        .unwrap();

        let started = Utc::now();
        record_run(
            &pool,
            NewCronRun {
                job_id: job.id,
                started_at: started,
                finished_at: Some(started + ChronoDuration::seconds(2)),
                output_path: "/tmp/cron-output.md",
                silent: false,
                error: None,
            },
        )
        .await
        .unwrap();
        record_run(
            &pool,
            NewCronRun {
                job_id: job.id,
                started_at: started + ChronoDuration::seconds(60),
                finished_at: None,
                output_path: "",
                silent: true,
                error: Some("timed out"),
            },
        )
        .await
        .unwrap();

        let runs = list_runs_for_job(&pool, job.id, 10).await.unwrap();
        assert_eq!(runs.len(), 2);
        // DESC by started_at: timed-out run is more recent.
        assert!(runs[0].error.is_some());
        assert!(runs[1].error.is_none());
    }
}
