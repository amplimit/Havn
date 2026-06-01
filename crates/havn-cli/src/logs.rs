//! `havn logs [--follow] [--lines N]` — read the gateway log.
//!
//! Only meaningful when the gateway was started in detached mode
//! (`havn gateway start --detach`), which redirects stdout/stderr into
//! `${data_dir}/gateway.log`. Foreground mode logs go to the calling
//! terminal — there's no central place to read them, so we tell the user
//! that explicitly rather than print an empty file.

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

use anyhow::{Context as _, anyhow};

use crate::paths;

const FOLLOW_POLL: Duration = Duration::from_millis(250);

pub fn run(follow: bool, lines: usize) -> anyhow::Result<()> {
    let cfg = paths::load_config().unwrap_or_default();
    let log_path = paths::log_file(&cfg);

    if !log_path.exists() {
        let pid_path = paths::pid_file(&cfg);
        if paths::read_pid(&pid_path)?.is_some() {
            return Err(anyhow!(
                "log file {} doesn't exist yet (gateway just started?)",
                log_path.display()
            ));
        }
        return Err(anyhow!(
            "no log file at {}. \
             Start the gateway in detached mode (`havn gateway start --detach`) \
             so logs go there. Foreground mode writes to the calling terminal.",
            log_path.display()
        ));
    }

    print_tail(&log_path, lines).with_context(|| format!("read {}", log_path.display()))?;
    if !follow {
        return Ok(());
    }

    follow_loop(&log_path)
}

/// Print the last `n` lines of `path`. Reads from the end backwards in
/// 8 KiB chunks so we don't slurp giant log files into memory just to
/// throw most of it away.
fn print_tail(path: &std::path::Path, n: usize) -> anyhow::Result<()> {
    let mut f = std::fs::File::open(path)?;
    let total_len = f.seek(SeekFrom::End(0))?;
    if total_len == 0 {
        return Ok(());
    }

    const CHUNK: u64 = 8192;
    let mut buf = Vec::<u8>::new();
    let mut pos = total_len;
    let mut newlines = 0usize;
    while pos > 0 && newlines <= n {
        let read_size = CHUNK.min(pos);
        pos -= read_size;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read_size as usize];
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        newlines = buf.iter().filter(|&&b| b == b'\n').count();
    }
    let to_skip = newlines.saturating_sub(n);
    let mut split_at = 0usize;
    let mut count = 0usize;
    if to_skip > 0 {
        for (i, &b) in buf.iter().enumerate() {
            if b == b'\n' {
                count += 1;
                if count == to_skip {
                    split_at = i + 1;
                    break;
                }
            }
        }
    }
    let mut out = std::io::stdout().lock();
    out.write_all(&buf[split_at..])?;
    Ok(())
}

/// `tail -F`-style follow. Re-opens the file if its inode changes
/// (logrotate). Doesn't try to catch deletes — operators who rotate logs
/// on this file should expect to re-run the command.
fn follow_loop(path: &std::path::Path) -> anyhow::Result<()> {
    let mut f = std::fs::File::open(path)?;
    let mut pos = f.seek(SeekFrom::End(0))?;
    let mut inode = file_inode(path)?;
    loop {
        std::thread::sleep(FOLLOW_POLL);
        // Detect logrotate.
        match file_inode(path) {
            Ok(cur) if cur != inode => {
                f = std::fs::File::open(path)?;
                pos = 0;
                inode = cur;
            }
            Ok(_) => {}
            Err(_) => continue, // file briefly gone during rotation
        }
        let end = f.seek(SeekFrom::End(0))?;
        if end < pos {
            // File truncated — start over from the top.
            pos = 0;
        }
        if end == pos {
            continue;
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut buf = vec![0u8; (end - pos) as usize];
        f.read_exact(&mut buf)?;
        std::io::stdout().lock().write_all(&buf)?;
        pos = end;
    }
}

#[cfg(unix)]
fn file_inode(path: &std::path::Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(path)?.ino())
}

#[cfg(not(unix))]
fn file_inode(path: &std::path::Path) -> std::io::Result<u64> {
    // Fallback — no logrotate detection on non-unix. Only mtime/size remain.
    Ok(std::fs::metadata(path)?.len())
}
