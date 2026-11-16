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
//! restored, in its workspace).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use sqlx::Connection as _;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};

use crate::paths;

const FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const GATEWAY_DB: &str = "havn.db";
const CONFIG: &str = "config.toml";
const AGENTS: &str = "agents";
/// An agent's database, in its workspace.
const AGENT_DB: &str = "workspace/agent.db";

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub havn_version: String,
    pub created_at: String,
    pub agents: Vec<String>,
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

impl Install {
    /// The installation the CLI's configuration describes.
    pub fn from_config() -> anyhow::Result<Self> {
        let cfg = paths::load_config()?;
        let db = cfg
            .db_path
            .clone()
            .unwrap_or_else(|| paths::data_dir(&cfg).join(GATEWAY_DB));
        Ok(Self {
            db,
            workspaces: paths::workspace_root(&cfg),
            config: paths::config_path().unwrap_or_else(paths::default_config_path),
        })
    }
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

/// The identity of regular file `name` of open directory `base` (inode,
/// size, change time), `None` when it is not there or not a regular file.
fn stamp(base: &Path, name: &str) -> io::Result<Option<(u64, u64, i64, i64)>> {
    match fs::symlink_metadata(base.join(name)) {
        Ok(m) if m.is_file() => Ok(Some((m.ino(), m.len(), m.mtime(), m.mtime_nsec()))),
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
/// one there to another tenant's database), again while either changes.
/// False when the workspace has no database file.
fn copy_agent_db(workspace: &File, into: &Path) -> anyhow::Result<bool> {
    let base = PathBuf::from(format!("/proc/self/fd/{}", workspace.as_raw_fd()));
    let wal_copy = PathBuf::from(format!("{}-wal", into.display()));
    for _ in 0..5 {
        let before = (stamp(&base, "agent.db")?, stamp(&base, "agent.db-wal")?);
        if before.0.is_none() {
            return Ok(false);
        }
        copy_regular(&base.join("agent.db"), into)?;
        if before.1.is_some() {
            copy_regular(&base.join("agent.db-wal"), &wal_copy)?;
        } else {
            let _ = fs::remove_file(&wal_copy);
        }
        if (stamp(&base, "agent.db")?, stamp(&base, "agent.db-wal")?) == before {
            return Ok(true);
        }
    }
    bail!("the agent's database kept changing while it was copied; try again")
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

/// `havn backup`: write the archive to `output`, which must not exist.
pub async fn backup(install: &Install, output: &Path) -> anyhow::Result<Manifest> {
    if !install.db.exists() {
        bail!("there is no gateway database at {}", install.db.display());
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let scratch = Scratch::new(parent)?;
    let gateway_copy = scratch.0.join(GATEWAY_DB);
    snapshot(&install.db, &gateway_copy).await?;

    // Each agent's database, copied as it is now; the archive takes the
    // copy in its place.
    let mut agents = Vec::new();
    let mut copies: HashMap<PathBuf, PathBuf> = HashMap::new();
    if install.workspaces.is_dir() {
        let mut names: Vec<OsString> = fs::read_dir(&install.workspaces)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        names.sort();
        for name in names {
            let dir = install.workspaces.join(&name);
            if !fs::symlink_metadata(&dir)?.is_dir() {
                continue;
            }
            let id = name.to_string_lossy().into_owned();
            // `<id>` is the gateway's; `workspace` in it the agent's.
            if let Ok(workspace) = open_dir(&dir.join("workspace")) {
                let raw = scratch.0.join(format!("{id}.raw.db"));
                if copy_agent_db(&workspace, &raw)
                    .with_context(|| format!("copying the database of agent {id}"))?
                {
                    let copy = scratch.0.join(format!("{id}.db"));
                    snapshot(&raw, &copy)
                        .await
                        .with_context(|| format!("the database of agent {id}"))?;
                    copies.insert(Path::new(AGENTS).join(&name).join(AGENT_DB), copy);
                }
            }
            agents.push(id);
        }
    }

    let manifest = Manifest {
        format: FORMAT,
        havn_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        agents,
        key: "Credentials are encrypted with the gateway's HAVN_AGE_KEY, which this archive \
              does not hold; restoring needs the same key."
            .into(),
    };
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .with_context(|| format!("creating {}", output.display()))?;
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

    if install.workspaces.is_dir() {
        let root = open_dir(&install.workspaces)?;
        add_tree(&mut tar, &root, Path::new(AGENTS), &copies)?;
    }
    let gz = tar.into_inner()?;
    let file = gz.finish()?;
    file.sync_all()?;
    Ok(manifest)
}

/// Open directory `path` itself, never what a link there names.
fn open_dir(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// Archive open directory `dir` as `name`, and what it holds: directories,
/// files and links (as links). Sockets, pipes and devices are left out, and
/// an agent's database and its journal files give way to its copy.
fn add_tree<W: io::Write>(
    tar: &mut tar::Builder<W>,
    dir: &File,
    name: &Path,
    copies: &HashMap<PathBuf, PathBuf>,
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
            let target = fs::read_link(&path)?;
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
            add_tree(tar, &sub, &entry, copies)?;
        } else if meta.is_file() {
            if let Some(copy) = copies.get(&entry) {
                let mut header = tar::Header::new_gnu();
                header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
                header.set_size(fs::metadata(copy)?.len());
                tar.append_data(&mut header, &entry, File::open(copy)?)?;
                continue;
            }
            let named = entry.to_string_lossy();
            let journal = ["-wal", "-shm", "-journal"].iter().any(|suffix| {
                named
                    .strip_suffix(suffix)
                    .is_some_and(|db| copies.contains_key(Path::new(db)))
            });
            if journal {
                continue;
            }
            let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
            else {
                continue;
            };
            let opened = file.metadata()?;
            if !opened.is_file() || opened.ino() != meta.ino() {
                continue;
            }
            // A second name of another owner's file: not the agent's to
            // keep.
            if opened.nlink() > 1 && opened.uid() != dir_meta.uid() {
                continue;
            }
            let mut header = tar::Header::new_gnu();
            header.set_metadata_in_mode(&opened, tar::HeaderMode::Complete);
            tar.append_data(&mut header, &entry, file)?;
        }
    }
    Ok(())
}

/// A version's numbers, for comparing.
fn version(v: &str) -> Vec<u64> {
    v.split(['.', '-'])
        .take(3)
        .map(|n| n.parse().unwrap_or(0))
        .collect()
}

/// Where `path` goes aside to before a restore with `--force`.
fn aside(path: &Path, stamp: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".pre-restore-{stamp}"));
    PathBuf::from(name)
}

/// What a restore did.
#[derive(Debug)]
pub struct Restored {
    pub manifest: Manifest,
    /// Where an existing installation was moved, with `--force`.
    pub moved: Vec<PathBuf>,
    /// Where the archive's `config.toml` went: in place, or beside the one
    /// there.
    pub config: Option<PathBuf>,
}

/// `havn restore`: put `archive` in place of `install`, which must hold no
/// installation unless `force` moves it aside. The gateway must be stopped.
pub fn restore(install: &Install, archive: &Path, force: bool) -> anyhow::Result<Restored> {
    match crate::gateway::lock_db(&install.db) {
        Ok(_) => {}
        Err(e) => bail!("{e}: stop the gateway before restoring"),
    }
    let occupied = |p: &Path| -> bool {
        p.exists()
            && (p.is_file()
                || fs::read_dir(p)
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(true))
    };
    let data = install
        .db
        .parent()
        .context("the database path names no directory")?
        .to_path_buf();
    let mut moved = Vec::new();
    if occupied(&install.db) || occupied(&install.workspaces) {
        if !force {
            bail!(
                "{} already holds an installation; `--force` moves it aside first",
                data.display()
            );
        }
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let mut gone: Vec<PathBuf> = Vec::new();
        for dir in [&data, &install.workspaces] {
            // The agents' directory usually lies in the data directory.
            if dir.exists() && !gone.iter().any(|g| dir.starts_with(g)) {
                let to = aside(dir, &stamp);
                fs::rename(dir, &to).with_context(|| format!("moving {} aside", dir.display()))?;
                gone.push(dir.clone());
                moved.push(to);
            }
        }
    }
    let parent = data.parent().context("the data directory has no parent")?;
    fs::create_dir_all(parent)?;
    let staging = Scratch::new(parent)?;

    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    tar.set_preserve_permissions(true);
    // Nothing restored runs as its owner for whoever starts it.
    tar.set_mask(0o6000);
    tar.set_preserve_ownerships(true);
    tar.set_preserve_mtime(true);
    tar.set_unpack_xattrs(false);
    tar.set_overwrite(false);
    let mut manifest: Option<Manifest> = None;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(Component::Normal(first)) = path.components().next() else {
            bail!("the archive holds an entry outside it: {}", path.display());
        };
        if manifest.is_none() {
            if path != Path::new(MANIFEST) {
                bail!(
                    "{} is not a havn backup: it starts with {}",
                    archive.display(),
                    path.display()
                );
            }
            let mut json = Vec::new();
            io::Read::read_to_end(&mut entry, &mut json)?;
            let read: Manifest = serde_json::from_slice(&json).context("reading the manifest")?;
            if read.format != FORMAT {
                bail!(
                    "the backup's format {} is not one this havn reads",
                    read.format
                );
            }
            if version(&read.havn_version) > version(env!("CARGO_PKG_VERSION")) {
                bail!(
                    "the backup was made by havn {}, newer than this {}: upgrade havn first",
                    read.havn_version,
                    env!("CARGO_PKG_VERSION")
                );
            }
            manifest = Some(read);
            continue;
        }
        if ![GATEWAY_DB, CONFIG, AGENTS].iter().any(|n| first == *n) {
            continue;
        }
        // Inside the staging directory only: no `..`, no absolute paths,
        // nothing through a link the archive made.
        entry.unpack_in(&staging.0)?;
    }
    let manifest = manifest.context("the archive is empty")?;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&data)?;
    fs::rename(staging.0.join(GATEWAY_DB), &install.db)
        .with_context(|| format!("putting the database at {}", install.db.display()))?;
    let agents = staging.0.join(AGENTS);
    if agents.exists() {
        if let Some(up) = install.workspaces.parent() {
            fs::create_dir_all(up)?;
        }
        fs::rename(&agents, &install.workspaces).with_context(|| {
            format!(
                "putting the agents at {} (on another filesystem, move {} there yourself)",
                install.workspaces.display(),
                agents.display()
            )
        })?;
    }
    let mut config = None;
    let restored_config = staging.0.join(CONFIG);
    if restored_config.exists() {
        let to = if install.config.exists() {
            install.config.with_extension("toml.restored")
        } else {
            if let Some(up) = install.config.parent() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(up)?;
            }
            install.config.clone()
        };
        fs::copy(&restored_config, &to)?;
        config = Some(to);
    }
    Ok(Restored {
        manifest,
        moved,
        config,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    async fn database(path: &Path, value: &str) {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let mut conn = SqliteConnection::connect_with(&opts).await.expect("db");
        sqlx::query("CREATE TABLE t (v TEXT)")
            .execute(&mut conn)
            .await
            .expect("table");
        sqlx::query("INSERT INTO t VALUES (?)")
            .bind(value)
            .execute(&mut conn)
            .await
            .expect("row");
        conn.close().await.ok();
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
        database(&at.db, "gateway").await;
        database(&at.workspaces.join("a1/workspace/agent.db"), "agent").await;
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
        assert_eq!(manifest.agents, ["a1", "a2"]);
        assert_eq!(fs::metadata(&archive).unwrap().mode() & 0o777, 0o600);

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

    #[test]
    fn a_newer_backup_is_refused() {
        assert!(version("0.3.0") > version(env!("CARGO_PKG_VERSION")));
        assert!(version("0.1.9") < version("0.2.0"));
        assert_eq!(version("0.2.1-rc1"), [0, 2, 1]);
    }
}
