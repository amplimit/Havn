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
    /// Either `{"interval_seconds": N}` or `{"at": "<RFC3339>"}`.
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

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ScheduleInput {
    Interval { interval_seconds: i64 },
    At { at: DateTime<Utc> },
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
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleView {
    Interval { seconds: i64 },
    At { instant: DateTime<Utc> },
}

impl From<CronJob> for CronJobView {
    fn from(j: CronJob) -> Self {
        let schedule = match j.schedule {
            Schedule::Interval { seconds } => ScheduleView::Interval { seconds },
            Schedule::At { instant } => ScheduleView::At { instant },
        };
        Self {
            id: j.id.to_string(),
            agent_id: j.agent_id.to_string(),
            name: j.name,
            prompt: j.prompt,
            schedule,
            next_run_at: j.next_run_at,
            wake_check: j.wake_check,
            deliver: j.deliver,
            enabled_toolsets: j.enabled_toolsets,
            enabled: j.enabled,
            created_at: j.created_at,
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
    Ok(Json(jobs.into_iter().map(CronJobView::from).collect()))
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
    // capability; refuse here before INSERT so a denied user can never
    // get a cron job into the table.
    let user_policy = crate::policy_resolver::for_user(&state.db, user.id).await;
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
    let (schedule, default_next) = match req.schedule {
        ScheduleInput::Interval { interval_seconds } => {
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
        ScheduleInput::At { at } => (Schedule::At { instant: at }, at),
    };
    let next_run_at = req.next_run_at.unwrap_or(default_next);

    let job = cron::create(
        &state.db,
        NewCronJob {
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

fn parse_job(s: &str) -> Result<CronJobId, ApiError> {
    CronJobId::from_str(s).map_err(|_| ApiError::BadRequest("invalid cron job id".into()))
}
