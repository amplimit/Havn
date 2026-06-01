//! `havn gateway {start,stop,restart,status}` — gateway lifecycle.
//!
//! Foreground mode (`start` without `--detach`) preserves the legacy
//! `havn start` behaviour: `exec` directly into `havn-gateway`, signals
//! reach it, no PID/log files involved.
//!
//! Detached mode (`start --detach`) double-forks via `setsid` so the
//! gateway survives the calling shell, redirects stdout/stderr into
//! `${data_dir}/gateway.log`, and writes its PID to `${data_dir}/gateway.pid`
//! for `stop` / `status` to discover. We don't try to be a fancy
//! supervisor — that's systemd / docker / pm2's job. Detached mode is
//! a single-server-quick-start convenience, not a production manager.

use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};

use crate::paths;
use crate::start::resolve_gateway_bin;

/// How long to wait for the detached gateway to bind its listen socket
/// before we declare the launch failed.
const DETACH_BIND_TIMEOUT: Duration = Duration::from_secs(10);

/// Graceful-stop budget. SIGTERM, then SIGKILL when this elapses.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn start(detach: bool) -> anyhow::Result<()> {
    if detach {
        start_detached()
    } else {
        crate::start::run()
    }
}

fn start_detached() -> anyhow::Result<()> {
    let cfg = paths::load_config().unwrap_or_default();
    let bin = resolve_gateway_bin()?;
    let pid_path = paths::pid_file(&cfg);
    let log_path = paths::log_file(&cfg);
    let data = paths::data_dir(&cfg);

    // Refuse to launch a second gateway if the PID file points at a live
    // process. Stale (process gone) PID files are harmless — we overwrite.
    if let Some(pid) = paths::read_pid(&pid_path)? {
        if paths::pid_alive(pid) {
            return Err(anyhow!(
                "gateway already running (pid {pid}, see {})",
                pid_path.display()
            ));
        }
        eprintln!(
            "note: removing stale pid file {} (pid {pid} no longer running)",
            pid_path.display()
        );
        let _ = std::fs::remove_file(&pid_path);
    }

    std::fs::create_dir_all(&data)
        .with_context(|| format!("create data dir {}", data.display()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open log file {}", log_path.display()))?;

    // Double-fork so the gateway is reparented to init and survives this
    // shell. We use std::process::Command for the actual exec (it already
    // dups the file descriptors for us); the parent waits long enough to
    // confirm the gateway bound its listen socket.
    use std::os::unix::process::CommandExt as _;
    let log_for_stderr = log
        .try_clone()
        .with_context(|| format!("dup log fd for {}", log_path.display()))?;

    // SAFETY: `setsid()` is async-signal-safe; we call it after `fork()`
    // inside `pre_exec` to detach from the controlling terminal.
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(std::env::args().skip(3))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_for_stderr));
    unsafe {
        cmd.pre_exec(|| {
            // New session → no controlling terminal → SIGHUP from the
            // calling shell can't reach us. `setsid` returns -1 only if
            // already a session leader (won't happen post-fork).
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    let pid = child.id();

    // Persist PID for `stop` / `status`. Best-effort write; if it fails
    // the gateway is still running, just unmanageable via the CLI.
    if let Err(e) = std::fs::write(&pid_path, format!("{pid}\n")) {
        eprintln!(
            "warning: could not write {} ({e}); use kill {pid} to stop",
            pid_path.display()
        );
    }

    // Wait until the listen socket is up OR the process exits early.
    // Both conditions are observable without inter-process signalling:
    // - `pid_alive(pid)` flips false when the gateway aborts on a config /
    //   env error (loud message lands in the log file).
    // - `tcp_probe` succeeds once axum is bound.
    let listen = paths::listen_addr(&cfg);
    let deadline = Instant::now() + DETACH_BIND_TIMEOUT;
    #[allow(clippy::cast_possible_wrap, reason = "child pids fit i32 on Linux")]
    let pid_i32 = pid as i32;
    while Instant::now() < deadline {
        if !paths::pid_alive(pid_i32) {
            return Err(anyhow!(
                "gateway exited shortly after launch — check {}",
                log_path.display()
            ));
        }
        if tcp_probe(&listen) {
            println!(
                "gateway started (pid {pid}, listening on {listen}, log {})",
                log_path.display()
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(anyhow!(
        "gateway didn't bind {listen} within {:?} — check {}",
        DETACH_BIND_TIMEOUT,
        log_path.display()
    ))
}

pub fn stop() -> anyhow::Result<()> {
    let cfg = paths::load_config().unwrap_or_default();
    let pid_path = paths::pid_file(&cfg);
    let Some(pid) = paths::read_pid(&pid_path)? else {
        return Err(anyhow!(
            "no PID file at {} — gateway either never started in detached mode or already stopped",
            pid_path.display()
        ));
    };
    if !paths::pid_alive(pid) {
        eprintln!("note: pid {pid} not running; cleaning up stale PID file");
        let _ = std::fs::remove_file(&pid_path);
        return Ok(());
    }

    // SAFETY: kill(pid, SIGTERM) — no memory passed. Errors via errno.
    let r = unsafe { libc::kill(pid, libc::SIGTERM) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("SIGTERM pid {pid}"));
    }
    println!(
        "sent SIGTERM to gateway pid {pid}, waiting up to {:?}…",
        STOP_TIMEOUT
    );

    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if !paths::pid_alive(pid) {
            let _ = std::fs::remove_file(&pid_path);
            println!("gateway stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    eprintln!("gateway didn't exit within {STOP_TIMEOUT:?}; sending SIGKILL");
    // SAFETY: see above.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    // Brief grace for the kernel to reap.
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_file(&pid_path);
    if paths::pid_alive(pid) {
        Err(anyhow!("pid {pid} survived SIGKILL — escalate manually"))
    } else {
        println!("gateway killed");
        Ok(())
    }
}

pub fn restart(detach: bool) -> anyhow::Result<()> {
    // Tolerate "no PID file" — the operator may want restart-as-start.
    if let Err(e) = stop() {
        eprintln!("note: stop step skipped ({e})");
    }
    start(detach)
}

pub fn status() -> anyhow::Result<()> {
    let cfg = paths::load_config().unwrap_or_default();
    let pid_path = paths::pid_file(&cfg);
    let listen = paths::listen_addr(&cfg);

    match paths::read_pid(&pid_path)? {
        Some(pid) if paths::pid_alive(pid) => {
            let uptime = uptime_for_pid(pid).unwrap_or_default();
            println!("gateway: running");
            println!("  pid:    {pid}");
            println!("  listen: {listen}");
            println!("  uptime: {uptime}");
            println!("  log:    {}", paths::log_file(&cfg).display());
            println!("  pid_file: {}", pid_path.display());
        }
        Some(pid) => {
            println!(
                "gateway: not running (stale pid {pid} in {})",
                pid_path.display()
            );
        }
        None => {
            // Could still be running in foreground mode — best-effort probe.
            if tcp_probe(&listen) {
                println!(
                    "gateway: reachable on {listen} (no pid file — likely foreground / external supervisor)"
                );
            } else {
                println!("gateway: not running");
            }
        }
    }
    Ok(())
}

/// Best-effort TCP connect probe. We're not validating that the gateway
/// actually answered — just that *something* accepts on the port. The
/// caller chases the API endpoint for richer health if needed.
fn tcp_probe(addr: &str) -> bool {
    match std::net::ToSocketAddrs::to_socket_addrs(&addr) {
        Ok(mut iter) => iter.any(|sa| {
            std::net::TcpStream::connect_timeout(&sa, Duration::from_millis(200)).is_ok()
        }),
        Err(_) => false,
    }
}

/// Read `/proc/<pid>/stat` field 22 (start time in clock ticks since boot)
/// and convert to a humanish "1h 23m 45s" string. Linux-only. Returns None
/// on macOS / dev / failure — caller prints "" and moves on.
fn uptime_for_pid(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 2 is `(comm)` which can contain spaces — skip past it before
    // splitting on whitespace.
    let after_comm = stat.rsplit_once(')').map(|(_, t)| t)?.trim();
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Already-skipped fields: 1 (pid), 2 (comm). Field 22 in the original
    // numbering is field 22 - 2 - 1 = index 19 in `fields`.
    let starttime: u64 = fields.get(19)?.parse().ok()?;
    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let host_uptime: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if clk_tck <= 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    let proc_started_secs = starttime as f64 / clk_tck as f64;
    let alive_secs = (host_uptime - proc_started_secs).max(0.0) as u64;

    let h = alive_secs / 3600;
    let m = (alive_secs % 3600) / 60;
    let s = alive_secs % 60;
    Some(if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_probe_rejects_invalid_address() {
        assert!(!tcp_probe("not-a-host:99999"));
    }

    #[test]
    fn uptime_renders_for_self() {
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let me = std::process::id() as i32;
        // On non-Linux this returns None. On Linux it should produce
        // some string.
        if cfg!(target_os = "linux") {
            assert!(uptime_for_pid(me).is_some());
        }
    }

    #[test]
    fn data_dir_value_is_used_for_pid_path() {
        use crate::paths::CliConfig;
        // Ensure pid_file derives from the configured db_path when present.
        let cfg = CliConfig {
            listen: None,
            db_path: Some(std::path::PathBuf::from("/var/lib/havn/havn.db")),
        };
        assert_eq!(
            paths::pid_file(&cfg),
            std::path::PathBuf::from("/var/lib/havn/gateway.pid")
        );
    }
}
