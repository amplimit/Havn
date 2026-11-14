//! Access to files inside an agent's workspace that never follows a path
//! the agent controls.
//!
//! The gateway runs as root and the workspace is writable by the sandboxed
//! agent, which can replace any entry with a symlink, a FIFO or a directory
//! at any moment. Every operation here therefore resolves names relative to
//! an already-open directory handle (through `/proc/self/fd/<fd>/<name>`,
//! which does not re-walk the directory's own path), refuses to follow a
//! symlink in the final component (`O_NOFOLLOW`), never blocks on special
//! files (`O_NONBLOCK`) and only reads or writes regular files. What it
//! creates gets the owner of the directory it is in: under a root gateway
//! that is the agent's own host uid, and the agent must be able to change
//! what the gateway writes for it.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

const NOFOLLOW_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

/// An open directory inside (or equal to) an agent workspace.
#[derive(Debug)]
pub struct WorkspaceDir {
    dir: File,
}

impl WorkspaceDir {
    /// Open `path` as a directory. The final component must not be a
    /// symlink; the components above it must be gateway-owned (the
    /// workspace root and the agent's directory under it are).
    pub fn open(path: &Path) -> io::Result<Self> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | NOFOLLOW_FLAGS)
            .open(path)?;
        Ok(Self { dir })
    }

    /// Open the subdirectory `name`. `Ok(None)` when it does not exist.
    pub fn subdir(&self, name: &str) -> io::Result<Option<Self>> {
        match Self::open(&self.child(name)?) {
            Ok(dir) => Ok(Some(dir)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read the regular file `name` as UTF-8 (lossily), refusing files
    /// larger than `max_bytes`. `Ok(None)` when it does not exist.
    pub fn read_text(&self, name: &str, max_bytes: u64) -> io::Result<Option<String>> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW_FLAGS)
            .open(self.child(name)?)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        require_regular(name, &meta)?;
        if meta.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name} is larger than {max_bytes} bytes"),
            ));
        }
        let mut bytes = Vec::new();
        io::Read::by_ref(&mut file)
            .take(max_bytes)
            .read_to_end(&mut bytes)?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Replace the contents of the regular file `name`, creating it if
    /// needed.
    pub fn write_text(&self, name: &str, contents: &str) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .custom_flags(NOFOLLOW_FLAGS)
            .open(self.child(name)?)?;
        require_regular(name, &file.metadata()?)?;
        self.give_to_owner(&file)?;
        file.set_len(0)?;
        file.write_all(contents.as_bytes())
    }

    /// Create the regular file `name` with `contents` unless an entry of
    /// any kind already has that name.
    pub fn create_if_missing(&self, name: &str, contents: &str) -> io::Result<()> {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(NOFOLLOW_FLAGS)
            .open(self.child(name)?)
        {
            Ok(mut file) => {
                self.give_to_owner(&file)?;
                file.write_all(contents.as_bytes())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Create the subdirectory `name` unless an entry of any kind already
    /// has that name (mkdir does not follow a symlink there).
    pub fn create_subdir_if_missing(&self, name: &str) -> io::Result<()> {
        match std::fs::create_dir(self.child(name)?) {
            Ok(()) => {
                let created = Self::open(&self.child(name)?)?;
                self.give_to_owner(&created.dir)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Give `file`, created or opened in this directory, the directory's
    /// owner and group.
    fn give_to_owner(&self, file: &File) -> io::Result<()> {
        let dir = self.dir.metadata()?;
        let meta = file.metadata()?;
        if (meta.uid(), meta.gid()) != (dir.uid(), dir.gid()) {
            std::os::unix::fs::fchown(file, Some(dir.uid()), Some(dir.gid()))?;
        }
        Ok(())
    }

    /// Remove the entry `name` (unlinking a symlink itself, never its
    /// target). Missing entries are fine.
    pub fn remove(&self, name: &str) -> io::Result<()> {
        match std::fs::remove_file(self.child(name)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Names and metadata of up to `max` regular files directly in this
    /// directory whose names `accept` (symlinks and other entry types are
    /// skipped). Stops early, so a directory with millions of entries costs
    /// at most `max` lookups.
    pub fn regular_files(
        &self,
        accept: impl Fn(&str) -> bool,
        max: usize,
    ) -> io::Result<Vec<(String, Metadata)>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(self.proc_path())? {
            if files.len() >= max {
                break;
            }
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !accept(&name) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(self.child(&name)?) else {
                continue;
            };
            if meta.file_type().is_file() {
                files.push((name, meta));
            }
        }
        Ok(files)
    }

    fn proc_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.dir.as_raw_fd()))
    }

    fn child(&self, name: &str) -> io::Result<PathBuf> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid workspace entry name {name:?}"),
            ));
        }
        Ok(self.proc_path().join(name))
    }
}

fn require_regular(name: &str, meta: &Metadata) -> io::Result<()> {
    if meta.file_type().is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} is not a regular file"),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("havn-agent-files-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn reads_and_writes_regular_files() {
        let root = scratch();
        let ws = WorkspaceDir::open(&root).expect("open");
        assert_eq!(ws.read_text("SOUL.md", 1024).expect("read"), None);
        ws.write_text("SOUL.md", "first version").expect("write");
        ws.write_text("SOUL.md", "second").expect("rewrite");
        assert_eq!(
            ws.read_text("SOUL.md", 1024).expect("read").as_deref(),
            Some("second")
        );
        ws.create_if_missing("SOUL.md", "ignored").expect("exists");
        assert_eq!(
            ws.read_text("SOUL.md", 1024).expect("read").as_deref(),
            Some("second")
        );
        ws.remove("SOUL.md").expect("remove");
        ws.remove("SOUL.md").expect("remove missing");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn symlinks_are_never_followed() {
        let root = scratch();
        let outside = scratch().join("secret");
        std::fs::write(&outside, "host secret").expect("outside file");
        symlink(&outside, root.join("USER.md")).expect("symlink");
        let ws = WorkspaceDir::open(&root).expect("open");

        assert!(
            ws.read_text("USER.md", 1024).is_err(),
            "read through symlink"
        );
        assert!(
            ws.write_text("USER.md", "owned").is_err(),
            "write through symlink"
        );
        ws.create_if_missing("USER.md", "owned")
            .expect("leaves the link alone");
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside"),
            "host secret"
        );

        ws.remove("USER.md").expect("unlink the symlink");
        assert!(outside.exists(), "the target survives");

        let dangling = root.join("HEARTBEAT.md");
        symlink(root.join("../created-by-gateway"), &dangling).expect("dangling");
        ws.create_if_missing("HEARTBEAT.md", "x")
            .expect("leaves the link alone");
        assert!(!root.join("../created-by-gateway").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn symlinked_directories_are_not_entered() {
        let root = scratch();
        let elsewhere = scratch();
        std::fs::write(elsewhere.join("report.md"), "host data").expect("file");
        symlink(&elsewhere, root.join(".curator")).expect("dir symlink");
        let ws = WorkspaceDir::open(&root).expect("open");
        assert!(ws.subdir(".curator").is_err());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
    }

    #[test]
    fn listing_skips_non_regular_entries() {
        let root = scratch();
        std::fs::write(root.join("a.md"), "a").expect("a");
        symlink("/etc/hostname", root.join("b.md")).expect("b");
        std::fs::create_dir(root.join("c.md")).expect("c");
        let ws = WorkspaceDir::open(&root).expect("open");
        let names: Vec<String> = ws
            .regular_files(|name| name.ends_with(".md"), 10)
            .expect("list")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["a.md".to_string()]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn oversized_and_special_files_are_refused() {
        let root = scratch();
        std::fs::write(root.join("big.md"), "x".repeat(2048)).expect("big");
        let ws = WorkspaceDir::open(&root).expect("open");
        assert!(ws.read_text("big.md", 1024).is_err());
        let fifo = std::ffi::CString::new(root.join("pipe.md").to_str().expect("utf8")).expect("c");
        // SAFETY: mkfifo with a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        assert!(
            ws.read_text("pipe.md", 1024).is_err(),
            "FIFO must not block or be read"
        );
        assert!(ws.read_text("../escape", 1024).is_err());
        std::fs::remove_dir_all(&root).ok();
    }
}
