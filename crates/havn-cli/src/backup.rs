//! `havn backup` and `havn restore` (issue #163): what a gateway holds, in
//! one archive, and back.
//!
//! The archive holds a consistent copy of the gateway's database and of
//! each agent's (SQLite's `VACUUM INTO`, which a running gateway allows),
//! every agent's directory (workspace, outbox) with owners, modes and
//! times, and `config.toml`. Credentials stay encrypted as they are at
//! rest: restoring needs the same `HAVN_AGE_KEY`, which the archive does
//! not hold.
//!
//! The backup reads agents' workspaces as root while agents may change
//! them, so it never follows a link: each directory is opened without
//! following links and its entries are reached through that open
//! directory (`/proc/self/fd`), so an agent swapping a file or directory
//! for a link to the host's files cannot put them in the archive (and,
//! restored, in its workspace). An agent's database is copied the same
//! way before SQLite reads the copy.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use sqlx::Connection as _;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};

use crate::paths::CliConfig;

const FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const GATEWAY_DB: &str = "havn.db";
const CONFIG: &str = "config.toml";
const AGENTS: &str = "agents";
/// An agent's database, in its directory.
const AGENT_DB: &str = "workspace/agent.db";
/// A file at least this large with holes in it is sparse: left out, as
/// restoring it would write all of it out.
const SPARSE_BYTES: u64 = 64 * 1024 * 1024;

/// Whether open file `file`, `len` bytes long, has a hole before its end.
/// A file a filesystem stores compressed has fewer blocks than bytes but
/// no holes.
/// The file's offset is back at its start afterwards, where reading it
/// begins.
fn has_hole(file: &File, len: u64) -> io::Result<bool> {
    // SAFETY: lseek on a descriptor this function borrows open; the offset
    // it moves is set back to the start below, before anything reads.
    let hole = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_HOLE) };
    io::Seek::seek(&mut &*file, io::SeekFrom::Start(0))?;
    Ok(u64::try_from(hole).is_ok_and(|hole| hole < len))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub havn_version: String,
    pub created_at: String,
    pub agents: Vec<String>,
    /// The newest migration of the gateway's database, and of any agent's:
    /// a havn that knows them can restore it.
    pub gateway_schema: i64,
    pub agent_schema: i64,
    /// What is not in it, and why.
    #[serde(default)]
    pub incomplete: Vec<String>,
    /// What the credentials it holds need to be read.
    pub key: String,
}

/// Where an installation keeps what a backup holds.
#[derive(Debug, Clone)]
pub struct Install {
    pub db: PathBuf,
    pub workspaces: PathBuf,
    /// `config.toml`, where it is or would be.
    pub config: PathBuf,
}

/// The gateway's data directory by default, under `home`.
fn default_data_dir(home: Option<&std::ffi::OsStr>) -> PathBuf {
    home.map_or_else(
        || PathBuf::from("./havn-data"),
        |h| PathBuf::from(h).join(".local/share/havn"),
    )
}

impl Install {
    /// The installation as the gateway finds it: `config.toml` at
    /// `$HAVN_CONFIG` or `~/.config/havn/config.toml`, and the paths it sets.
    pub fn from_config() -> anyhow::Result<Self> {
        Self::in_env(
            std::env::var_os("HAVN_CONFIG").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// The installation a gateway with `HAVN_CONFIG` = `config` and `HOME`
    /// = `home` finds (`havn update` reads them from the running gateway).
    pub fn in_env(
        config: Option<&std::ffi::OsStr>,
        home: Option<&std::ffi::OsStr>,
    ) -> anyhow::Result<Self> {
        let config = config.map_or_else(
            || {
                home.map_or_else(
                    || PathBuf::from("./havn.toml"),
                    |h| PathBuf::from(h).join(".config/havn/config.toml"),
                )
            },
            PathBuf::from,
        );
        let cfg = if config.exists() {
            let text = fs::read_to_string(&config)
                .with_context(|| format!("reading {}", config.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", config.display()))?
        } else {
            CliConfig::default()
        };
        Ok(Self::of(&cfg, config, home))
    }

    fn of(cfg: &CliConfig, config: PathBuf, home: Option<&std::ffi::OsStr>) -> Self {
        Self {
            db: cfg
                .db_path
                .clone()
                .unwrap_or_else(|| default_data_dir(home).join(GATEWAY_DB)),
            workspaces: cfg
                .workspace_root
                .clone()
                .unwrap_or_else(|| default_data_dir(home).join(AGENTS)),
            config,
        }
    }

    /// This installation with the paths `cfg` sets in place of its own.
    fn with_paths_of(&self, cfg: &CliConfig) -> Self {
        Self {
            db: cfg.db_path.clone().unwrap_or_else(|| self.db.clone()),
            workspaces: cfg
                .workspace_root
                .clone()
                .unwrap_or_else(|| self.workspaces.clone()),
            config: self.config.clone(),
        }
    }
}

/// The newest migration database `db` has run, 0 for none.
async fn schema(db: &Path) -> anyhow::Result<i64> {
    let opts = SqliteConnectOptions::new().filename(db).read_only(true);
    let mut conn = SqliteConnection::connect_with(&opts).await?;
    let version: Option<(i64,)> =
        sqlx::query_as("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
            .fetch_optional(&mut conn)
            .await
            .ok()
            .flatten();
    conn.close().await.ok();
    Ok(version.map_or(0, |(v,)| v))
}

/// Copy database `db` to `into`, consistent even while it is written, and
/// check the copy.
async fn snapshot(db: &Path, into: &Path) -> anyhow::Result<()> {
    let opts = SqliteConnectOptions::new().filename(db).read_only(true);
    let mut conn = SqliteConnection::connect_with(&opts)
        .await
        .with_context(|| format!("opening {}", db.display()))?;
    sqlx::query("VACUUM INTO ?")
        .bind(into.to_string_lossy().into_owned())
        .execute(&mut conn)
        .await
        .with_context(|| format!("copying {}", db.display()))?;
    conn.close().await.ok();
    let opts = SqliteConnectOptions::new().filename(into).read_only(true);
    let mut copy = SqliteConnection::connect_with(&opts).await?;
    let (check,): (String,) = sqlx::query_as("PRAGMA integrity_check")
        .fetch_one(&mut copy)
        .await?;
    copy.close().await.ok();
    if check != "ok" {
        bail!("the copy of {} is not sound: {check}", db.display());
    }
    Ok(())
}

/// What a regular file of open directory `base` is now: inode, size and
/// change time; `None` when it is not there or not a regular file.
type Stamp = Option<(u64, u64, i64, i64)>;

fn stamp(base: &Path, name: &str) -> io::Result<Stamp> {
    match fs::symlink_metadata(base.join(name)) {
        Ok(m) if m.is_file() => Ok(Some((m.ino(), m.len(), m.ctime(), m.ctime_nsec()))),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Copy regular file `src` to `dst` (only this user can read it), never
/// what a link at `src` names.
fn copy_regular(src: &Path, dst: &Path) -> io::Result<()> {
    let mut from = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)?;
    if !from.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut to = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dst)?;
    io::copy(&mut from, &mut to)?;
    Ok(())
}

/// Copy an agent's database and its write-ahead log out of its workspace,
/// open directory `workspace`, into `into` and `into-wal`: byte for byte,
/// never through a link (the workspace is the agent's, and it could put
/// one there to another tenant's database). Copied again, a while later,
/// when the database changed or the log started over meanwhile; a log
/// that only grew still holds a whole prefix. False when the workspace
/// has no database file.
async fn copy_agent_db(workspace: &File, into: &Path) -> anyhow::Result<bool> {
    let base = PathBuf::from(format!("/proc/self/fd/{}", workspace.as_raw_fd()));
    let wal_copy = PathBuf::from(format!("{}-wal", into.display()));
    for attempt in 0..6u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(250 * attempt)).await;
        }
        let (db, wal) = (stamp(&base, "agent.db")?, stamp(&base, "agent.db-wal")?);
        if db.is_none() {
            return Ok(false);
        }
        copy_regular(&base.join("agent.db"), into)?;
        if wal.is_some() {
            copy_regular(&base.join("agent.db-wal"), &wal_copy)?;
        } else {
            let _ = fs::remove_file(&wal_copy);
        }
        let (db_after, wal_after) = (stamp(&base, "agent.db")?, stamp(&base, "agent.db-wal")?);
        let wal_grew = match (wal, wal_after) {
            (Some(a), Some(b)) => a.0 == b.0 && b.1 >= a.1,
            (a, b) => a == b,
        };
        if db_after == db && wal_grew {
            return Ok(true);
        }
    }
    bail!("it kept changing while it was copied")
}

/// A directory only this user can read, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(near: &Path) -> anyhow::Result<Self> {
        let dir = near.join(format!(".havn-backup-{}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An archive being written under a temporary name: renamed to its own
/// once whole, removed otherwise.
struct Partial {
    path: PathBuf,
    done: bool,
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.done {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// `havn backup`: write the archive to `output`, which must not exist and
/// must not lie among what it backs up.
pub async fn backup(install: &Install, output: &Path) -> anyhow::Result<Manifest> {
    if !install.db.exists() {
        bail!("there is no gateway database at {}", install.db.display());
    }
    if fs::symlink_metadata(output).is_ok() {
        bail!("{} exists already", output.display());
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent =
        fs::canonicalize(parent).with_context(|| format!("finding {}", parent.display()))?;
    let workspaces = fs::canonicalize(&install.workspaces).ok();
    if workspaces.as_ref().is_some_and(|w| parent.starts_with(w)) {
        bail!(
            "write the backup outside {}, which it backs up",
            install.workspaces.display()
        );
    }
    let scratch = Scratch::new(&parent)?;
    let gateway_copy = scratch.0.join(GATEWAY_DB);
    snapshot(&install.db, &gateway_copy).await?;
    let gateway_schema = schema(&gateway_copy).await?;

    // Each agent's database, copied as it is now; the archive takes the
    // copy in its place. One that cannot be copied is left out (its
    // agent's other files are not), and the backup says so.
    let mut agents = Vec::new();
    let mut incomplete = Vec::new();
    let mut agent_schema = 0;
    let mut copies: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut left_out: HashSet<PathBuf> = HashSet::new();
    if let Some(root) = &workspaces {
        let mut names: Vec<OsString> = fs::read_dir(root)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        names.sort();
        for name in names {
            let dir = root.join(&name);
            if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let id = name.to_string_lossy().into_owned();
            let entry = Path::new(AGENTS).join(&name).join(AGENT_DB);
            // `<id>` is the gateway's; `workspace` in it the agent's.
            if let Ok(workspace) = open_dir(&dir.join("workspace")) {
                let raw = scratch.0.join(format!("{id}.raw.db"));
                let copy = scratch.0.join(format!("{id}.db"));
                let copied = match copy_agent_db(&workspace, &raw).await {
                    Ok(true) => snapshot(&raw, &copy).await.map(|()| true),
                    other => other,
                };
                for f in ["", "-wal", "-shm"] {
                    let _ = fs::remove_file(format!("{}{f}", raw.display()));
                }
                match copied {
                    Ok(true) => {
                        agent_schema = agent_schema.max(schema(&copy).await.unwrap_or(0));
                        copies.insert(entry, copy);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!(
                            "warning: the database of agent {id} is not in the backup: {e:#}"
                        );
                        incomplete.push(format!("agent {id}: database not copied ({e:#})"));
                        left_out.insert(entry);
                    }
                }
            }
            agents.push(id);
        }
    }

    let mut manifest = Manifest {
        format: FORMAT,
        havn_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        agents,
        gateway_schema,
        agent_schema,
        incomplete,
        key: "Credentials and agent secrets are encrypted with the gateway's HAVN_AGE_KEY, \
              which this archive \
              does not hold; restoring needs the same key."
            .into(),
    };
    let mut partial_name = output.as_os_str().to_os_string();
    partial_name.push(".partial");
    let mut partial = Partial {
        path: PathBuf::from(partial_name),
        done: false,
    };
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial.path)
        .with_context(|| format!("creating {}", partial.path.display()))?;
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::default(),
    ));
    tar.follow_symlinks(false);

    let json = serde_json::to_vec_pretty(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    header.set_size(json.len() as u64);
    header.set_mtime(chrono::Utc::now().timestamp().try_into().unwrap_or(0));
    header.set_entry_type(tar::EntryType::Regular);
    tar.append_data(&mut header, MANIFEST, json.as_slice())?;

    // The gateway's database: its copy, with the original's owner and mode.
    let mut header = tar::Header::new_gnu();
    header.set_metadata_in_mode(&fs::metadata(&install.db)?, tar::HeaderMode::Complete);
    header.set_size(fs::metadata(&gateway_copy)?.len());
    tar.append_data(&mut header, GATEWAY_DB, File::open(&gateway_copy)?)?;

    if install.config.is_file() {
        let mut header = tar::Header::new_gnu();
        header.set_metadata_in_mode(&fs::metadata(&install.config)?, tar::HeaderMode::Complete);
        tar.append_data(&mut header, CONFIG, File::open(&install.config)?)?;
    }

    if let Some(root) = &workspaces {
        let walk = Walk {
            copies: &copies,
            left_out: &left_out,
            skipped: std::cell::RefCell::new(Vec::new()),
        };
        walk.add_tree(&mut tar, &open_dir(root)?, Path::new(AGENTS))?;
        // Found after the manifest went in: reported, not in the archive's.
        manifest.incomplete.extend(walk.skipped.into_inner());
    }
    let file = tar.into_inner()?.finish()?;
    file.sync_all()?;
    fs::rename(&partial.path, output)?;
    partial.done = true;
    Ok(manifest)
}

/// Open directory `path` itself, never what a link there names.
fn open_dir(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// Exactly `len` bytes of a file that may change while it is read: what
/// it holds, cut at `len` or made up to it with zeros, so the archive
/// stays whole whatever an agent does to the file meanwhile.
struct Exactly {
    inner: io::Take<File>,
    left: u64,
}

impl io::Read for Exactly {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let mut n = self.inner.read(buf)?;
        if n == 0 {
            n = usize::try_from(self.left)
                .unwrap_or(usize::MAX)
                .min(buf.len());
            buf[..n].fill(0);
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// What the walk of the agents' directories puts in place of what.
struct Walk<'a> {
    /// Agents' database copies, by their entry.
    copies: &'a HashMap<PathBuf, PathBuf>,
    /// Agents' databases that could not be copied: not archived as they
    /// are on disk either.
    left_out: &'a HashSet<PathBuf>,
    /// What the walk left out, and why.
    skipped: std::cell::RefCell<Vec<String>>,
}

impl Walk<'_> {
    /// Archive open directory `dir` as `name`, and what it holds:
    /// directories, files and links (as links). Sockets, pipes, devices
    /// and sparse giants are left out, and an agent's database and its
    /// journal files give way to its copy.
    fn add_tree<W: io::Write>(
        &self,
        tar: &mut tar::Builder<W>,
        dir: &File,
        name: &Path,
    ) -> io::Result<()> {
        let dir_meta = dir.metadata()?;
        let mut header = tar::Header::new_gnu();
        header.set_metadata_in_mode(&dir_meta, tar::HeaderMode::Complete);
        header.set_size(0);
        tar.append_data(&mut header, name, io::empty())?;

        // Through the open directory: its path may have changed since.
        let base = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        let mut children: Vec<OsString> = fs::read_dir(&base)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        children.sort();
        for child in children {
            let path = base.join(&child);
            let entry = name.join(&child);
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                let Ok(target) = fs::read_link(&path) else {
                    continue;
                };
                let mut header = tar::Header::new_gnu();
                header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
                header.set_size(0);
                tar.append_link(&mut header, &entry, &target)?;
            } else if meta.is_dir() {
                let Ok(sub) = open_dir(&path) else {
                    continue;
                };
                if sub.metadata()?.ino() != meta.ino() {
                    continue;
                }
                self.add_tree(tar, &sub, &entry)?;
            } else if meta.is_file() {
                self.add_file(tar, &path, &entry, &meta, &dir_meta)?;
            }
        }
        Ok(())
    }

    fn add_file<W: io::Write>(
        &self,
        tar: &mut tar::Builder<W>,
        path: &Path,
        entry: &Path,
        meta: &fs::Metadata,
        dir_meta: &fs::Metadata,
    ) -> io::Result<()> {
        if let Some(copy) = self.copies.get(entry) {
            let mut header = tar::Header::new_gnu();
            header.set_metadata_in_mode(meta, tar::HeaderMode::Complete);
            header.set_size(fs::metadata(copy)?.len());
            tar.append_data(&mut header, entry, File::open(copy)?)?;
            return Ok(());
        }
        let named = entry.to_string_lossy();
        let of_db = |suffix: &str| {
            named.strip_suffix(suffix).is_some_and(|db| {
                let db = Path::new(db);
                self.copies.contains_key(db) || self.left_out.contains(db)
            })
        };
        if self.left_out.contains(entry) || ["-wal", "-shm", "-journal"].into_iter().any(of_db) {
            return Ok(());
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        else {
            return Ok(());
        };
        let opened = file.metadata()?;
        if !opened.is_file() || opened.ino() != meta.ino() {
            return Ok(());
        }
        // A second name of another owner's file: not the agent's to keep.
        if opened.nlink() > 1 && opened.uid() != dir_meta.uid() {
            return Ok(());
        }
        let len = opened.len();
        // Holes, and most of it: a hole or two in a large file (a
        // preallocated download) keeps it.
        if len >= SPARSE_BYTES
            && opened.blocks().saturating_mul(512) < len / 2
            && has_hole(&file, len)?
        {
            eprintln!("warning: {} is sparse, left out", entry.display());
            self.skipped
                .borrow_mut()
                .push(format!("{}: sparse, {len} bytes", entry.display()));
            return Ok(());
        }
        let mut header = tar::Header::new_gnu();
        header.set_metadata_in_mode(&opened, tar::HeaderMode::Complete);
        header.set_size(len);
        let body = Exactly {
            inner: file.take(len),
            left: len,
        };
        tar.append_data(&mut header, entry, body)?;
        Ok(())
    }
}

/// What a restore did.
#[derive(Debug)]
pub struct Restored {
    pub manifest: Manifest,
    /// Where it put things.
    pub install: Install,
    /// Where an existing installation's files were moved, with `--force`.
    pub moved: Vec<PathBuf>,
    /// Where the archive's `config.toml` went: in place, or beside the one
    /// there.
    pub config: Option<PathBuf>,
}

/// What reading an archive through found.
struct Contents {
    manifest: Manifest,
    has_db: bool,
    config: Option<Vec<u8>>,
}

fn open_archive(archive: &Path) -> anyhow::Result<tar::Archive<flate2::read::GzDecoder<File>>> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(file)))
}

/// Read `archive` through without unpacking it: its manifest (first),
/// whether it holds the gateway's database, its config.
fn inspect(archive: &Path) -> anyhow::Result<Contents> {
    let mut tar = open_archive(archive)?;
    let mut manifest: Option<Manifest> = None;
    let mut has_db = false;
    let mut config = None;
    for entry in tar
        .entries()
        .with_context(|| format!("{} is not a havn backup", archive.display()))?
    {
        let mut entry = entry.with_context(|| format!("reading {}", archive.display()))?;
        let path = entry.path()?.into_owned();
        if !path.components().all(|c| matches!(c, Component::Normal(_))) {
            bail!(
                "the archive names a path outside its place: {}",
                path.display()
            );
        }
        if entry.header().entry_type().is_hard_link() {
            bail!(
                "the archive holds a hard link, which no havn backup does: {}",
                path.display()
            );
        }
        if manifest.is_none() {
            if path != Path::new(MANIFEST) {
                bail!(
                    "{} is not a havn backup: it starts with {}",
                    archive.display(),
                    path.display()
                );
            }
            let mut json = Vec::new();
            entry.read_to_end(&mut json)?;
            manifest = Some(serde_json::from_slice(&json).context("reading the manifest")?);
        } else if path == Path::new(GATEWAY_DB) {
            has_db = true;
        } else if path == Path::new(CONFIG) {
            let mut text = Vec::new();
            entry.read_to_end(&mut text)?;
            config = Some(text);
        }
    }
    Ok(Contents {
        manifest: manifest.context("the archive is empty")?,
        has_db,
        config,
    })
}

/// Where `path` goes aside to before a restore with `--force`.
fn aside(path: &Path, stamp: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".pre-restore-{stamp}"));
    PathBuf::from(name)
}

/// `rel` of `root`, its directories made as they are missing and none of
/// them a link or other non-directory: nothing an archive put there is
/// gone through.
fn within(root: &Path, rel: &Path) -> anyhow::Result<PathBuf> {
    let mut at = root.to_path_buf();
    let mut parts = rel.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(part) = part else {
            bail!(
                "the archive names a path outside its place: {}",
                rel.display()
            );
        };
        at.push(part);
        if parts.peek().is_none() {
            break;
        }
        match fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("the archive names a path through a link: {}", rel.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o755).create(&at)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(at)
}

/// `havn restore`: put `archive` in place of `install`, which must hold no
/// installation unless `force` moves its files aside. The gateway must be
/// stopped. A host with no `config.toml` takes the archive's, and its
/// paths. Nothing is moved before the archive has been read through.
pub fn restore(install: &Install, archive: &Path, force: bool) -> anyhow::Result<Restored> {
    let contents = inspect(archive)?;
    let manifest = contents.manifest;
    if manifest.format != FORMAT {
        bail!(
            "the backup's format {} is not one this havn reads",
            manifest.format
        );
    }
    if !contents.has_db {
        bail!("{} holds no gateway database", archive.display());
    }
    let known =
        |migrator: &sqlx::migrate::Migrator| migrator.iter().map(|m| m.version).max().unwrap_or(0);
    if manifest.gateway_schema > known(&havn_db::MIGRATOR)
        || manifest.agent_schema > known(&havn_db::AGENT_MIGRATOR)
    {
        bail!(
            "the backup's databases are newer than this havn ({}) knows: install havn {} or later first",
            env!("CARGO_PKG_VERSION"),
            manifest.havn_version
        );
    }
    let target = match &contents.config {
        Some(text) if !install.config.exists() => {
            let cfg: CliConfig = toml::from_str(&String::from_utf8_lossy(text))
                .context("parsing the backup's config.toml")?;
            install.with_paths_of(&cfg)
        }
        _ => install.clone(),
    };

    // Held through the restore: no gateway starts on it meanwhile.
    let _lock = match crate::gateway::lock_db(&target.db) {
        Ok(lock) => lock,
        Err(e) => bail!("{e}: stop the gateway before restoring"),
    };
    let mut target = target;
    // A link to where the agents are is where they are.
    if let Ok(real) = fs::canonicalize(&target.workspaces) {
        target.workspaces = real;
    }
    let db_files = ["", "-wal", "-shm"].map(|f| {
        let mut p = target.db.as_os_str().to_os_string();
        p.push(f);
        PathBuf::from(p)
    });
    let occupied = db_files.iter().any(|p| fs::symlink_metadata(p).is_ok())
        || fs::read_dir(&target.workspaces).is_ok_and(|mut d| d.next().is_some());
    let mut moved = Vec::new();
    if occupied {
        if !force {
            bail!(
                "{} already holds an installation; `--force` moves its database and agents aside first",
                target.db.display()
            );
        }
        // A mount point cannot be moved aside: found out before anything
        // is, not half way.
        if let (Ok(root), Some(up)) = (fs::metadata(&target.workspaces), target.workspaces.parent())
            && fs::metadata(up).is_ok_and(|parent| parent.dev() != root.dev())
        {
            bail!(
                "{} is a mount point, which `--force` cannot move aside: empty it first",
                target.workspaces.display()
            );
        }
        let stamp = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S"),
            std::process::id()
        );
        for path in db_files.iter().chain([&target.workspaces]) {
            if fs::symlink_metadata(path).is_ok() {
                let to = aside(path, &stamp);
                fs::rename(path, &to).with_context(|| {
                    format!(
                        "moving {} aside (moved so far: {})",
                        path.display(),
                        list(&moved)
                    )
                })?;
                eprintln!("moved aside: {} -> {}", path.display(), to.display());
                moved.push(to);
            }
        }
    }

    let placed = place(archive, &target, contents.config.as_deref());
    match placed {
        Ok(config) => Ok(Restored {
            manifest,
            install: target,
            moved,
            config,
        }),
        Err(e) if moved.is_empty() => Err(e),
        Err(e) => Err(e.context(format!(
            "the restore stopped part way; the installation that was there is at {}",
            list(&moved)
        ))),
    }
}

fn list(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "nothing".into();
    }
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Unpack `archive` into `target`, and write its config; where the config
/// went.
fn place(
    archive: &Path,
    target: &Install,
    config_text: Option<&[u8]>,
) -> anyhow::Result<Option<PathBuf>> {
    let data = target
        .db
        .parent()
        .context("the database path names no directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(data)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&target.workspaces)?;
    let db_name = target
        .db
        .file_name()
        .context("the database path names no file")?;

    let mut tar = open_archive(archive)?;
    tar.set_preserve_permissions(true);
    // Nothing restored runs as its owner for whoever starts it.
    tar.set_mask(0o6000);
    tar.set_preserve_ownerships(true);
    tar.set_preserve_mtime(true);
    tar.set_unpack_xattrs(false);
    tar.set_overwrite(false);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path == Path::new(GATEWAY_DB) {
            entry.unpack(data.join(db_name))?;
        } else if let Ok(rel) = path.strip_prefix(AGENTS) {
            if rel.as_os_str().is_empty() {
                continue;
            }
            let at = within(&target.workspaces, rel)?;
            entry
                .unpack(&at)
                .with_context(|| format!("restoring {}", path.display()))?;
        }
    }

    let mut config = None;
    if let Some(text) = config_text {
        let to = if target.config.exists() {
            target.config.with_extension("toml.restored")
        } else {
            if let Some(up) = target.config.parent() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(up)?;
            }
            target.config.clone()
        };
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&to)
            .and_then(|mut f| io::Write::write_all(&mut f, text))
            .with_context(|| format!("writing {}", to.display()))?;
        config = Some(to);
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    async fn database(path: &Path, value: &str) -> SqliteConnection {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let mut conn = SqliteConnection::connect_with(&opts).await.expect("db");
        sqlx::query("PRAGMA wal_autocheckpoint = 0")
            .execute(&mut conn)
            .await
            .expect("no checkpoint");
        sqlx::query("CREATE TABLE t (v TEXT)")
            .execute(&mut conn)
            .await
            .expect("table");
        sqlx::query("INSERT INTO t VALUES (?)")
            .bind(value)
            .execute(&mut conn)
            .await
            .expect("row");
        conn
    }

    async fn read(path: &Path) -> String {
        let opts = SqliteConnectOptions::new().filename(path).read_only(true);
        let mut conn = SqliteConnection::connect_with(&opts).await.expect("db");
        let (v,): (String,) = sqlx::query_as("SELECT v FROM t")
            .fetch_one(&mut conn)
            .await
            .expect("row");
        v
    }

    fn install(root: &Path) -> Install {
        Install {
            db: root.join("data/havn.db"),
            workspaces: root.join("data/agents"),
            config: root.join("config/havn/config.toml"),
        }
    }

    #[tokio::test]
    async fn a_backup_restores_what_it_held_and_never_follows_a_link() {
        let from = tempfile::tempdir().expect("dir");
        let at = install(from.path());
        fs::create_dir_all(at.workspaces.join("a1/workspace/inbox")).unwrap();
        fs::create_dir_all(at.config.parent().unwrap()).unwrap();
        let gateway = database(&at.db, "gateway").await;
        // Open while the backup runs: its row is only in the log.
        let agent = database(&at.workspaces.join("a1/workspace/agent.db"), "agent").await;
        assert!(at.workspaces.join("a1/workspace/agent.db-wal").exists());
        fs::write(at.workspaces.join("a1/workspace/inbox/notes.txt"), "kept").unwrap();
        fs::write(&at.config, "listen = \"127.0.0.1:8080\"\n").unwrap();
        // A link an agent made to a host file: kept as a link, not read.
        let secret = from.path().join("secret");
        fs::write(&secret, "host secret").unwrap();
        std::os::unix::fs::symlink(&secret, at.workspaces.join("a1/workspace/leak")).unwrap();
        // An agent whose database is a link to the gateway's: the link is
        // kept, the gateway's database is not copied as the agent's.
        fs::create_dir_all(at.workspaces.join("a2/workspace")).unwrap();
        std::os::unix::fs::symlink(&at.db, at.workspaces.join("a2/workspace/agent.db")).unwrap();
        // A setuid file comes back without the bit.
        let suid = at.workspaces.join("a1/workspace/tool");
        fs::write(&suid, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&suid, std::os::unix::fs::PermissionsExt::from_mode(0o4755)).unwrap();

        let archive = from.path().join("backup.tar.gz");
        let manifest = backup(&at, &archive).await.expect("backup");
        drop((gateway, agent));
        assert_eq!(manifest.agents, ["a1", "a2"]);
        assert!(manifest.incomplete.is_empty(), "{:?}", manifest.incomplete);
        assert_eq!(fs::metadata(&archive).unwrap().mode() & 0o777, 0o600);
        assert!(
            backup(&at, &archive).await.is_err(),
            "an existing archive stays"
        );

        let to = tempfile::tempdir().expect("dir");
        let into = install(to.path());
        let restored = restore(&into, &archive, false).expect("restore");
        assert_eq!(restored.config.as_deref(), Some(into.config.as_path()));
        assert_eq!(read(&into.db).await, "gateway");
        assert_eq!(
            read(&into.workspaces.join("a1/workspace/agent.db")).await,
            "agent"
        );
        assert_eq!(
            fs::read_to_string(into.workspaces.join("a1/workspace/inbox/notes.txt")).unwrap(),
            "kept"
        );
        let leak = into.workspaces.join("a1/workspace/leak");
        assert!(
            fs::symlink_metadata(&leak)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&leak).unwrap(), secret);
        let linked = into.workspaces.join("a2/workspace/agent.db");
        assert!(
            fs::symlink_metadata(&linked)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::metadata(into.workspaces.join("a1/workspace/tool"))
                .unwrap()
                .mode()
                & 0o7777,
            0o755
        );

        // A second restore needs --force, and moves the first aside.
        let err = restore(&into, &archive, false).expect_err("occupied");
        assert!(err.to_string().contains("--force"), "{err}");
        let again = restore(&into, &archive, true).expect("forced");
        assert!(
            again
                .moved
                .iter()
                .any(|m| m.to_string_lossy().contains(".pre-restore-"))
        );
        assert_eq!(
            again.config.unwrap(),
            into.config.with_extension("toml.restored")
        );
        assert_eq!(read(&into.db).await, "gateway");
    }

    #[tokio::test]
    async fn a_bad_archive_moves_nothing() {
        let host = tempfile::tempdir().expect("dir");
        let at = install(host.path());
        fs::create_dir_all(&at.workspaces).unwrap();
        drop(database(&at.db, "mine").await);
        let junk = host.path().join("junk.tar.gz");
        fs::write(&junk, "not an archive").unwrap();
        assert!(restore(&at, &junk, true).is_err());
        assert!(restore(&at, &host.path().join("missing.tar.gz"), true).is_err());
        assert_eq!(
            read(&at.db).await,
            "mine",
            "the installation stays in place"
        );
    }

    #[test]
    fn looking_for_holes_leaves_the_file_to_be_read_from_its_start() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("dense");
        fs::write(&path, vec![7u8; 100_000]).unwrap();
        let file = File::open(&path).unwrap();
        assert!(!has_hole(&file, 100_000).unwrap());
        let mut body = Exactly {
            inner: file.take(100_000),
            left: 100_000,
        };
        let mut out = Vec::new();
        body.read_to_end(&mut out).unwrap();
        assert!(
            out.iter().all(|b| *b == 7),
            "read from where the hole search left it"
        );
    }

    #[test]
    fn a_file_read_while_it_changes_keeps_its_length() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("f");
        fs::write(&path, "short").unwrap();
        let mut grown = Exactly {
            inner: File::open(&path).unwrap().take(3),
            left: 3,
        };
        let mut out = Vec::new();
        grown.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"sho");
        let mut shrunk = Exactly {
            inner: File::open(&path).unwrap().take(8),
            left: 8,
        };
        let mut out = Vec::new();
        shrunk.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"short\0\0\0");
    }
}
