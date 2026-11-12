//! Cron scheduler — gateway side of spec §8.5.
//!
//! Every 60 seconds the scheduler asks the DB for jobs whose `next_run_at`
//! has elapsed, optionally runs each job's `wake_check` script
//! gateway-side (NOT in the agent — fail-open with concurrency cap), then
//! sends a `CronTick` frame to the corresponding agent. Output capture and
//! `[SILENT]` suppression happen on the way back via [`record_result`],
//! invoked by the agent socket handler when an `AgentToGateway::CronResult`
//! frame arrives. Each tick carries a gateway-assigned `run_id`; a result is
//! accepted only for a run that is still pending, once.
//!
//! Per-tick state is in-memory; long-lived state lives in `cron_jobs`
//! (next_run_at, enabled) and `cron_runs` (history). Restarting the
//! gateway resumes from whatever the DB says without losing schedule
//! drift.

#![allow(
    clippy::doc_markdown,
    reason = "schema column names like next_run_at / cron_runs read better unquoted"
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use havn_core::{AgentId, CronJobId, MessageContent, OutboundMessage};
use havn_db::DbError;
use havn_db::repo::cron::{
    self, CronJob, NewCronRun, RunOutcome, Schedule, advance_after_run, record_run,
};
use havn_proto::{CronOutcome, CronTick, GatewayToAgent};
use sqlx::SqlitePool;
use tokio::io::AsyncReadExt as _;
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
/// Max bytes of a `CronResult` output written to disk and broadcast.
const CRON_OUTPUT_CAP: usize = 256 * 1024;
/// How long a dispatched run waits for its `CronResult` before the gateway
/// forgets it (a later result is then rejected).
const PENDING_RUN_TTL: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Clone, Copy, Debug)]
struct WakeCheckCreds {
    uid: u32,
    gid: u32,
}

#[derive(Clone, Copy, Debug)]
struct PendingRun {
    agent_id: AgentId,
    job_id: CronJobId,
    dispatched_at: DateTime<Utc>,
    dispatched: Instant,
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
    /// `None` means wake-check scripts are not run (jobs always wake)
    /// unless `HAVN_WAKECHECK_UNISOLATED` opts into running them as the
    /// gateway's UID; a warning was logged at startup either way.
    wakecheck_creds: Option<WakeCheckCreds>,
    allow_unisolated_wakecheck: bool,
    /// Runs sent to an agent and not yet answered, keyed by `run_id`.
    pending: Arc<Mutex<HashMap<String, PendingRun>>>,
}

impl CronScheduler {
    pub fn new(
        db: SqlitePool,
        registry: AgentRegistry,
        webchat_router: WebChatRouter,
        workspace_root: PathBuf,
    ) -> Self {
        let wakecheck_creds = resolve_wakecheck_creds();
        let allow_unisolated_wakecheck = matches!(
            std::env::var("HAVN_WAKECHECK_UNISOLATED").as_deref(),
            Ok("1" | "true")
        );
        if wakecheck_creds.is_none() {
            if allow_unisolated_wakecheck {
                warn!(
                    "HAVN_WAKECHECK_UNISOLATED is set: wake-check scripts run as the gateway UID \
                     and can read its memory, database and config; use only on a trusted dev host"
                );
            } else {
                warn!(
                    "wake-check scripts are disabled (jobs with a wake_check always wake) because \
                     they cannot run as the dedicated havn-wakecheck user"
                );
            }
        }
        Self {
            db,
            registry,
            webchat_router,
            workspace_root,
            wake_semaphore: Arc::new(Semaphore::new(WAKE_CHECK_CONCURRENCY)),
            wakecheck_creds,
            allow_unisolated_wakecheck,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn pending_runs(&self) -> MutexGuard<'_, HashMap<String, PendingRun>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record a run about to be sent to `agent_id` and return its id.
    fn track_dispatch(&self, agent_id: AgentId, job_id: CronJobId) -> String {
        let run_id = uuid::Uuid::now_v7().to_string();
        let mut pending = self.pending_runs();
        pending.retain(|_, run| run.dispatched.elapsed() < PENDING_RUN_TTL);
        pending.insert(
            run_id.clone(),
            PendingRun {
                agent_id,
                job_id,
                dispatched_at: Utc::now(),
                dispatched: Instant::now(),
            },
        );
        run_id
    }

    /// Remove and return the pending run `run_id` if it was sent to
    /// `agent_id` for `job_id` and has not expired.
    fn take_pending(
        &self,
        agent_id: AgentId,
        job_id: CronJobId,
        run_id: &str,
    ) -> Option<PendingRun> {
        let mut pending = self.pending_runs();
        let run = *pending.get(run_id)?;
        if run.agent_id != agent_id || run.job_id != job_id {
            return None;
        }
        pending.remove(run_id);
        (run.dispatched.elapsed() < PENDING_RUN_TTL).then_some(run)
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
        let run_id = self.track_dispatch(agent_id, job_id);
        let frame = GatewayToAgent::CronTick(CronTick {
            job_id: job_id.to_string(),
            run_id: run_id.clone(),
            prompt: job.prompt.clone(),
            enabled_toolsets: job.enabled_toolsets.clone(),
        });
        if let Err(e) = self.registry.send(agent_id, frame).await {
            warn!(%job_id, %agent_id, error = %e, "could not deliver CronTick");
            self.pending_runs().remove(&run_id);
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

        if self.wakecheck_creds.is_none() && !self.allow_unisolated_wakecheck {
            debug!("wake-check skipped: no dedicated wake-check user; waking");
            return true;
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", "/")
            .current_dir("/")
            .process_group(0)
            .kill_on_drop(true);

        // Spec §8.5 / §11: wake-checks run as a dedicated unprivileged UID,
        // distinct from the gateway's. Resolved at startup; only effective
        // when the gateway runs as root (otherwise `setuid` is impossible
        // and we already logged the downgrade).
        #[cfg(unix)]
        if let Some(creds) = self.wakecheck_creds {
            // std drops supplementary groups itself (setgroups(0, NULL)) when a
            // root parent sets `uid` without explicit groups; pre_exec closures
            // run after setuid, so they cannot do it.
            cmd.uid(creds.uid).gid(creds.gid);
            let expected_gid = creds.gid;
            // SAFETY: getgroups into a stack buffer is async-signal-safe and
            // allocates nothing. std silently ignores EPERM from its own
            // setgroups (e.g. inside a user namespace with setgroups=deny),
            // so verify no supplementary group other than the target gid
            // survived before running the script.
            unsafe {
                cmd.pre_exec(move || {
                    let mut groups = [0 as libc::gid_t; 64];
                    let n = libc::getgroups(64, groups.as_mut_ptr());
                    if n < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let n = usize::try_from(n).unwrap_or(0);
                    if groups[..n].iter().any(|g| *g != expected_gid) {
                        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                    }
                    Ok(())
                });
            }
        }

        // stderr goes to /dev/null, so only stdout is ever buffered here.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        Self::wake_check_stdout(&mut cmd)
            .await
            .is_none_or(|stdout| Self::decide_wake(&stdout))
    }

    /// Spawn the wake-check and read at most [`WAKE_CHECK_STDOUT_CAP`] bytes
    /// of its stdout. `None` (fail open) on spawn/read errors, on timeout,
    /// and as soon as the script writes more than the cap; the script's
    /// process group is killed in every case but a clean exit.
    async fn wake_check_stdout(cmd: &mut Command) -> Option<Vec<u8>> {
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                warn!(error = %e, "wake-check exec failed; failing open");
                return None;
            }
        };
        let pgid = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok());
        let Some(stdout) = child.stdout.take() else {
            warn!("wake-check stdout was not piped; failing open");
            kill_group(pgid);
            return None;
        };
        let limit = u64::try_from(WAKE_CHECK_STDOUT_CAP)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let run = async {
            let mut buf = Vec::new();
            stdout.take(limit).read_to_end(&mut buf).await?;
            if buf.len() > WAKE_CHECK_STDOUT_CAP {
                return Ok(None);
            }
            child.wait().await?;
            Ok::<_, std::io::Error>(Some(buf))
        };
        match timeout(WAKE_CHECK_TIMEOUT, run).await {
            Ok(Ok(Some(buf))) => Some(buf),
            Ok(Ok(None)) => {
                warn!(
                    cap = WAKE_CHECK_STDOUT_CAP,
                    "wake-check stdout exceeded the cap; killed, failing open"
                );
                kill_group(pgid);
                None
            }
            Ok(Err(e)) => {
                warn!(error = %e, "wake-check read failed; failing open");
                kill_group(pgid);
                None
            }
            Err(_) => {
                warn!(
                    timeout_s = WAKE_CHECK_TIMEOUT.as_secs(),
                    "wake-check timed out; failing open"
                );
                kill_group(pgid);
                None
            }
        }
    }

    fn decide_wake(stdout: &[u8]) -> bool {
        // Last non-empty line of stdout.
        let stdout_str = String::from_utf8_lossy(stdout);
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
    /// `CronResult` frame. Accepts it only for a run dispatched to
    /// `agent_id` that is still pending, then persists the output (capped at
    /// [`CRON_OUTPUT_CAP`]) to disk, records a `cron_runs` row, and (unless
    /// `silent`) broadcasts the output to the agent's open WebChat sessions.
    pub async fn record_result(
        &self,
        agent_id: AgentId,
        mut result: havn_proto::CronResult,
    ) -> Result<(), CronError> {
        use std::str::FromStr as _;
        let job_id = CronJobId::from_str(&result.job_id)
            .map_err(|_| CronError::InvalidJobId(result.job_id.clone()))?;
        let run = self
            .take_pending(agent_id, job_id, &result.run_id)
            .ok_or_else(|| CronError::NotPending(result.run_id.clone()))?;

        let job = cron::find_by_id(&self.db, job_id)
            .await?
            .ok_or(CronError::UnknownJob(job_id))?;
        if job.agent_id != agent_id {
            return Err(CronError::AgentMismatch);
        }

        if result.output.len() > CRON_OUTPUT_CAP {
            warn!(
                %job_id,
                bytes = result.output.len(),
                cap = CRON_OUTPUT_CAP,
                "cron output truncated"
            );
            let end = result.output.floor_char_boundary(CRON_OUTPUT_CAP);
            result.output.truncate(end);
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
                started_at: run.dispatched_at,
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
    #[error("no pending run {0:?} for this agent and job")]
    NotPending(String),
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
            "user not found; wake-check scripts cannot be isolated. \
             Run install.sh (or `useradd --system --no-create-home --shell /usr/sbin/nologin havn-wakecheck`) \
             to enable spec §8.5 / §11 isolation."
        );
        return None;
    };
    if c.uid == 0 || c.gid == 0 {
        warn!(
            user = WAKECHECK_USERNAME,
            uid = c.uid,
            gid = c.gid,
            "wake-check user resolves to uid or gid 0; refusing to use it, so wake-check \
             scripts cannot be isolated."
        );
        return None;
    }
    if !running_as_root() {
        warn!(
            user = WAKECHECK_USERNAME,
            uid = c.uid,
            "wake-check user resolved but gateway is not root; \
             cannot setuid, so wake-check scripts cannot be isolated. \
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

fn kill_group(pgid: Option<libc::pid_t>) {
    if let Some(pgid) = pgid {
        // SAFETY: plain syscall on the process group created for this
        // wake-check (`process_group(0)` in `run_wake_check`).
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
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
        let mut s = CronScheduler::new(
            db,
            AgentRegistry::new(),
            WebChatRouter::new(),
            std::env::temp_dir().join(format!("havn-cron-{}", uuid::Uuid::now_v7())),
        );
        s.allow_unisolated_wakecheck = true;
        s
    }

    #[tokio::test]
    async fn wake_check_is_not_executed_without_isolation() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let mut s = make_scheduler(pool);
        s.wakecheck_creds = None;
        s.allow_unisolated_wakecheck = false;
        let marker = std::env::temp_dir().join(format!("havn-wake-{}", uuid::Uuid::now_v7()));
        let script = format!(
            "touch {}; echo '{{\"wakeAgent\": false}}'",
            marker.display()
        );
        assert!(s.run_wake_check(&script).await);
        assert!(!marker.exists(), "script must not run without isolation");

        s.allow_unisolated_wakecheck = true;
        assert!(!s.run_wake_check(&script).await);
        assert!(marker.exists(), "control: the opted-in script does run");
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn wake_check_runs_with_a_cleared_environment() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        assert!(
            std::env::var_os("CARGO").is_some(),
            "cargo sets CARGO for tests"
        );
        assert!(
            !s.run_wake_check("[ -z \"${CARGO:-}\" ] && echo '{\"wakeAgent\": false}'")
                .await
        );
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

    #[tokio::test]
    async fn endless_wake_check_output_is_cut_at_the_cap() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        let marker = std::env::temp_dir().join(format!("havn-wake-{}", uuid::Uuid::now_v7()));
        let started = Instant::now();
        assert!(
            s.run_wake_check(&format!("yes; touch {}", marker.display()))
                .await,
            "over-cap output fails open"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "reading must stop at the cap, not at the timeout: {:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!marker.exists(), "the script must be killed at the cap");
    }

    #[tokio::test]
    async fn wake_check_output_at_the_cap_is_still_parsed() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool);
        let line = "{\"wakeAgent\": false}\n";
        let padding = WAKE_CHECK_STDOUT_CAP - line.len() - 1;
        let script = format!(
            "head -c {padding} /dev/zero | tr '\\0' a; echo; printf '{{\"wakeAgent\": false}}\\n'"
        );
        assert!(!s.run_wake_check(&script).await);
    }

    async fn job_fixture(pool: &SqlitePool) -> (AgentId, CronJobId) {
        use havn_db::repo::agents::{NewAgent, create as create_agent};
        use havn_db::repo::users::{NewUser, create as create_user};
        let user = create_user(pool, NewUser { display_name: "u" })
            .await
            .expect("user");
        let agent = create_agent(
            pool,
            NewAgent {
                owner_id: user.id,
                team_id: None,
                name: "a",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");
        let job = cron::create(
            pool,
            cron::NewCronJob {
                agent_id: agent.id,
                name: "j",
                prompt: "p",
                schedule: &Schedule::Interval { seconds: 3600 },
                next_run_at: Utc::now(),
                wake_check: None,
                deliver: "local",
                enabled_toolsets: &[],
            },
        )
        .await
        .expect("job");
        (agent.id, job.id)
    }

    fn result_for(job_id: CronJobId, run_id: &str, output: String) -> havn_proto::CronResult {
        havn_proto::CronResult {
            job_id: job_id.to_string(),
            run_id: run_id.into(),
            outcome: CronOutcome::Success,
            output,
            silent: true,
        }
    }

    #[tokio::test]
    async fn cron_result_is_accepted_once_and_only_for_a_dispatched_run() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool.clone());
        let (agent_id, job_id) = job_fixture(&pool).await;
        let unsolicited = s
            .record_result(agent_id, result_for(job_id, "", "x".into()))
            .await;
        assert!(
            matches!(unsolicited, Err(CronError::NotPending(_))),
            "{unsolicited:?}"
        );

        let run_id = s.track_dispatch(agent_id, job_id);
        let other_agent = s
            .record_result(AgentId::new(), result_for(job_id, &run_id, "x".into()))
            .await;
        assert!(
            matches!(other_agent, Err(CronError::NotPending(_))),
            "{other_agent:?}"
        );

        s.record_result(agent_id, result_for(job_id, &run_id, "x".into()))
            .await
            .expect("the dispatched run is accepted");
        let duplicate = s
            .record_result(agent_id, result_for(job_id, &run_id, "x".into()))
            .await;
        assert!(
            matches!(duplicate, Err(CronError::NotPending(_))),
            "{duplicate:?}"
        );
        let runs = cron::list_runs_for_job(&pool, job_id, 10)
            .await
            .expect("runs");
        assert_eq!(runs.len(), 1, "one row per dispatched run");
    }

    #[tokio::test]
    async fn stored_cron_output_is_capped() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        let s = make_scheduler(pool.clone());
        let (agent_id, job_id) = job_fixture(&pool).await;
        let run_id = s.track_dispatch(agent_id, job_id);
        let output = "\u{e9}".repeat(CRON_OUTPUT_CAP);
        s.record_result(agent_id, result_for(job_id, &run_id, output))
            .await
            .expect("recorded");
        let runs = cron::list_runs_for_job(&pool, job_id, 10)
            .await
            .expect("runs");
        let stored = std::fs::read(&runs[0].output_path).expect("output file");
        assert!(stored.len() <= CRON_OUTPUT_CAP, "{} bytes", stored.len());
        assert!(stored.len() > CRON_OUTPUT_CAP - 4);
        assert!(String::from_utf8(stored).is_ok(), "cut on a char boundary");
    }
}
