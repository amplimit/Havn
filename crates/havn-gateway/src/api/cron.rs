//! `/agents/{id}/cron` endpoints (spec §8.3 / §8.5).

use std::str::FromStr as _;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use havn_core::CronJobId;
use havn_db::repo::cron::{self, CronJob, CronRun, NewCronJob, Schedule};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;

#[derive(Debug, Deserialize)]
pub struct CreateCronRequest {
    pub name: String,
    pub prompt: String,
    /// `{"interval_seconds": N}`, `{"at": "<RFC3339>"}`, `{"cron": "0 8 * *
    /// *"}` or `{"every": "daily 08:00"}`; the last two in `timezone`, by
    /// default the owner's.
    pub schedule: ScheduleInput,
    /// Optional first-run override; when absent, schedules immediately for
    /// `interval` jobs and uses `at` for one-shot.
    #[serde(default)]
    pub next_run_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub wake_check: Option<String>,
    /// Routing target. v0.6 accepts only `"local"` (write file + audit
    /// trail, no broadcast) or `"webchat"` (write file AND broadcast to
    /// any open WebChat sessions for this agent). Earlier drafts also
    /// accepted `"telegram"` / `"slack"` / `"email"` / etc.; cut per
    /// spec §8.5 because external channel delivery is the operator's
    /// webhook concern.
    #[serde(default = "default_deliver")]
    pub deliver: String,
    #[serde(default)]
    pub enabled_toolsets: Vec<String>,
}

fn default_deliver() -> String {
    "webchat".into()
}

/// Validate `deliver` against the v0.6 enum. Returns `None` when valid.
fn validate_deliver(s: &str) -> Option<String> {
    match s {
        "local" | "webchat" => None,
        other => Some(format!(
            "deliver must be 'local' or 'webchat'; got {other:?}. \
             External chat-platform delivery is the operator's webhook \
             concern (spec §8.5)."
        )),
    }
}

/// Exactly one of `interval_seconds`, `at`, `cron` and `every`; `timezone`
/// only with the last two.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleInput {
    #[serde(default)]
    pub interval_seconds: Option<i64>,
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cron: Option<String>,
    #[serde(default)]
    pub every: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
}

enum ScheduleKind {
    Interval(i64),
    At(DateTime<Utc>),
    Calendar(String, Option<String>),
}

impl ScheduleInput {
    fn kind(self) -> Result<ScheduleKind, String> {
        let given = [
            self.interval_seconds.is_some(),
            self.at.is_some(),
            self.cron.is_some(),
            self.every.is_some(),
        ]
        .iter()
        .filter(|g| **g)
        .count();
        if given != 1 {
            return Err(
                "schedule needs exactly one of interval_seconds, at, cron and every".into(),
            );
        }
        if self.timezone.is_some() && self.cron.is_none() && self.every.is_none() {
            return Err("timezone goes with cron or every".into());
        }
        Ok(
            match (self.interval_seconds, self.at, self.cron.or(self.every)) {
                (Some(seconds), _, _) => ScheduleKind::Interval(seconds),
                (_, Some(at), _) => ScheduleKind::At(at),
                (_, _, Some(expr)) => ScheduleKind::Calendar(expr, self.timezone),
                _ => return Err("schedule is empty".into()),
            },
        )
    }
}

/// A calendar schedule from a cron expression or words, in `timezone`
/// or else `owner`'s zone.
pub async fn calendar_schedule(
    db: &sqlx::SqlitePool,
    owner: havn_core::UserId,
    expr: &str,
    timezone: Option<&str>,
) -> Result<havn_core::calendar::Calendar, String> {
    let expr = havn_core::calendar::Calendar::from_words(expr).unwrap_or_else(|| expr.to_string());
    let zone = match timezone {
        Some(name) => crate::timezone::canonical(name)?,
        None => crate::timezone::for_user(db, owner).await,
    };
    havn_core::calendar::Calendar::parse(&zone, &expr)
}

#[derive(Debug, Serialize)]
pub struct CronJobView {
    pub id: String,
    pub agent_id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: ScheduleView,
    pub next_run_at: DateTime<Utc>,
    pub wake_check: Option<String>,
    pub deliver: String,
    pub enabled_toolsets: Vec<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    /// The schedule in words, such as `cron 0 8 * * * in Asia/Shanghai`.
    pub summary: String,
    /// The latest run, in listings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<CronRunView>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleView {
    Interval { seconds: i64 },
    At { instant: DateTime<Utc> },
    Calendar { cron: String, timezone: String },
}

impl From<CronJob> for CronJobView {
    fn from(j: CronJob) -> Self {
        let summary = crate::cron::describe_schedule(&j.schedule);
        let schedule = match j.schedule {
            Schedule::Interval { seconds } => ScheduleView::Interval { seconds },
            Schedule::At { instant } => ScheduleView::At { instant },
            Schedule::Calendar(c) => ScheduleView::Calendar {
                cron: c.expr().to_string(),
                timezone: c.zone().to_string(),
            },
        };
        Self {
            id: j.id.to_string(),
            agent_id: j.agent_id.to_string(),
            summary,
            name: j.name,
            prompt: j.prompt,
            schedule,
            next_run_at: j.next_run_at,
            wake_check: j.wake_check,
            deliver: j.deliver,
            enabled_toolsets: j.enabled_toolsets,
            enabled: j.enabled,
            created_at: j.created_at,
            last_run: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CronRunView {
    pub job_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub output_path: String,
    pub silent: bool,
    pub error: Option<String>,
}

impl From<CronRun> for CronRunView {
    fn from(r: CronRun) -> Self {
        Self {
            job_id: r.job_id.to_string(),
            started_at: r.started_at,
            finished_at: r.finished_at,
            output_path: r.output_path,
            silent: r.silent,
            error: r.error,
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<CronJobView>>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&agent_id, &user, &state).await?;
    let agent_id = agent.id;
    let jobs = cron::list_for_agent(&state.db, agent_id).await?;
    let mut views = Vec::with_capacity(jobs.len());
    for job in jobs {
        let id = job.id;
        let mut view = CronJobView::from(job);
        view.last_run = cron::list_runs_for_job(&state.db, id, 1)
            .await?
            .into_iter()
            .next()
            .map(CronRunView::from);
        views.push(view);
    }
    Ok(Json(views))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(agent_id): Path<String>,
    Json(req): Json<CreateCronRequest>,
) -> Result<(StatusCode, Json<CronJobView>), ApiError> {
    let agent = crate::api::agents::resolve_agent(&agent_id, &user, &state).await?;
    let agent_id = agent.id;

    // Spec §6.3 enforcement: `can_schedule_cron` is a per-user
    // capability (the owner's team role for a team agent); refuse here
    // before INSERT so a denied user can never get a cron job into the table.
    let user_policy = crate::policy_resolver::for_owner(&state.db, &agent).await;
    if !user_policy.permissions.can_schedule_cron {
        return Err(ApiError::Forbidden(
            "this user's policy does not allow scheduling cron jobs (can_schedule_cron=false)"
                .into(),
        ));
    }

    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must be non-empty".into()));
    }
    if req.prompt.trim().is_empty() {
        return Err(ApiError::BadRequest("prompt must be non-empty".into()));
    }
    if let Some(err) = validate_deliver(req.deliver.trim()) {
        return Err(ApiError::BadRequest(err));
    }

    let now = Utc::now();
    let kind = req.schedule.kind().map_err(ApiError::BadRequest)?;
    let (schedule, default_next) = match kind {
        ScheduleKind::Interval(interval_seconds) => {
            if interval_seconds < 1 {
                return Err(ApiError::BadRequest("interval_seconds must be >= 1".into()));
            }
            (
                Schedule::Interval {
                    seconds: interval_seconds,
                },
                now + ChronoDuration::seconds(interval_seconds),
            )
        }
        ScheduleKind::At(at) => (Schedule::At { instant: at }, at),
        ScheduleKind::Calendar(cron, timezone) => {
            let c = calendar_schedule(&state.db, agent.owner_id, &cron, timezone.as_deref())
                .await
                .map_err(ApiError::BadRequest)?;
            let first = c
                .next_after(now)
                .ok_or_else(|| ApiError::BadRequest(format!("{:?} never runs", c.expr())))?;
            (Schedule::Calendar(c), first)
        }
    };
    let next_run_at = req.next_run_at.unwrap_or(default_next);

    let job = cron::create(
        &state.db,
        NewCronJob {
            chat: None,
            agent_id,
            name: req.name.trim(),
            prompt: req.prompt.trim(),
            schedule: &schedule,
            next_run_at,
            wake_check: req.wake_check.as_deref(),
            deliver: req.deliver.trim(),
            enabled_toolsets: &req.enabled_toolsets,
        },
    )
    .await?;
    info!(job_id = %job.id, %agent_id, name = %job.name, "cron job created");
    Ok((StatusCode::CREATED, Json(job.into())))
}

#[derive(Debug, Deserialize)]
pub struct UpdateCronRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((agent_id, job_id)): Path<(String, String)>,
    Json(req): Json<UpdateCronRequest>,
) -> Result<Json<CronJobView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&agent_id, &user, &state).await?;
    let agent_id = agent.id;
    let job_id = parse_job(&job_id)?;
    let job = cron::find_by_id(&state.db, job_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if job.agent_id != agent_id {
        return Err(ApiError::NotFound);
    }
    if let Some(enabled) = req.enabled {
        cron::set_enabled(&state.db, job_id, enabled).await?;
    }
    let updated = cron::find_by_id(&state.db, job_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(updated.into()))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((agent_id, job_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&agent_id, &user, &state).await?;
    let agent_id = agent.id;
    let job_id = parse_job(&job_id)?;
    let job = cron::find_by_id(&state.db, job_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if job.agent_id != agent_id {
        return Err(ApiError::NotFound);
    }
    cron::delete(&state.db, job_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_runs(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((agent_id, job_id)): Path<(String, String)>,
) -> Result<Json<Vec<CronRunView>>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&agent_id, &user, &state).await?;
    let agent_id = agent.id;
    let job_id = parse_job(&job_id)?;
    let job = cron::find_by_id(&state.db, job_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if job.agent_id != agent_id {
        return Err(ApiError::NotFound);
    }
    let runs = cron::list_runs_for_job(&state.db, job_id, 100).await?;
    Ok(Json(runs.into_iter().map(CronRunView::from).collect()))
}

/// `POST /agents/{id}/cron/{job_id}/run`: run a task now (issue #236),
/// paused or not, without its wake check and without moving its schedule.
/// Its result goes where a scheduled run's does.
pub async fn run_now(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((agent_id, job_id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let agent = crate::api::agents::resolve_agent(&agent_id, &user, &state).await?;
    let job_id = parse_job(&job_id)?;
    let job = cron::find_by_id(&state.db, job_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if job.agent_id != agent.id {
        return Err(ApiError::NotFound);
    }
    let run_id = state
        .cron_scheduler
        .run_now(&job)
        .await
        .map_err(|e| match e {
            crate::cron::CronError::NotRunning => {
                ApiError::Conflict("the agent is not running; start it first".into())
            }
            other => ApiError::Internal(other.to_string()),
        })?;
    crate::audit::record(
        &state.db,
        user.id,
        agent.team_id,
        Some(agent.id),
        "cron.run_now",
        serde_json::json!({ "job_id": job_id.to_string(), "run_id": run_id }),
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "run_id": run_id })),
    ))
}

fn parse_job(s: &str) -> Result<CronJobId, ApiError> {
    CronJobId::from_str(s).map_err(|_| ApiError::BadRequest("invalid cron job id".into()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_schedule_names_one_kind() {
        let parse = |v: serde_json::Value| -> Result<ScheduleKind, String> {
            serde_json::from_value::<ScheduleInput>(v)
                .map_err(|e| e.to_string())?
                .kind()
        };
        assert!(matches!(
            parse(serde_json::json!({"interval_seconds": 60})),
            Ok(ScheduleKind::Interval(60))
        ));
        assert!(matches!(
            parse(serde_json::json!({"every": "daily 08:00", "timezone": "Asia/Shanghai"})),
            Ok(ScheduleKind::Calendar(_, Some(_)))
        ));
        assert!(parse(serde_json::json!({"interval_seconds": 60, "cron": "0 8 * * *"})).is_err());
        assert!(
            parse(serde_json::json!({"at": "2026-12-01T08:00:00Z", "timezone": "UTC"})).is_err()
        );
        assert!(parse(serde_json::json!({"cron": "0 8 * * *", "zone": "UTC"})).is_err());
        assert!(parse(serde_json::json!({})).is_err());
    }
}
