//! Cron scheduler — gateway side of spec §8.5.
//!
//! Every 60 seconds the scheduler asks the DB for jobs whose `next_run_at`
//! has elapsed, optionally runs each job's `wake_check` script
//! gateway-side (NOT in the agent — fail-open with concurrency cap), then
//! sends a `CronTick` frame to the corresponding agent. Output capture and
//! `[SILENT]` suppression happen on the way back via [`record_result`],
//! invoked by the agent socket handler when an `AgentToGateway::CronResult`
//! frame arrives.
//!
//! Per-tick state is in-memory; long-lived state lives in `cron_jobs`
//! (next_run_at, enabled) and `cron_runs` (history). Restarting the
//! gateway resumes from whatever the DB says without losing schedule
//! drift.

#![allow(
    clippy::doc_markdown,
    reason = "schema column names like next_run_at / cron_runs read better unquoted"
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use havn_core::{AgentId, MessageContent, OutboundMessage};
use havn_db::DbError;
use havn_db::repo::cron::{
    self, CronJob, NewCronRun, RunOutcome, Schedule, advance_after_run, record_run,
};
use havn_proto::{CronOutcome, CronTick, GatewayToAgent};
use sqlx::SqlitePool;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::{MissedTickBehavior, timeout};
use tracing::{debug, info, warn};

use crate::agent_registry::AgentRegistry;
use crate::webchat::WebChatRouter;

/// Dedicated system user the wake-check subprocess setuid()s to (spec §8.5 / §11).
/// Created by `install.sh`; missing in dev environments — see [`resolve_wakecheck_creds`].
const WAKECHECK_USERNAME: &str = "havn-wakecheck";

/// Tick cadence per spec §8.5.
const TICK_INTERVAL: Duration = Duration::from_secs(60);
/// Per-tick cap on jobs picked up — bounds load when many jobs are due at once.
const TICK_BATCH_LIMIT: u32 = 100;
/// Max wall-clock for any single wake-check subprocess (spec §8.5).
const WAKE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// Max stdout bytes captured from a wake-check (spec §8.5: 64 KiB).
const WAKE_CHECK_STDOUT_CAP: usize = 64 * 1024;
/// Default global concurrency for wake-check subprocesses (spec §8.5: 16).
const WAKE_CHECK_CONCURRENCY: usize = 16;

#[derive(Clone, Copy, Debug)]
struct WakeCheckCreds {
    uid: u32,
    gid: u32,
}

#[derive(Clone)]
pub struct CronScheduler {
    db: SqlitePool,
    registry: AgentRegistry,
    webchat_router: WebChatRouter,
    workspace_root: PathBuf,
    wake_semaphore: Arc<Semaphore>,
    /// Resolved at startup. `Some(_)` only when both:
    /// - the `havn-wakecheck` user exists on this host, AND
    /// - the gateway is running as root (a non-root gateway cannot `setuid`).
    ///
    /// `None` means wake-checks run as the gateway's UID with the same
    /// fail-open semantics, and a warning was logged at startup.
    wakecheck_creds: Option<WakeCheckCreds>,
}

impl CronScheduler {
    pub fn new(
        db: SqlitePool,
        registry: AgentRegistry,
        webchat_router: WebChatRouter,
        workspace_root: PathBuf,
    ) -> Self {
        let wakecheck_creds = resolve_wakecheck_creds();
        Self {
            db,
            registry,
            webchat_router,
            workspace_root,
            wake_semaphore: Arc::new(Semaphore::new(WAKE_CHECK_CONCURRENCY)),
            wakecheck_creds,
        }
    }

    pub async fn run(self) {
        info!(
            tick_interval_s = TICK_INTERVAL.as_secs(),
            wake_check_concurrency = WAKE_CHECK_CONCURRENCY,
            "cron scheduler started"
        );
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Skip the immediate first fire so we don't double-tick on startup.
        interval.tick().await;
        loop {
            interval.tick().await;
            self.tick_once().await;
        }
    }

    async fn tick_once(&self) {
        let now = Utc::now();
        let due = match cron::list_due(&self.db, now, TICK_BATCH_LIMIT).await {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "list_due failed");
                return;
            }
        };
        if due.is_empty() {
            return;
        }
        debug!(count = due.len(), "dispatching due cron jobs");
        for job in due {
            let scheduler = self.clone();
            tokio::spawn(async move { scheduler.dispatch(job).await });
        }
    }

    async fn dispatch(&self, job: CronJob) {
        let agent_id = job.agent_id;
        let job_id = job.id;

        // 1. Optional wake check.
        if let Some(script) = &job.wake_check {
            let wake = self.run_wake_check(script).await;
            if !wake {
                debug!(%job_id, "wake_check returned skip; recording skipped run");
                let now = Utc::now();
                let _ = record_run(
                    &self.db,
                    NewCronRun {
                        job_id,
                        started_at: now,
                        finished_at: Some(now),
                        output_path: "",
                        silent: true,
                        error: Some(SKIPPED_BY_WAKE_CHECK),
                    },
                )
                .await;
                let _ = advance_after_run(&self.db, job_id, &job.schedule, now).await;
                return;
            }
        }

        // 2. Agent must be connected; otherwise we re-fire at the next tick.
        if !self.registry.is_connected(agent_id).await {
            warn!(%job_id, %agent_id, "agent not connected; re-trying next tick");
            return;
        }

        // 3. Send CronTick. Advance schedule eagerly so we don't double-fire if
        //    the agent takes a while; the next tick will see the new next_run_at.
        let frame = GatewayToAgent::CronTick(CronTick {
            job_id: job_id.to_string(),
            prompt: job.prompt.clone(),
            enabled_toolsets: job.enabled_toolsets.clone(),
        });
        if let Err(e) = self.registry.send(agent_id, frame).await {
            warn!(%job_id, %agent_id, error = %e, "could not deliver CronTick");
            return;
        }
        let now = Utc::now();
        if let Err(e) = advance_after_run(&self.db, job_id, &job.schedule, now).await {
            warn!(%job_id, error = %e, "advance_after_run failed");
        }
        debug!(%job_id, %agent_id, "cron tick dispatched");
    }

    /// Run a wake-check script gateway-side. Fails open: timeout, non-zero
    /// exit, or any parse error → wake. Only an explicit `{"wake": false}`
    /// JSON on the last stdout line skips. Bounded by a global semaphore so
    /// one tenant can't starve others.
    pub(crate) async fn run_wake_check(&self, script: &str) -> bool {
        let Ok(_permit) = self.wake_semaphore.clone().acquire_owned().await else {
            warn!("wake-check semaphore closed; failing open");
            return true;
        };

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script).kill_on_drop(true);

        // Spec §8.5 / §11: wake-checks run as a dedicated unprivileged UID,
        // distinct from the gateway's. Resolved at startup; only effective
        // when the gateway runs as root (otherwise `setuid` is impossible
        // and we already logged the downgrade).
        #[cfg(unix)]
        if let Some(creds) = self.wakecheck_creds {
            cmd.uid(creds.uid).gid(creds.gid);
            // SAFETY: `setgroups(0, NULL)` drops supplementary groups; it
            // performs no allocation and touches no shared state, so it
            // satisfies the post-fork pre-exec contract. Running before
            // setuid means the call needs CAP_SETGID, which root has.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setgroups(0, std::ptr::null()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let output = match timeout(WAKE_CHECK_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                warn!(error = %e, "wake-check exec failed; failing open");
                return true;
            }
            Err(_) => {
                warn!(
                    timeout_s = WAKE_CHECK_TIMEOUT.as_secs(),
                    "wake-check timed out; failing open"
                );
                return true;
            }
        };

        let mut stdout = output.stdout;
        if stdout.len() > WAKE_CHECK_STDOUT_CAP {
            warn!(
                bytes = stdout.len(),
                cap = WAKE_CHECK_STDOUT_CAP,
                "wake-check stdout truncated"
            );
            stdout.truncate(WAKE_CHECK_STDOUT_CAP);
        }

        // Last non-empty line of stdout.
        let stdout_str = String::from_utf8_lossy(&stdout);
        let last_line = stdout_str
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or("");

        // The JSON key is `wakeAgent` to match the Hermes Agent convention —
        // users can paste their existing Hermes wake-gate scripts unchanged.
        // Anything other than an explicit `false` (including missing key,
        // malformed JSON, non-JSON output) means "wake".
        let wake = serde_json::from_str::<serde_json::Value>(last_line)
            .ok()
            .and_then(|v| v.get("wakeAgent").and_then(serde_json::Value::as_bool))
            .unwrap_or(true);

        debug!(wake, "wake_check decision");
        wake
    }

    /// Called by the agent socket handler when the runtime returns a
    /// `CronResult` frame. Persists the output to disk, records a `cron_runs`
    /// row, and (unless `silent`) broadcasts the output to the agent's open
    /// WebChat sessions.
    pub async fn record_result(
        &self,
        agent_id: AgentId,
        result: havn_proto::CronResult,
    ) -> Result<(), CronError> {
        use std::str::FromStr as _;
        let job_id = havn_core::CronJobId::from_str(&result.job_id)
            .map_err(|_| CronError::InvalidJobId(result.job_id.clone()))?;

        let job = cron::find_by_id(&self.db, job_id)
            .await?
            .ok_or(CronError::UnknownJob(job_id))?;
        if job.agent_id != agent_id {
            return Err(CronError::AgentMismatch);
        }

        let now = Utc::now();
        let timestamp = now.format("%Y%m%dT%H%M%SZ");
        let output_dir = self
            .workspace_root
            .join(agent_id.to_string())
            .join("cron")
            .join(&result.job_id);
        if let Err(e) = tokio::fs::create_dir_all(&output_dir).await {
            warn!(error = %e, dir = %output_dir.display(), "create_dir_all for cron output failed");
            return Err(CronError::Io(e));
        }
        let output_path = output_dir.join(format!("{timestamp}.md"));
        if let Err(e) = tokio::fs::write(&output_path, &result.output).await {
            warn!(error = %e, file = %output_path.display(), "writing cron output failed");
            return Err(CronError::Io(e));
        }

        let (outcome, error_text) = match &result.outcome {
            CronOutcome::Success => (RunOutcome::Success, None),
            CronOutcome::Error { message } => (RunOutcome::Error, Some(message.clone())),
        };

        record_run(
            &self.db,
            NewCronRun {
                job_id,
                started_at: now,
                finished_at: Some(now),
                output_path: output_path.to_string_lossy().as_ref(),
                silent: result.silent,
                error: error_text.as_deref(),
            },
        )
        .await?;

        if !result.silent && outcome == RunOutcome::Success {
            let outbound = OutboundMessage {
                channel_id: "broadcast".into(),
                content: MessageContent::Text {
                    text: result.output,
                },
                reply_to: None,
            };
            let n = self.webchat_router.broadcast(agent_id, outbound).await;
            debug!(%job_id, sessions = n, "cron output broadcast");
        }

        Ok(())
    }
}

const SKIPPED_BY_WAKE_CHECK: &str = "skipped: wake_check returned wake=false";

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CronError {
    #[error("invalid job id: {0:?}")]
    InvalidJobId(String),
    #[error("unknown job: {0}")]
    UnknownJob(havn_core::CronJobId),
    #[error("agent does not own job")]
    AgentMismatch,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("db: {0}")]
    Db(#[from] DbError),
}

#[allow(
    dead_code,
    reason = "exposed for the schedule-help text in dashboard responses"
)]
pub fn describe_schedule(s: &Schedule) -> String {
    match s {
        Schedule::Interval { seconds } => format!("every {seconds}s"),
        Schedule::At { instant } => format!("once at {}", instant.to_rfc3339()),
    }
}

/// Resolve the `havn-wakecheck` UID/GID and decide whether we can actually
/// drop privs to it. Logged loudly so operators see when production
/// isolation is degraded.
#[cfg(unix)]
fn resolve_wakecheck_creds() -> Option<WakeCheckCreds> {
    let Some(c) = lookup_user(WAKECHECK_USERNAME) else {
        warn!(
            user = WAKECHECK_USERNAME,
            "user not found; wake-checks run as gateway UID. \
             Run install.sh (or `useradd --system --no-create-home --shell /usr/sbin/nologin havn-wakecheck`) \
             to enable spec §8.5 / §11 isolation."
        );
        return None;
    };
    if !running_as_root() {
        warn!(
            user = WAKECHECK_USERNAME,
            uid = c.uid,
            "wake-check user resolved but gateway is not root; \
             cannot setuid — running wake-checks as gateway UID. \
             Production deployments need root or CAP_SETUID."
        );
        return None;
    }
    info!(
        user = WAKECHECK_USERNAME,
        uid = c.uid,
        gid = c.gid,
        "wake-check setuid creds active"
    );
    Some(c)
}

#[cfg(not(unix))]
const fn resolve_wakecheck_creds() -> Option<WakeCheckCreds> {
    None
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: `getuid` is async-signal-safe and always succeeds.
    unsafe { libc::getuid() == 0 }
}

#[cfg(unix)]
fn lookup_user(name: &str) -> Option<WakeCheckCreds> {
    let cname = std::ffi::CString::new(name).ok()?;
    // 4 KiB is the conventional safe size for `_SC_GETPW_R_SIZE_MAX` callers.
    let mut buf = vec![0u8; 4096];
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result: *mut libc::passwd = std::ptr::null_mut();

    // SAFETY: pointers are valid for the duration of the call; buffer is
    // sized appropriately for typical passwd entries.
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast::<libc::c_char>(),
            buf.len(),
            &raw mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: rc == 0 with non-null `result` means `pwd` is initialised.
    let pwd = unsafe { pwd.assume_init() };
    Some(WakeCheckCreds {
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn make_scheduler(db: SqlitePool) -> CronScheduler {
        CronScheduler::new(
            db,
            AgentRegistry::new(),
            WebChatRouter::new(),
            std::env::temp_dir().join(format!("havn-cron-{}", uuid::Uuid::now_v7())),
        )
    }

    #[tokio::test]
    async fn wake_check_default_wakes_when_script_succeeds_silently() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        // Script writes nothing → no JSON → fail-open default = wake.
        assert!(s.run_wake_check("true").await);
    }

    #[tokio::test]
    async fn wake_check_skip_when_last_line_is_wake_agent_false() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        // `wakeAgent` is the Hermes Agent convention — keeping the same key
        // means users can paste their existing wake-gate scripts unchanged.
        assert!(
            !s.run_wake_check("printf 'noise\\n{\"wakeAgent\": false}\\n'")
                .await
        );
    }

    #[tokio::test]
    async fn wake_check_wake_when_last_line_is_wake_agent_true() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        assert!(s.run_wake_check("echo '{\"wakeAgent\": true}'").await);
    }

    #[tokio::test]
    async fn wake_check_unrecognised_keys_fail_open() {
        // Anything other than `{"wakeAgent": false}` must wake — fail-open
        // is the safer default and matches Hermes's behaviour. Tests both
        // misspelled keys and the bare `wake` shorthand from older prototypes.
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        assert!(s.run_wake_check("echo '{\"wake\": false}'").await);
        assert!(s.run_wake_check("echo '{\"WakeAgent\": false}'").await);
    }

    #[tokio::test]
    async fn wake_check_fails_open_on_nonzero_exit() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        // `false` exits 1; output empty; no JSON → wake (fail-open).
        assert!(s.run_wake_check("false").await);
    }

    #[tokio::test]
    async fn wake_check_fails_open_on_timeout() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        // Intentionally exceed the 10s timeout. Fail open = wake.
        assert!(s.run_wake_check("sleep 12").await);
    }

    #[tokio::test]
    async fn wake_check_ignores_non_json_last_lines() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        assert!(s.run_wake_check("echo 'definitely not json'").await);
    }
}
