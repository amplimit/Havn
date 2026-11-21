//! `schedule`, a tool the gateway runs for agents (issue #104): an agent
//! creates, lists, pauses, resumes and deletes its own scheduled tasks
//! from chat ("remind me at 5pm", "a news summary every morning"). A task
//! made in a conversation sends its results there, channel conversations
//! included, through the checks every agent message gets.

use std::str::FromStr as _;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, TimeZone as _, Utc};
use havn_core::{AgentId, CronJobId, UserId};
use havn_db::repo::cron::{self, NewCronJob, Schedule};
use havn_proto::GatewayToolSpec;
use serde_json::{Value, json};
use sqlx::SqlitePool;

pub const TOOL_NAME: &str = "schedule";

/// Enabled tasks one agent may have.
pub const MAX_ACTIVE_TASKS: usize = 20;
/// Tasks, enabled or done, one agent may keep; one-shots that ran more
/// than [`DONE_KEPT_DAYS`] ago are deleted to make room.
const MAX_TASKS: usize = 200;
const DONE_KEPT_DAYS: i64 = 7;
/// Shortest time between two runs of a recurring task, checked over the
/// runs of the next [`GAP_WINDOW_DAYS`].
const MIN_GAP: ChronoDuration = ChronoDuration::minutes(5);
const GAP_WINDOW_DAYS: i64 = 32;
/// Longest conversation id stored with a task.
const MAX_CONVERSATION_BYTES: usize = 512;

/// One change to the task list at a time, so the caps hold when several
/// calls run at once.
static CHANGES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const MAX_NAME_CHARS: usize = 100;
const MAX_PROMPT_CHARS: usize = 4000;

#[must_use]
pub fn spec() -> GatewayToolSpec {
    GatewayToolSpec {
        name: TOOL_NAME.into(),
        description: "Manage your scheduled tasks: reminders and recurring jobs that run later \
                      on their own, with the prompt you give, and send their result to this \
                      conversation. action=create needs name, prompt and one of: at (a time, \
                      \"2026-11-16 17:00\" in the user's time zone or with an offset), \
                      in_minutes, every (\"daily 08:00\", \"weekdays 09:30\", \"weekly mon \
                      10:00\", \"monthly 1 09:00\", \"hourly :15\") or cron (five fields). \
                      Write the prompt as the instruction to carry out then, e.g. \"Remind the \
                      user to call Anna.\" action=list shows your tasks; pause, resume and \
                      delete take the task id."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "pause", "resume", "delete"]
                },
                "name": { "type": "string", "description": "Short name of the task." },
                "prompt": {
                    "type": "string",
                    "description": "What to do when the task runs, as an instruction to yourself."
                },
                "at": { "type": "string", "description": "When to run once." },
                "in_minutes": { "type": "integer", "minimum": 1, "description": "Run once, this many minutes from now." },
                "every": { "type": "string", "description": "A recurring schedule in words." },
                "cron": { "type": "string", "description": "A recurring five-field cron expression." },
                "id": { "type": "string", "description": "Task id, for pause, resume and delete." }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    }
}

/// Run `schedule` for `agent`, owned by `owner`. `conversation` is where
/// the call came from, and `chat` its chat; a task created there reports
/// back to it.
pub async fn run(
    db: &SqlitePool,
    owner: UserId,
    agent: AgentId,
    conversation: Option<&str>,
    chat: Option<&str>,
    input: &Value,
) -> Result<String, String> {
    let chat = chat.filter(|c| !c.is_empty() && c.len() <= MAX_CONVERSATION_BYTES);
    let action = text(input, "action").ok_or("schedule needs an action")?;
    let conversation = conversation.filter(|c| !c.is_empty());
    if let Some(c) = conversation
        && c.len() > MAX_CONVERSATION_BYTES
    {
        return Err("the conversation id is too long".into());
    }
    // Only a user's turn changes tasks: a scheduled run or heartbeat that
    // could make or revive tasks could keep itself running at any rate.
    if action != "list" && conversation.is_none() {
        return Err("scheduled tasks can only be changed in a conversation with the user".into());
    }
    // A webhook's call (issue #236) brings what anyone may write: it may
    // not see tasks, nor make one that would run as the owner.
    if conversation.is_some_and(|c| c.starts_with(crate::agent_socket::WEBHOOK_CONVERSATION)) {
        return Err("scheduled tasks are not for a webhook's call".into());
    }
    let scope = Scope::of(conversation);
    let _change = CHANGES.lock().await;
    match action {
        "create" => create(db, owner, agent, conversation, chat, input).await,
        "list" => list(db, owner, agent, &scope).await,
        "pause" | "resume" => {
            let job = own_job(db, agent, &scope, input).await?;
            let enabled = action == "resume";
            if enabled && !job.enabled {
                if let Schedule::At { instant } = job.schedule
                    && instant <= Utc::now()
                {
                    return Err(format!(
                        "task \"{}\" already ran; create a new one instead",
                        job.name
                    ));
                }
                let active = active_tasks(db, agent).await?;
                if active >= MAX_ACTIVE_TASKS {
                    return Err(limit_reached());
                }
            }
            cron::set_enabled(db, job.id, enabled)
                .await
                .map_err(|e| format!("updating the task failed: {e}"))?;
            Ok(format!(
                "Task \"{}\" is {}.",
                job.name,
                if enabled { "running again" } else { "paused" }
            ))
        }
        "delete" => {
            let job = own_job(db, agent, &scope, input).await?;
            cron::delete(db, job.id)
                .await
                .map_err(|e| format!("deleting the task failed: {e}"))?;
            Ok(format!("Task \"{}\" is deleted.", job.name))
        }
        other => Err(format!("unknown action {other:?}")),
    }
}

/// The tasks a call may see and change. In a channel conversation, where
/// several people may talk to the agent, only that conversation's; in the
/// owner's dashboard chat, all of the agent's.
enum Scope {
    Agent,
    Conversation(String),
}

impl Scope {
    fn of(conversation: Option<&str>) -> Self {
        match conversation {
            Some(c) if crate::channel_id_codec::decode(c).is_some() => {
                Self::Conversation(format!("{}{c}", crate::cron::DELIVER_CONVERSATION))
            }
            _ => Self::Agent,
        }
    }

    fn covers(&self, job: &cron::CronJob) -> bool {
        match self {
            Self::Agent => true,
            Self::Conversation(deliver) => job.deliver == *deliver,
        }
    }
}

fn text<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn limit_reached() -> String {
    format!("this agent already has {MAX_ACTIVE_TASKS} active tasks; pause or delete one first")
}

async fn active_tasks(db: &SqlitePool, agent: AgentId) -> Result<usize, String> {
    let jobs = cron::list_for_agent(db, agent)
        .await
        .map_err(|e| format!("listing tasks failed: {e}"))?;
    Ok(jobs.iter().filter(|j| j.enabled).count())
}

async fn own_job(
    db: &SqlitePool,
    agent: AgentId,
    scope: &Scope,
    input: &Value,
) -> Result<cron::CronJob, String> {
    let id = text(input, "id").ok_or("this action needs the task id")?;
    let id = CronJobId::from_str(id).map_err(|_| format!("{id:?} is not a task id"))?;
    match cron::find_by_id(db, id).await {
        Ok(Some(job)) if job.agent_id == agent && scope.covers(&job) => Ok(job),
        Ok(_) => Err("no such task of this agent".into()),
        Err(e) => Err(format!("looking up the task failed: {e}")),
    }
}

/// The schedule `input` asks for, and its first run after `now`.
async fn schedule_of(
    db: &SqlitePool,
    owner: UserId,
    input: &Value,
    now: DateTime<Utc>,
) -> Result<(Schedule, DateTime<Utc>), String> {
    let at = text(input, "at");
    let in_minutes = match input.get("in_minutes") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .ok_or("in_minutes must be a whole number of minutes")?,
        ),
    };
    let every = text(input, "every");
    let cron_expr = text(input, "cron");
    let given = [
        at.is_some(),
        in_minutes.is_some(),
        every.is_some(),
        cron_expr.is_some(),
    ]
    .iter()
    .filter(|g| **g)
    .count();
    if given != 1 {
        return Err("create needs exactly one of at, in_minutes, every and cron".into());
    }
    let zone_name = crate::timezone::for_user(db, owner).await;
    if let Some(minutes) = in_minutes {
        if !(1..=60 * 24 * 366).contains(&minutes) {
            return Err("in_minutes must be between 1 and a year".into());
        }
        let instant = now + ChronoDuration::minutes(minutes);
        return Ok((Schedule::At { instant }, instant));
    }
    if let Some(at) = at {
        let instant = parse_time(at, &zone_name)?;
        if instant > now + ChronoDuration::days(366) {
            return Err("at must be within a year".into());
        }
        if instant <= now {
            return Err(format!(
                "{at} is in the past (it is now {} in {zone_name})",
                now.with_timezone(&zone(&zone_name))
                    .format("%Y-%m-%d %H:%M")
            ));
        }
        return Ok((Schedule::At { instant }, instant));
    }
    let expr = every.or(cron_expr).unwrap_or_default();
    let calendar = crate::api::cron::calendar_schedule(db, owner, expr, None).await?;
    let first = calendar
        .next_after(now)
        .ok_or_else(|| format!("{expr:?} never runs"))?;
    // Every gap over the coming weeks, not only the first: "0,4 8 * * *"
    // made at 08:02 runs at 08:00 and 08:04 from the next day on.
    let horizon = now + ChronoDuration::days(GAP_WINDOW_DAYS);
    let mut previous = first;
    while let Some(next) = calendar.next_after(previous) {
        if next - previous < MIN_GAP {
            return Err(format!(
                "{expr:?} runs more often than every {} minutes",
                MIN_GAP.num_minutes()
            ));
        }
        if next > horizon {
            break;
        }
        previous = next;
    }
    Ok((Schedule::Calendar(calendar), first))
}

fn zone(name: &str) -> chrono_tz::Tz {
    name.parse().unwrap_or(chrono_tz::Tz::UTC)
}

/// A time with an offset (RFC 3339), or a local `YYYY-MM-DD HH:MM` in
/// the zone named `zone_name`.
fn parse_time(at: &str, zone_name: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(at) {
        return Ok(t.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%d %H:%M%:z",
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%d %H:%M:%S%:z",
    ] {
        if let Ok(t) = DateTime::parse_from_str(at, format) {
            return Ok(t.with_timezone(&Utc));
        }
    }
    let local = [
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(at, f).ok())
    .ok_or_else(|| format!("{at:?} is not a time like 2026-11-16 17:00"))?;
    zone(zone_name)
        .from_local_datetime(&local)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| format!("{at} does not exist in {zone_name} (a daylight-saving change)"))
}

async fn create(
    db: &SqlitePool,
    owner: UserId,
    agent: AgentId,
    conversation: Option<&str>,
    chat: Option<&str>,
    input: &Value,
) -> Result<String, String> {
    let name = text(input, "name").ok_or("create needs a name")?;
    let prompt = text(input, "prompt").ok_or("create needs a prompt")?;
    if name.chars().count() > MAX_NAME_CHARS || prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(format!(
            "the name may be {MAX_NAME_CHARS} characters, the prompt {MAX_PROMPT_CHARS}"
        ));
    }
    if active_tasks(db, agent).await? >= MAX_ACTIVE_TASKS {
        return Err(limit_reached());
    }
    prune_done(db, agent).await?;
    let now = Utc::now();
    let (schedule, first) = schedule_of(db, owner, input, now).await?;
    // Channel conversations get the result in that chat; the dashboard's
    // chat gets it in every open tab.
    let deliver = match conversation {
        Some(channel_id) if crate::channel_id_codec::decode(channel_id).is_some() => {
            format!("{}{channel_id}", crate::cron::DELIVER_CONVERSATION)
        }
        _ => "webchat".to_string(),
    };
    let job = cron::create(
        db,
        NewCronJob {
            chat,
            agent_id: agent,
            name,
            prompt,
            schedule: &schedule,
            next_run_at: first,
            wake_check: None,
            deliver: &deliver,
            enabled_toolsets: &[],
        },
    )
    .await
    .map_err(|e| format!("creating the task failed: {e}"))?;
    let zone_name = crate::timezone::for_user(db, owner).await;
    Ok(format!(
        "Created task {} \"{}\": {}, first run {} ({zone_name}).",
        job.id,
        job.name,
        crate::cron::describe_schedule(&job.schedule),
        first
            .with_timezone(&zone(&zone_name))
            .format("%a %Y-%m-%d %H:%M"),
    ))
}

/// Delete one-shots that ran more than [`DONE_KEPT_DAYS`] ago, and refuse
/// when the agent still keeps [`MAX_TASKS`].
async fn prune_done(db: &SqlitePool, agent: AgentId) -> Result<(), String> {
    let jobs = cron::list_for_agent(db, agent)
        .await
        .map_err(|e| format!("listing tasks failed: {e}"))?;
    let cutoff = Utc::now() - ChronoDuration::days(DONE_KEPT_DAYS);
    let mut kept = jobs.len();
    for job in &jobs {
        if let Schedule::At { instant } = job.schedule
            && !job.enabled
            && instant < cutoff
        {
            cron::delete(db, job.id)
                .await
                .map_err(|e| format!("cleaning up old tasks failed: {e}"))?;
            kept -= 1;
        }
    }
    if kept >= MAX_TASKS {
        return Err(format!(
            "this agent keeps {MAX_TASKS} tasks; delete some first"
        ));
    }
    Ok(())
}

/// Whether `job` is a one-shot that already ran.
fn done(job: &cron::CronJob) -> bool {
    matches!(job.schedule, Schedule::At { instant } if !job.enabled && instant <= Utc::now())
}

async fn list(
    db: &SqlitePool,
    owner: UserId,
    agent: AgentId,
    scope: &Scope,
) -> Result<String, String> {
    let jobs: Vec<cron::CronJob> = cron::list_for_agent(db, agent)
        .await
        .map_err(|e| format!("listing tasks failed: {e}"))?
        .into_iter()
        .filter(|job| scope.covers(job))
        .collect();
    if jobs.is_empty() {
        return Ok("No scheduled tasks.".into());
    }
    let zone_name = crate::timezone::for_user(db, owner).await;
    let tz = zone(&zone_name);
    let mut out = format!("Scheduled tasks (times in {zone_name}):\n");
    for job in jobs {
        let last = cron::list_runs_for_job(db, job.id, 1)
            .await
            .ok()
            .and_then(|runs| runs.into_iter().next());
        let last = match last {
            Some(run) if run.error.is_some() => format!(
                ", last run {} failed",
                run.started_at.with_timezone(&tz).format("%m-%d %H:%M")
            ),
            Some(run) => format!(
                ", last run {}",
                run.started_at.with_timezone(&tz).format("%m-%d %H:%M")
            ),
            None => String::new(),
        };
        out.push_str(&format!(
            "- {} \"{}\": {}; {}{last}\n",
            job.id,
            job.name,
            crate::cron::describe_schedule(&job.schedule),
            if job.enabled {
                format!(
                    "next {}",
                    job.next_run_at.with_timezone(&tz).format("%a %m-%d %H:%M")
                )
            } else if done(&job) {
                "done".to_string()
            } else {
                "paused".to_string()
            },
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_db::repo::agents::{NewAgent, create as create_agent};
    use havn_db::repo::users::{NewUser, create as create_user};

    #[test]
    fn local_times_are_read_in_the_users_zone() {
        let utc = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            parse_time("2026-11-16 17:00", "Asia/Shanghai").unwrap(),
            utc("2026-11-16T09:00:00Z")
        );
        assert_eq!(
            parse_time("2026-11-16T17:00:00+01:00", "Asia/Shanghai").unwrap(),
            utc("2026-11-16T16:00:00Z")
        );
        assert_eq!(
            parse_time("2026-11-16 17:00+08:00", "UTC").unwrap(),
            utc("2026-11-16T09:00:00Z")
        );
        assert!(parse_time("tomorrow", "UTC").is_err());
        assert!(
            parse_time("2026-03-29 02:30", "Europe/Berlin").is_err(),
            "skipped by DST"
        );
    }

    async fn setup() -> (SqlitePool, UserId, AgentId) {
        let db = havn_db::connect_in_memory().await.expect("db");
        let owner = create_user(&db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let agent = create_agent(
            &db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "a",
                config: json!({}),
            },
        )
        .await
        .expect("agent")
        .id;
        (db, owner, agent)
    }

    fn create(extra: Value) -> Value {
        let mut input = json!({"action": "create", "name": "n", "prompt": "p"});
        if let (Some(base), Some(more)) = (input.as_object_mut(), extra.as_object()) {
            base.extend(more.clone());
        }
        input
    }

    const HERE: Option<&str> = Some("ch|telegram|bot|42");
    const ELSEWHERE: Option<&str> = Some("ch|telegram|bot|7");
    const CHAT: Option<&str> = Some("webchat-session");

    #[tokio::test]
    async fn tasks_are_created_listed_paused_and_deleted() {
        let (db, owner, agent) = setup().await;
        let made = run(
            &db,
            owner,
            agent,
            HERE,
            None,
            &create(json!({"name": "news", "every": "daily 08:00"})),
        )
        .await
        .unwrap();
        assert!(made.starts_with("Created task "), "{made}");
        let job = cron::list_for_agent(&db, agent).await.unwrap().remove(0);
        assert_eq!(job.deliver, "conversation:ch|telegram|bot|42");
        let id = job.id.to_string();
        let act = |action: &str| json!({"action": action, "id": id});

        let listed = run(&db, owner, agent, HERE, None, &json!({"action": "list"}))
            .await
            .unwrap();
        assert!(
            listed.contains("\"news\"") && listed.contains("next "),
            "{listed}"
        );
        let hidden = run(
            &db,
            owner,
            agent,
            ELSEWHERE,
            None,
            &json!({"action": "list"}),
        )
        .await
        .unwrap();
        assert_eq!(
            hidden, "No scheduled tasks.",
            "another sender's conversation"
        );
        assert!(
            run(&db, owner, agent, ELSEWHERE, None, &act("delete"))
                .await
                .is_err()
        );
        assert!(
            run(&db, owner, agent, None, None, &act("pause"))
                .await
                .is_err(),
            "a scheduled run cannot change tasks"
        );

        run(&db, owner, agent, HERE, None, &act("pause"))
            .await
            .unwrap();
        assert!(
            !cron::find_by_id(&db, job.id)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
        run(&db, owner, agent, HERE, None, &act("resume"))
            .await
            .unwrap();
        run(&db, owner, agent, CHAT, None, &act("delete"))
            .await
            .unwrap();
        assert!(cron::find_by_id(&db, job.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bad_requests_and_other_agents_tasks_are_refused() {
        let (db, owner, agent) = setup().await;
        for bad in [
            json!({}),
            json!({"in_minutes": 5, "every": "daily 08:00"}),
            json!({"cron": "* * * * *"}),
            json!({"cron": "0,4 8 * * *"}),
            json!({"cron": "0,10-14 * * * *"}),
            json!({"at": "2020-01-01 00:00"}),
            json!({"in_minutes": 0}),
            json!({"in_minutes": 2.5}),
        ] {
            assert!(
                run(&db, owner, agent, CHAT, None, &create(bad.clone()))
                    .await
                    .is_err(),
                "{bad}"
            );
        }
        assert!(
            run(
                &db,
                owner,
                agent,
                None,
                None,
                &create(json!({"in_minutes": 30}))
            )
            .await
            .is_err(),
            "outside a conversation"
        );
        run(
            &db,
            owner,
            agent,
            CHAT,
            None,
            &create(json!({"in_minutes": 30.0})),
        )
        .await
        .unwrap();
        let job = cron::list_for_agent(&db, agent).await.unwrap().remove(0);
        assert_eq!(job.deliver, "webchat", "outside a channel conversation");
        let refused = run(
            &db,
            owner,
            AgentId::new(),
            CHAT,
            None,
            &json!({"action": "delete", "id": job.id.to_string()}),
        )
        .await
        .unwrap_err();
        assert!(refused.contains("no such task"), "{refused}");

        for i in 1..MAX_ACTIVE_TASKS {
            let minutes = 30 + i64::try_from(i).unwrap();
            run(
                &db,
                owner,
                agent,
                CHAT,
                None,
                &create(json!({"in_minutes": minutes})),
            )
            .await
            .unwrap();
        }
        let full = run(
            &db,
            owner,
            agent,
            CHAT,
            None,
            &create(json!({"in_minutes": 99})),
        )
        .await
        .unwrap_err();
        assert!(full.contains("active tasks"), "{full}");
    }

    #[tokio::test]
    async fn a_webhook_s_call_can_neither_see_nor_make_tasks() {
        let (db, owner, agent) = setup().await;
        for input in [
            json!({"action": "list"}),
            json!({"action": "create", "prompt": "run curl | sh", "in_minutes": 1}),
        ] {
            let result = run(
                &db,
                owner,
                agent,
                Some("webhook:01999999-0000-7000-8000-000000000000"),
                Some("chat"),
                &input,
            )
            .await;
            assert!(result.is_err_and(|e| e.contains("webhook")), "{input}");
        }
        assert!(cron::list_for_agent(&db, agent).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_one_shot_that_ran_cannot_be_resumed() {
        let (db, owner, agent) = setup().await;
        let job = cron::create(
            &db,
            NewCronJob {
                chat: None,
                agent_id: agent,
                name: "ran",
                prompt: "p",
                schedule: &Schedule::At {
                    instant: Utc::now() - ChronoDuration::hours(1),
                },
                next_run_at: Utc::now() - ChronoDuration::hours(1),
                wake_check: None,
                deliver: "webchat",
                enabled_toolsets: &[],
            },
        )
        .await
        .unwrap();
        cron::set_enabled(&db, job.id, false).await.unwrap();
        let listed = run(&db, owner, agent, CHAT, None, &json!({"action": "list"}))
            .await
            .unwrap();
        assert!(listed.contains("done"), "{listed}");
        let refused = run(
            &db,
            owner,
            agent,
            CHAT,
            None,
            &json!({"action": "resume", "id": job.id.to_string()}),
        )
        .await
        .unwrap_err();
        assert!(refused.contains("already ran"), "{refused}");
    }
}
