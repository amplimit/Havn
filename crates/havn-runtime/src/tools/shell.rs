//! `shell` and `process` tools — run commands in the agent's environment.
//!
//! Under `NamespaceSpawner` the shell runs inside the agent's namespaces,
//! cgroup, Landlock and seccomp confinement (spec §4.1), in the Landlock
//! domain of tool processes (`sandbox::confine_tool`). Under the dev-only
//! `SubprocessSpawner` it inherits whatever the runtime can do on the
//! host.
//!
//! A command runs for the policy's `shell.timeout_seconds`, or as long as
//! it asks up to `shell.max_timeout_seconds` (issue #215); on a timeout
//! what it printed so far comes back. One started with `background: true`
//! goes on after the call, as a process the `process` tool lists, reads,
//! waits on and kills; the turn's stop ends the ones it started.
//! Commands get the agent's shell environment, `secret:<name>` resolved
//! (issue #233).

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};
use tokio::sync::watch;

use super::{Tool, ToolCtx, ToolResult};

/// Output kept of a command: its first bytes and its last.
const HEAD_BYTES: usize = 192 * 1024;
const TAIL_BYTES: usize = 64 * 1024;
/// How long output is still read after a command ended: a process it left
/// running may hold its pipes open.
const DRAIN: Duration = Duration::from_secs(2);
/// Ended background commands kept for their output to be read.
const ENDED_KEPT: usize = 16;
/// How long the last lines a shell printed as it ended are waited for, when
/// what it left running holds its output open.
const LAST_LINES: Duration = Duration::from_millis(200);
/// How often a background command whose first process ended is checked
/// for others of its group still running.
const GROUP_POLL: Duration = Duration::from_secs(1);
/// Seconds `process wait` waits when it does not say.
const DEFAULT_WAIT_SECONDS: u64 = 30;
/// Characters of a command shown in a list.
const COMMAND_SHOWN: usize = 80;

pub struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the agent's environment, with the workspace as the working \
         directory: `bash -c` when bash is installed, else `sh -c`. It runs for up to \
         `timeout_seconds` (300 by default, at most the policy's limit, 1800 unless set); \
         stdout and stderr come back as they were printed (the first 192 KB and the last \
         64 KB) with the exit code, and on a timeout what it printed so far. With \
         `background: true` it goes on after the call (a server, a long build): you get a \
         process id to read, wait on or kill with the `process` tool. The variables your \
         owner set for you are in its environment; the value of a secret shows as \
         [secret:<name>] in what it prints."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command." },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "How long it may run; 300 when not given."
                },
                "background": {
                    "type": "boolean",
                    "description": "Go on after the call, as a process of the `process` tool."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: command".into());
        };
        if command.trim().is_empty() {
            return ToolResult::Error("command must be non-empty".into());
        }
        let limits = ctx.policy.shell;
        let most = u64::from(limits.max_timeout_seconds.max(1));
        let seconds = input
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| u64::from(limits.timeout_seconds.max(1)))
            .clamp(1, most);
        let background = input
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // What ends a background command: the stop of the turn that started
        // it. A heartbeat, a cron job or another agent's question has none.
        let stop = crate::tool_loop::TURN_STOP.try_with(Clone::clone).ok();
        if background && stop.is_none() {
            return ToolResult::Error(
                "background commands are for conversations, whose stop ends them; run it in \
                 the foreground with a timeout_seconds"
                    .into(),
            );
        }

        let mut cmd = Command::new(shell_program());
        cmd.arg("-c")
            .arg(command)
            .current_dir(&ctx.workspace_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // A group of its own, so a timeout or a stopped turn ends every
        // process the command started, not only the shell.
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(target_os = "linux")]
        crate::sandbox::confine_tool(&mut cmd);
        crate::secrets::apply_shell_env(&mut cmd);

        if background {
            // Counted and started under one lock: loops running at once
            // cannot together go past the limit.
            let mut procs = lock(&PROCS);
            if procs.running() >= limits.background.max(1) as usize {
                return ToolResult::Error(format!(
                    "{} commands are running in the background already, the most allowed: end \
                     one with the process tool (action \"kill\") first",
                    limits.background.max(1)
                ));
            }
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => return ToolResult::Error(format!("exec failed: {e}")),
            };
            let (output, readers) = capture(&mut child);
            let id = procs.watch(
                command,
                child.id(),
                First::Running(child),
                output,
                readers,
                stop,
                0,
            );
            return ToolResult::Ok(format!(
                "started in the background as process {id}. Read what it prints with the \
                 process tool ({{\"action\": \"read\", \"id\": {id}}}), wait for it with \
                 \"wait\", end it with \"kill\"."
            ));
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return ToolResult::Error(format!("exec failed: {e}")),
        };
        let (output, readers) = capture(&mut child);
        let pid = child.id();
        let mut group = GroupKill(pid);
        let waited = tokio::time::timeout(Duration::from_secs(seconds), child.wait()).await;
        match waited {
            Ok(Ok(status)) => {
                group.0 = None;
                let mut out = format!("exit={}\n", exit_text(Some(status)));
                // What it left running (a server started with `&`) goes on
                // as a background process, so the turn's stop, `kill` and
                // the limit reach it.
                if group_alive(pid) {
                    // The shell's last lines, printed as it ended.
                    tokio::time::sleep(LAST_LINES).await;
                    let mut procs = lock(&PROCS);
                    let full = procs.running() >= limits.background.max(1) as usize;
                    match stop {
                        Some(stop) if !full => {
                            let from = lock(&output).total;
                            let id = procs.watch(
                                command,
                                pid,
                                First::Ended(status),
                                Arc::clone(&output),
                                readers,
                                Some(stop),
                                from,
                            );
                            let _ = writeln!(
                                out,
                                "[It left processes running: they go on as background process \
                                 {id}; read or end them with the process tool.]"
                            );
                        }
                        Some(_) => {
                            kill_group(pid);
                            let _ = writeln!(
                                out,
                                "[It left processes running; they were ended, as {} commands \
                                 run in the background already, the most allowed.]",
                                limits.background.max(1)
                            );
                        }
                        None => {
                            kill_group(pid);
                            let _ = writeln!(
                                out,
                                "[It left processes running; they were ended, as nothing here \
                                 would end them later.]"
                            );
                        }
                    }
                } else {
                    let _ = tokio::time::timeout(DRAIN, readers).await;
                }
                out.push_str(&printed(&output));
                if status.success() {
                    ToolResult::Ok(out)
                } else {
                    // Non-zero exit is still "the tool ran"; an error, so
                    // the model sees `is_error=true` and can react.
                    ToolResult::Error(out)
                }
            }
            Ok(Err(e)) => ToolResult::Error(format!("exec failed: {e}")),
            Err(_) => {
                // Out of time: it and what it started end, and what it
                // printed so far comes back.
                drop(group);
                let _ = tokio::time::timeout(DRAIN, child.wait()).await;
                let _ = tokio::time::timeout(DRAIN, readers).await;
                ToolResult::Error(format!(
                    "timed out after {seconds}s and was stopped. Run it with a longer \
                     timeout_seconds (at most {most}), or with background: true. What it printed \
                     so far:\n{}",
                    printed(&output)
                ))
            }
        }
    }
}

/// The background commands, by id: the `process` tool's.
pub struct ProcessTool;

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &'static str {
        "process"
    }

    fn description(&self) -> &'static str {
        "Commands started with the shell tool's `background: true`. `list` names them; `read` \
         gives what one printed since the last read, and whether it is still running; `wait` \
         waits up to `seconds` (30 by default) for it to end, then reads; `kill` ends it and \
         what it started. A stopped reply ends the commands it started."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "read", "wait", "kill"] },
                "id": { "type": "integer", "description": "The process, for read, wait and kill." },
                "seconds": { "type": "integer", "description": "How long wait waits." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        if action == "list" {
            return ToolResult::Ok(list());
        }
        let Some(id) = input
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
        else {
            return ToolResult::Error(format!("{action:?} needs the process's id"));
        };
        let Some((ended, pid)) = with_proc(id, |p| (p.ended.clone(), p.pid)) else {
            return ToolResult::Error(format!(
                "there is no process {id}; `list` names the ones there are"
            ));
        };
        match action {
            "read" => ToolResult::Ok(read(id)),
            "wait" => {
                let most = u64::from(ctx.policy.shell.max_timeout_seconds.max(1));
                let seconds = input
                    .get("seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_WAIT_SECONDS)
                    .clamp(1, most);
                let mut ended = ended;
                let _ = tokio::time::timeout(
                    Duration::from_secs(seconds),
                    ended.wait_for(Option::is_some),
                )
                .await;
                ToolResult::Ok(read(id))
            }
            "kill" => {
                // The whole group, whatever its first process did: what it
                // started may still run. Not one that ended, all of it:
                // its id may be another's by now.
                if ended.borrow().is_none() {
                    kill_group(pid);
                }
                let mut ended = ended;
                let _ = tokio::time::timeout(DRAIN * 2, ended.wait_for(Option::is_some)).await;
                ToolResult::Ok(read(id))
            }
            other => ToolResult::Error(format!(
                "unknown action {other:?}: list, read, wait or kill"
            )),
        }
    }
}

/// bash where it is installed, else sh: the path run.
fn shell_program() -> &'static str {
    static FOUND: LazyLock<&'static str> = LazyLock::new(|| {
        ["/bin/bash", "/usr/bin/bash"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .unwrap_or("/bin/sh")
    });
    &FOUND
}

/// What a command printed, stdout and stderr as they came: its first
/// [`HEAD_BYTES`], and its last [`TAIL_BYTES`] past them.
#[derive(Default)]
struct Output {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
}

impl Output {
    fn push(&mut self, bytes: &[u8]) {
        let room = HEAD_BYTES.saturating_sub(self.head.len()).min(bytes.len());
        let (first, rest) = bytes.split_at(room);
        self.head.extend_from_slice(first);
        self.tail.extend(rest);
        let excess = self.tail.len().saturating_sub(TAIL_BYTES);
        self.tail.drain(..excess);
        self.total += bytes.len() as u64;
    }

    /// What it printed from byte `from` on, saying where bytes were not
    /// kept, and where the next read starts. With `more` (the command still
    /// runs), the end is held back when it could be the start of one of the
    /// agent's secrets, or of a character, until the rest comes; where
    /// bytes were not kept, a start or end of a secret beside the gap is
    /// left out (issue #233). Redaction of whole values comes after.
    fn since(&self, from: u64, more: bool) -> (String, u64) {
        let head_len = self.head.len() as u64;
        let tail_start = self.total - self.tail.len() as u64;
        let skipped = tail_start.saturating_sub(from.max(head_len));
        let mut head: &[u8] = if from < head_len {
            &self.head[usize::try_from(from).unwrap_or(0)..]
        } else {
            &[]
        };
        let skip = usize::try_from(from.max(tail_start) - tail_start).unwrap_or(usize::MAX);
        let mut rest: Vec<u8> = self.tail.iter().skip(skip).copied().collect();
        if skipped > 0 {
            head = &head[..crate::secrets::cut_end(head)];
            rest.drain(..crate::secrets::cut_start(&rest));
        }
        let mut next = self.total;
        if more {
            if rest.is_empty() && skipped == 0 {
                let shown = crate::secrets::emit_len(head);
                next -= (head.len() - shown) as u64;
                head = &head[..shown];
            } else {
                let shown = crate::secrets::emit_len(&rest);
                next -= (rest.len() - shown) as u64;
                rest.truncate(shown);
            }
        }
        let mut text = String::from_utf8_lossy(head).into_owned();
        if skipped > 0 {
            let _ = write!(text, "\n[... {skipped} bytes of output not kept ...]\n");
        }
        text.push_str(&String::from_utf8_lossy(&rest));
        (text, next)
    }
}

/// All `output` holds.
fn printed(output: &Arc<Mutex<Output>>) -> String {
    lock(output).since(0, false).0
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read what `child` prints into an [`Output`], until its pipes close.
fn capture(child: &mut Child) -> (Arc<Mutex<Output>>, tokio::task::JoinHandle<()>) {
    let output = Arc::new(Mutex::new(Output::default()));
    let mut readers = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        readers.push(read_into(pipe, Arc::clone(&output)));
    }
    if let Some(pipe) = child.stderr.take() {
        readers.push(read_into(pipe, Arc::clone(&output)));
    }
    let done = tokio::spawn(async move {
        for reader in readers {
            let _ = reader.await;
        }
    });
    (output, done)
}

fn read_into<R>(mut pipe: R, output: Arc<Mutex<Output>>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0_u8; 16 * 1024];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => lock(&output).push(&buf[..n]),
            }
        }
    })
}

fn exit_text(status: Option<std::process::ExitStatus>) -> String {
    match status.and_then(|s| s.code()) {
        Some(code) => code.to_string(),
        None => "(killed by signal)".into(),
    }
}

/// The first process of a background command: still running, or ended
/// (its shell, which left others running).
enum First {
    Running(Child),
    Ended(std::process::ExitStatus),
}

/// A command going on in the background.
struct Proc {
    command: String,
    started: Instant,
    output: Arc<Mutex<Output>>,
    /// Where the next read starts.
    read: u64,
    pid: Option<u32>,
    /// Its exit, once it ended.
    ended: watch::Receiver<Option<String>>,
}

#[derive(Default)]
struct Procs {
    next: u32,
    all: BTreeMap<u32, Proc>,
}

static PROCS: LazyLock<Mutex<Procs>> = LazyLock::new(Mutex::default);

impl Procs {
    /// Commands still running: their first process, or any it started.
    fn running(&self) -> usize {
        self.all
            .values()
            .filter(|p| p.ended.borrow().is_none())
            .count()
    }

    /// Keep the command `command`, whose process group `pid` leads, going
    /// in the background, `first` its first process: it counts as ended
    /// once all of its group is gone, and what it printed is read
    /// (`readers`) by then. The turn's stop, when there is one, ends it.
    /// Reads start at byte `from`. Its id.
    #[allow(clippy::too_many_arguments)]
    fn watch(
        &mut self,
        command: &str,
        pid: Option<u32>,
        first: First,
        output: Arc<Mutex<Output>>,
        readers: tokio::task::JoinHandle<()>,
        stop: Option<tokio_util::sync::CancellationToken>,
        from: u64,
    ) -> u32 {
        let (ended_tx, ended) = watch::channel(None);
        let (child, status) = match first {
            First::Running(child) => (Some(child), None),
            First::Ended(status) => (None, Some(status)),
        };
        tokio::spawn(async move {
            let mut child = child;
            let mut status = status;
            let mut stopped = false;
            loop {
                if child.is_none() && !group_alive(pid) {
                    break;
                }
                let stopping = async {
                    match &stop {
                        Some(stop) if !stopped => stop.cancelled().await,
                        _ => std::future::pending().await,
                    }
                };
                tokio::select! {
                    waited = async {
                        match child.as_mut() {
                            Some(first) => first.wait().await.ok(),
                            None => std::future::pending().await,
                        }
                    }, if child.is_some() => {
                        status = waited;
                        child = None;
                    }
                    () = stopping => {
                        stopped = true;
                        kill_group(pid);
                    }
                    () = tokio::time::sleep(GROUP_POLL), if child.is_none() => {}
                }
            }
            let _ = tokio::time::timeout(DRAIN, readers).await;
            let _ = ended_tx.send(Some(exit_text(status)));
        });
        self.next += 1;
        let id = self.next;
        self.all.insert(
            id,
            Proc {
                command: command.to_string(),
                started: Instant::now(),
                output,
                read: from,
                pid,
                ended,
            },
        );
        // The oldest ended ones go.
        let ended: Vec<u32> = self
            .all
            .iter()
            .filter(|(_, p)| p.ended.borrow().is_some())
            .map(|(id, _)| *id)
            .collect();
        for old in ended.iter().take(ended.len().saturating_sub(ENDED_KEPT)) {
            self.all.remove(old);
        }
        id
    }
}

fn with_proc<T>(id: u32, f: impl FnOnce(&mut Proc) -> T) -> Option<T> {
    lock(&PROCS).all.get_mut(&id).map(f)
}

/// What process `id` printed since the last read, and how it is.
fn read(id: u32) -> String {
    with_proc(id, |p| {
        let more = p.ended.borrow().is_none();
        let (text, next) = lock(&p.output).since(p.read, more);
        p.read = next;
        let state = match p.ended.borrow().as_ref() {
            Some(exit) => format!("ended, exit={exit}"),
            None => format!("running for {}s", p.started.elapsed().as_secs()),
        };
        if text.is_empty() {
            format!("process {id}: {state}; nothing new printed")
        } else {
            format!("process {id}: {state}; printed since the last read:\n{text}")
        }
    })
    .unwrap_or_else(|| format!("there is no process {id}"))
}

fn list() -> String {
    let procs = lock(&PROCS);
    if procs.all.is_empty() {
        return "No background commands.".into();
    }
    let mut out = String::new();
    for (id, p) in &procs.all {
        let state = match p.ended.borrow().as_ref() {
            Some(exit) => format!("ended, exit={exit}"),
            None => format!("running for {}s", p.started.elapsed().as_secs()),
        };
        let mut command: String = p.command.chars().take(COMMAND_SHOWN).collect();
        if p.command.chars().count() > COMMAND_SHOWN {
            command.push('…');
        }
        let _ = writeln!(out, "{id}: {state}: {command}");
    }
    out
}

/// Whether a process of the group `pid` leads still runs, a zombie not
/// counted (its parent may be slow to reap it). A group's id is not given
/// to another process while any of its own is alive.
fn group_alive(pid: Option<u32>) -> bool {
    let Some(pgid) = pid else {
        return false;
    };
    #[cfg(target_os = "linux")]
    if let Ok(dir) = std::fs::read_dir("/proc") {
        return dir.flatten().any(|entry| {
            let name = entry.file_name();
            let Some(n) = name
                .to_str()
                .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
            else {
                return false;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{n}/stat")) else {
                return false;
            };
            // After the command's name, in parentheses: state, ppid, pgrp.
            let Some((_, rest)) = stat.rsplit_once(')') else {
                return false;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next().unwrap_or("Z");
            let group = fields.nth(1).and_then(|g| g.parse::<u32>().ok());
            state != "Z" && state != "X" && group == Some(pgid)
        });
    }
    #[cfg(unix)]
    if let Ok(pgid) = i32::try_from(pgid) {
        // SAFETY: kill with signal 0 sends nothing; it only checks.
        return unsafe { libc::kill(-pgid, 0) } == 0;
    }
    false
}

/// End the process group `pid` leads.
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: kill has no memory-safety preconditions. The group is
        // the command's own (process_group(0)); once all of it has exited
        // the call fails with ESRCH and does nothing.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

/// Kills the process group `0` leads when dropped, unless cleared: when a
/// command times out, or its turn is stopped and the call is dropped.
struct GroupKill(Option<u32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::Policy;
    use std::sync::Arc;

    async fn ctx() -> ToolCtx {
        let pool = havn_db::agent::connect_in_memory().await.expect("db");
        let workspace = std::env::temp_dir().join(format!("havn-shell-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&workspace).await.expect("ws");
        ToolCtx {
            workspace_dir: workspace,
            agent_db: pool,
            policy: Arc::new(Policy::default()),
            embedder: None,
        }
    }

    #[tokio::test]
    async fn ok_when_command_succeeds() {
        let r = ShellTool
            .execute(&ctx().await, json!({"command": "echo hi"}))
            .await;
        assert!(!r.is_error(), "{}", r.text());
        assert!(r.text().contains("exit=0"));
        assert!(r.text().contains("hi"));
    }

    #[tokio::test]
    async fn error_when_command_fails() {
        let r = ShellTool
            .execute(&ctx().await, json!({"command": "false"}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("exit=1"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_a_call_kills_what_the_command_started() {
        let ctx = ctx().await;
        let call = ShellTool.execute(
            &ctx,
            json!({"command": "sleep 30 & echo $! > bg.pid; wait"}),
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1500), call)
                .await
                .is_err(),
            "the command waits on its background sleep"
        );
        let pid = std::fs::read_to_string(ctx.workspace_dir.join("bg.pid")).expect("pid file");
        let stat = format!("/proc/{}/stat", pid.trim());
        let mut gone = false;
        for _ in 0..40 {
            // Gone, or a zombie waiting for a reaper that is not ours.
            gone = std::fs::read_to_string(&stat).map_or(true, |s| {
                s.rsplit(')')
                    .next()
                    .is_some_and(|rest| rest.trim_start().starts_with('Z'))
            });
            if gone {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(gone, "the background sleep outlived its stopped call");
    }

    #[tokio::test]
    async fn empty_command_rejected() {
        let r = ShellTool
            .execute(&ctx().await, json!({"command": "   "}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("non-empty"));
    }

    fn id_of(started: &str) -> u64 {
        let rest = started.split("process ").nth(1).expect("an id");
        rest.split('.')
            .next()
            .and_then(|n| n.trim().parse().ok())
            .expect("a number")
    }

    #[tokio::test]
    async fn a_timed_out_command_returns_what_it_printed() {
        let r = ShellTool
            .execute(
                &ctx().await,
                json!({"command": "echo started; sleep 30", "timeout_seconds": 1}),
            )
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("timed out after 1s"), "{}", r.text());
        assert!(r.text().contains("started"), "{}", r.text());
    }

    #[tokio::test]
    async fn a_background_command_is_read_waited_on_and_killed() {
        let ctx = ctx().await;
        let stop = tokio_util::sync::CancellationToken::new();
        let started = crate::tool_loop::TURN_STOP
            .scope(
                stop,
                ShellTool.execute(
                    &ctx,
                    json!({"command": "echo ready; sleep 30", "background": true}),
                ),
            )
            .await;
        assert!(!started.is_error(), "{}", started.text());
        let id = id_of(started.text());
        let mut seen = String::new();
        for _ in 0..50 {
            seen.push_str(
                ProcessTool
                    .execute(&ctx, json!({"action": "read", "id": id}))
                    .await
                    .text(),
            );
            if seen.contains("ready") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(seen.contains("ready"), "{seen}");
        assert!(seen.contains("running"), "{seen}");
        let again = ProcessTool
            .execute(&ctx, json!({"action": "read", "id": id}))
            .await;
        assert!(again.text().contains("nothing new"), "{}", again.text());
        assert!(
            ProcessTool
                .execute(&ctx, json!({"action": "list"}))
                .await
                .text()
                .contains("sleep 30")
        );
        let killed = ProcessTool
            .execute(&ctx, json!({"action": "kill", "id": id}))
            .await;
        assert!(killed.text().contains("ended"), "{}", killed.text());
        let unknown = ProcessTool
            .execute(&ctx, json!({"action": "read", "id": 999_999}))
            .await;
        assert!(unknown.is_error());
    }

    #[tokio::test]
    async fn background_needs_a_turn_to_end_it() {
        let r = ShellTool
            .execute(
                &ctx().await,
                json!({"command": "sleep 1", "background": true}),
            )
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("conversations"), "{}", r.text());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn what_a_command_leaves_running_is_tracked_and_stopped() {
        let ctx = ctx().await;
        let stop = tokio_util::sync::CancellationToken::new();
        let r = crate::tool_loop::TURN_STOP
            .scope(
                stop.clone(),
                ShellTool.execute(
                    &ctx,
                    json!({"command": "sleep 30 > /dev/null 2>&1 & echo left"}),
                ),
            )
            .await;
        assert!(r.text().contains("background process"), "{}", r.text());
        let after = r.text().split("background process").nth(1).unwrap_or("");
        let id: u64 = after
            .trim_start()
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse().ok())
            .expect("an id");
        let running = ProcessTool
            .execute(&ctx, json!({"action": "read", "id": id}))
            .await;
        assert!(running.text().contains("running"), "{}", running.text());
        stop.cancel();
        let waited = ProcessTool
            .execute(&ctx, json!({"action": "wait", "id": id, "seconds": 5}))
            .await;
        assert!(waited.text().contains("ended"), "{}", waited.text());
    }

    #[tokio::test]
    async fn a_stopped_turn_ends_the_commands_it_started() {
        let ctx = ctx().await;
        let stop = tokio_util::sync::CancellationToken::new();
        let started = crate::tool_loop::TURN_STOP
            .scope(
                stop.clone(),
                ShellTool.execute(&ctx, json!({"command": "sleep 30", "background": true})),
            )
            .await;
        let id = id_of(started.text());
        stop.cancel();
        let waited = ProcessTool
            .execute(&ctx, json!({"action": "wait", "id": id, "seconds": 5}))
            .await;
        assert!(waited.text().contains("ended"), "{}", waited.text());
    }

    #[tokio::test]
    async fn commands_run_in_bash_when_it_is_there() {
        if !shell_program().ends_with("bash") {
            return;
        }
        let r = ShellTool
            .execute(
                &ctx().await,
                json!({"command": "[[ 1 == 1 ]] && echo bashy"}),
            )
            .await;
        assert!(r.text().contains("bashy"), "{}", r.text());
    }

    #[test]
    fn long_output_keeps_its_start_and_its_end() {
        let mut output = Output::default();
        output.push(b"START");
        output.push(&vec![b'x'; HEAD_BYTES + TAIL_BYTES]);
        output.push(b"END");
        let (all, next) = output.since(0, false);
        assert!(all.starts_with("START"));
        assert!(all.ends_with("END"));
        assert!(all.contains("bytes of output not kept"));
        assert_eq!(next, output.total);
        assert_eq!(output.since(next, false).0, "");
    }

    #[test]
    fn a_secret_printed_in_pieces_is_read_whole() {
        let _one = crate::secrets::tests::SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::secrets::set(havn_proto::Secrets {
            values: BTreeMap::from([("split".to_string(), "split-secret-0123".to_string())]),
            shell_env: BTreeMap::new(),
        });
        // Read while it runs: the start of the value waits for its rest.
        let mut output = Output::default();
        output.push(b"token: split-sec");
        let (first, next) = output.since(0, true);
        assert_eq!(first, "token: ");
        output.push(b"ret-0123 done");
        let (second, _) = output.since(next, true);
        assert_eq!(crate::secrets::redact(&second), "[secret:split] done");
        // A character cut in two waits for its other half too.
        let mut output = Output::default();
        output.push(&"caf\u{e9}".as_bytes()[..4]);
        let (first, next) = output.since(0, true);
        assert_eq!((first.as_str(), next), ("caf", 3));
        output.push(&"\u{e9}".as_bytes()[1..]);
        assert_eq!(output.since(next, true).0, "\u{e9}");
        // Where output was not kept, no piece of the value beside the gap.
        let mut output = Output::default();
        output.push(&vec![b'x'; HEAD_BYTES - 6]);
        output.push(b"split-secret-0123");
        output.push(&vec![b'y'; TAIL_BYTES + 100]);
        output.push(b"split-secret-0123 end");
        let all = output.since(0, false).0;
        let (before, after) = all.split_once("not kept").expect("a gap");
        assert!(!before.contains("split"), "the start beside the gap is shown");
        assert!(crate::secrets::redact(after).ends_with("[secret:split] end"));
        crate::secrets::set(havn_proto::Secrets::default());
    }

    #[tokio::test]
    async fn captures_stderr() {
        let ctx = ctx().await;
        let r = ShellTool
            .execute(&ctx, json!({"command": "echo to-out; echo to-err 1>&2"}))
            .await;
        assert!(r.text().contains("to-out"));
        assert!(r.text().contains("to-err"));
    }
}
