//! `havn update` (issue #236): install a release of Havn over this one.
//!
//! It downloads the release's archive for this machine and its checksum
//! (which, from the same release, shows the download is whole, not who made
//! it), backs up the installation, puts each binary in place of the one
//! beside this `havn` (keeping the old one as `<name>.old`), restarts the
//! gateway through systemd and waits for it to answer as the new release.
//! A gateway that does not is given back its old binaries.
//!
//! Only a gateway that runs as the systemd unit `havn` is restarted: one
//! that runs otherwise is not touched (stop it first), and with none
//! running the binaries are only put in place.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use havn_core::release;
use sha2::{Digest as _, Sha256};

use crate::paths;

/// The binaries a release must bring; others it brings named `havn*` are
/// installed too.
pub const BINARIES: [&str; 4] = ["havn", "havn-gateway", "havn-runtime", "havn-init"];

/// The systemd unit the gateway runs as.
const UNIT: &str = "havn";

/// How long the restarted gateway may take to answer as the new release
/// while systemd still runs it (migrating a large database takes a while).
const START_WITHIN: Duration = Duration::from_secs(600);

/// Bytes of an archive the command takes.
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

/// Bytes of one binary the command takes from an archive.
const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;

fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("havn-cli")
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .build()
        .context("building the HTTP client")
}

/// The latest release's tag.
async fn latest(http: &reqwest::Client) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let response = http
        .get(release::LATEST_RELEASE_API)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the latest release")?;
    if !response.status().is_success() {
        bail!(
            "GitHub answered {} for the latest release",
            response.status()
        );
    }
    let release: Release = response
        .json()
        .await
        .context("reading the latest release")?;
    if release::parse_version(&release.tag_name).is_none() {
        bail!(
            "the latest release's tag {:?} is not a version",
            release.tag_name
        );
    }
    Ok(release.tag_name)
}

async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let mut response = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if !response.status().is_success() {
        bail!("downloading {url}: {}", response.status());
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_ARCHIVE_BYTES)
    {
        bail!("{url} is larger than an archive of Havn can be");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("downloading {url}"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > MAX_ARCHIVE_BYTES {
            bail!("{url} is larger than an archive of Havn can be");
        }
    }
    Ok(body)
}

/// Whether `archive` is what `sums` (a `sha256sum` line) says.
fn verify(archive: &[u8], sums: &str) -> anyhow::Result<()> {
    let wanted = sums
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("the checksum file is empty"))?
        .to_ascii_lowercase();
    let got: String = Sha256::digest(archive)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != wanted {
        bail!("the archive's checksum is not the release's: refusing to install it");
    }
    Ok(())
}

/// Whether `name` is one of the release's binaries: `havn` or `havn-…`.
fn a_binary(name: &str) -> bool {
    name == "havn"
        || name.strip_prefix("havn-").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Unpack the release's binaries from `archive` (a `.tar.gz`) into `to`:
/// regular files named as [`a_binary`], by their base name, at most
/// [`MAX_BINARY_BYTES`] each. Their names, sorted; an error when one of
/// [`BINARIES`] is missing.
fn unpack(archive: &[u8], to: &Path) -> anyhow::Result<Vec<String>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut names = Vec::new();
    for entry in tar.entries().context("reading the archive")? {
        let entry = entry.context("reading the archive")?;
        let path = entry.path().context("reading the archive")?.into_owned();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if !a_binary(&name) || !entry.header().entry_type().is_file() {
            continue;
        }
        if entry.header().size().unwrap_or(u64::MAX) > MAX_BINARY_BYTES {
            bail!("{name} in the archive is larger than a binary of Havn can be");
        }
        let mut body = Vec::new();
        entry
            .take(MAX_BINARY_BYTES + 1)
            .read_to_end(&mut body)
            .with_context(|| format!("reading {name} from the archive"))?;
        if body.len() as u64 > MAX_BINARY_BYTES {
            bail!("{name} in the archive is larger than a binary of Havn can be");
        }
        std::fs::write(to.join(&name), body).with_context(|| format!("unpacking {name}"))?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    for name in BINARIES {
        if !names.iter().any(|n| n == name) {
            bail!("the archive has no {name}");
        }
    }
    // The gateway last: the running one starts agents from the others.
    names.sort_by_key(|n| (n == "havn-gateway", n.clone()));
    Ok(names)
}

/// `dir`'s mark that an update runs there, removed when it ends.
struct Updating(PathBuf);

impl Updating {
    fn take(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(".havn-update.lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Self(path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "another havn update runs ({} is there; remove it if none does)",
                path.display()
            ),
            Err(e) => Err(e).with_context(|| format!("writing {}", path.display())),
        }
    }
}

impl Drop for Updating {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// What [`swap`] did: the binaries it replaced (the old kept as
/// `<name>.old`) and those it added.
#[derive(Debug, Default, PartialEq, Eq)]
struct Swapped {
    replaced: Vec<String>,
    added: Vec<String>,
}

/// Put each of `names` from `from` in place of the one in `dir`, the old one
/// kept as `<name>.old`: linked there first, then replaced in one rename, so
/// it is never missing. All or none.
fn swap(dir: &Path, from: &Path, names: &[String]) -> anyhow::Result<Swapped> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut done = Swapped::default();
    let result = (|| -> anyhow::Result<()> {
        for name in names {
            let fresh = dir.join(format!(".{name}.new"));
            let _ = std::fs::remove_file(&fresh);
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o755)
                .open(&fresh)
                .with_context(|| format!("writing {name} into {}", dir.display()))?;
            std::io::copy(&mut std::fs::File::open(from.join(name))?, &mut out)
                .with_context(|| format!("writing {name} into {}", dir.display()))?;
            drop(out);
            std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755))?;
            let current = dir.join(name);
            let old = dir.join(format!("{name}.old"));
            if current.exists() {
                let _ = std::fs::remove_file(&old);
                std::fs::hard_link(&current, &old)
                    .with_context(|| format!("keeping the old {name}"))?;
                std::fs::rename(&fresh, &current)
                    .with_context(|| format!("putting {name} in place"))?;
                done.replaced.push(name.clone());
            } else {
                std::fs::rename(&fresh, &current)
                    .with_context(|| format!("putting {name} in place"))?;
                done.added.push(name.clone());
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = restore(dir, &done);
        for name in names {
            let _ = std::fs::remove_file(dir.join(format!(".{name}.new")));
        }
        return Err(e);
    }
    Ok(done)
}

/// Undo [`swap`] in `dir`: the binaries it replaced get their old ones
/// back, and those it added are removed. Every one is tried; the errors
/// together.
fn restore(dir: &Path, swapped: &Swapped) -> anyhow::Result<()> {
    let mut failed = Vec::new();
    for name in &swapped.replaced {
        if let Err(e) = std::fs::rename(dir.join(format!("{name}.old")), dir.join(name)) {
            failed.push(format!("the old {name}: {e}"));
        }
    }
    for name in &swapped.added {
        if let Err(e) = std::fs::remove_file(dir.join(name))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            failed.push(format!("the new {name}: {e}"));
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("could not put back {}", failed.join("; "))
    }
}

fn systemctl(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether the unit's `state` is one the gateway runs, or is starting, in.
fn unit_up(state: &str) -> bool {
    matches!(state, "active" | "activating" | "reloading")
}

/// The process of the gateway that runs as the systemd unit, if one does;
/// an error while it is starting or stopping, to try again then.
fn unit_gateway() -> anyhow::Result<Option<u32>> {
    let shown =
        systemctl(&["show", "-p", "ActiveState", "-p", "MainPID", UNIT]).unwrap_or_default();
    let (state, pid) = (field(&shown, "ActiveState"), field(&shown, "MainPID"));
    if state == "active" {
        return Ok(pid.parse().ok().filter(|pid| *pid > 0));
    }
    if unit_up(&state) || state == "deactivating" {
        bail!("the gateway's unit {UNIT} is {state}: run havn update once it runs or is stopped");
    }
    Ok(None)
}

/// A `havn-gateway` process that runs on this host, however it was started.
fn gateway_process() -> Option<u32> {
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
            (comm.trim() == "havn-gateway").then_some(pid)
        })
}

/// `HAVN_CONFIG` and `HOME` in `raw`, a process's `environ`.
fn environment_in(raw: &[u8]) -> (Option<OsString>, Option<OsString>) {
    use std::os::unix::ffi::OsStrExt as _;
    let mut config = None;
    let mut home = None;
    for pair in raw.split(|b| *b == 0) {
        if let Some(value) = pair.strip_prefix(b"HAVN_CONFIG=") {
            config = Some(std::ffi::OsStr::from_bytes(value).to_os_string());
        } else if let Some(value) = pair.strip_prefix(b"HOME=") {
            home = Some(std::ffi::OsStr::from_bytes(value).to_os_string());
        }
    }
    (config, home)
}

/// `HAVN_CONFIG` and `HOME` of process `pid`, as it runs.
fn environment_of(pid: u32) -> anyhow::Result<(Option<OsString>, Option<OsString>)> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).with_context(|| {
        format!("reading the gateway's environment (pid {pid}): run it as root")
    })?;
    Ok(environment_in(&raw))
}

/// The value of `key` in `systemctl show` output `shown`.
fn field(shown: &str, key: &str) -> String {
    shown
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_default()
        .to_string()
}

/// `(ActiveState, NRestarts)` in `systemctl show` output `shown`.
fn parse_unit_state(shown: &str) -> (String, u64) {
    (
        field(shown, "ActiveState"),
        field(shown, "NRestarts").parse().unwrap_or(0),
    )
}

/// `(ActiveState, NRestarts)` of the unit.
fn unit_state() -> (String, u64) {
    parse_unit_state(
        &systemctl(&["show", "-p", "ActiveState", "-p", "NRestarts", UNIT]).unwrap_or_default(),
    )
}

fn restart_unit() -> anyhow::Result<u64> {
    // A start that failed before can leave the unit failed, or past
    // systemd's start limit: cleared first.
    let _ = systemctl(&["reset-failed", UNIT]);
    let (_, restarts) = unit_state();
    let status = std::process::Command::new("systemctl")
        .args(["restart", UNIT])
        .status()
        .context("running systemctl restart havn")?;
    if !status.success() {
        bail!("systemctl restart havn failed");
    }
    Ok(restarts)
}

/// The version the gateway at `listen` answers as, if it answers.
async fn answering(http: &reqwest::Client, listen: &str) -> Option<String> {
    let response = http
        .get(format!("http://{listen}/auth/info"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let info: serde_json::Value = response.json().await.ok()?;
    info.get("version")?.as_str().map(str::to_string)
}

/// Whether the gateway answers as `version` before systemd gives it up
/// (it fails, or restarts it after `restarts`) or [`START_WITHIN`] passes.
async fn comes_up_as(listen: &str, version: &str, restarts: u64) -> anyhow::Result<bool> {
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("building the HTTP client")?;
    let wanted = release::parse_version(version);
    let started = Instant::now();
    let mut said = Instant::now();
    while started.elapsed() < START_WITHIN {
        if answering(&http, listen)
            .await
            .as_deref()
            .and_then(release::parse_version)
            == wanted
        {
            return Ok(true);
        }
        let (state, now) = unit_state();
        // Failed, stopped, or restarted by systemd: not coming up.
        if (!state.is_empty() && !unit_up(&state)) || now > restarts {
            return Ok(false);
        }
        if said.elapsed() > Duration::from_secs(15) {
            println!("  still starting ({state}); a database being migrated takes a while…");
            said = Instant::now();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(false)
}

/// `havn update`: install release `version` (the latest when `None`), or
/// with `check` only say whether a newer one is out. With `backup`, the
/// installation is backed up first: the new gateway migrates its
/// databases, which the old binaries put back may not open. A release
/// older than the gateway's needs `downgrade`.
pub async fn run(
    version: Option<String>,
    check: bool,
    backup: bool,
    downgrade: bool,
) -> anyhow::Result<()> {
    let http = client()?;
    // The gateway as systemd runs it: its config and the release it is.
    let pid = unit_gateway()?;
    let (config, home) = match pid {
        Some(pid) => environment_of(pid)?,
        None => (std::env::var_os("HAVN_CONFIG"), std::env::var_os("HOME")),
    };
    let install = crate::backup::Install::in_env(config.as_deref(), home.as_deref())?;
    let cfg = if install.config.exists() {
        let text = std::fs::read_to_string(&install.config)
            .with_context(|| format!("reading {}", install.config.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", install.config.display()))?
    } else {
        paths::CliConfig::default()
    };
    let listen = paths::listen_addr(&cfg);
    let probe = reqwest::Client::builder().no_proxy().build()?;
    let current = match pid {
        Some(_) => answering(&probe, &listen).await.ok_or_else(|| {
            anyhow!("the gateway does not answer at {listen}: run havn update once it does")
        })?,
        None => env!("CARGO_PKG_VERSION").to_string(),
    };

    let tag = match version {
        Some(tag) => {
            let tag = if tag.starts_with('v') {
                tag
            } else {
                format!("v{tag}")
            };
            if release::parse_version(&tag).is_none() {
                bail!("{tag} is not a release's version, such as v0.2.9");
            }
            Some(tag)
        }
        None => None,
    };
    let asked = tag.is_some();
    let tag = match tag {
        Some(tag) => tag,
        None => latest(&http).await?,
    };
    if check {
        if release::newer(&tag, &current) {
            println!("{tag} is out (this is {current}): `havn update` installs it.");
        } else if asked {
            println!("{tag} is not newer than {current}.");
        } else {
            println!("Havn {current} is the latest release.");
        }
        return Ok(());
    }
    if !asked && !release::newer(&tag, &current) {
        println!("Havn {current} is the latest release.");
        return Ok(());
    }
    if release::newer(&current, &tag) && !downgrade {
        bail!(
            "{tag} is older than {current}: its gateway cannot open databases a newer one \
             migrated (--allow-downgrade to try, with a backup to restore)"
        );
    }
    if pid.is_none() {
        let other = gateway_process()
            .map(|pid| format!("pid {pid}"))
            .or_else(|| crate::gateway::running_gateway(&cfg));
        if let Some(holder) = other {
            bail!(
                "a gateway runs ({holder}), not as the systemd unit {UNIT}: stop it, run havn \
                 update, then start it again"
            );
        }
    }

    let archive_name = release::archive_for(std::env::consts::ARCH)
        .ok_or_else(|| anyhow!("there is no release of Havn for {}", std::env::consts::ARCH))?;
    let dir: PathBuf = std::env::current_exe()
        .context("finding this havn")?
        .canonicalize()
        .context("finding this havn")?
        .parent()
        .ok_or_else(|| anyhow!("this havn is in no directory"))?
        .to_path_buf();
    let stage = tempfile::Builder::new()
        .prefix("havn-update-")
        .tempdir()
        .context("making a directory to unpack into")?;
    if tempfile::Builder::new()
        .prefix(".havn-update-")
        .tempfile_in(&dir)
        .is_err()
    {
        bail!(
            "cannot write to {}: run it as root (sudo havn update)",
            dir.display()
        );
    }
    // One update at a time: each would undo the other's files.
    let _updating = Updating::take(&dir)?;

    println!("Downloading Havn {tag} for {}…", std::env::consts::ARCH);
    let url = format!("{}/{tag}/{archive_name}", release::RELEASE_DOWNLOADS);
    let archive = fetch(&http, &url).await?;
    let sums = fetch(&http, &format!("{url}.sha256")).await?;
    verify(&archive, &String::from_utf8_lossy(&sums))?;
    let names = unpack(&archive, stage.path())?;

    if backup {
        let output = home
            .as_deref()
            .map_or_else(|| dir.clone(), PathBuf::from)
            .join(format!(
                "havn-backup-before-{tag}-{}.tar.gz",
                chrono::Utc::now().format("%Y%m%d-%H%M%S")
            ));
        crate::backup::backup(&install, &output)
            .await
            .context("backing up first (--no-backup skips it)")?;
        println!("Backed up to {}.", output.display());
    }
    let swapped = swap(&dir, stage.path(), &names)?;
    println!(
        "Installed {tag} in {} (the previous binaries are kept as *.old).",
        dir.display()
    );

    if pid.is_none() {
        println!("No gateway runs as the systemd unit {UNIT}: start it to use {tag}.");
        return Ok(());
    }
    println!("Restarting the gateway…");
    let came_up = match restart_unit() {
        Ok(restarts) => comes_up_as(&listen, &tag, restarts).await?,
        Err(e) => {
            eprintln!("{e:#}");
            false
        }
    };
    if came_up {
        println!("The gateway runs {tag}. Agents start on it as they are restarted.");
        println!(
            "Channel adapters (Telegram, Lark) are updated on their own, and the release's \
             havn.service is not installed over yours."
        );
        return Ok(());
    }
    eprintln!("The gateway did not come up as {tag}; putting the previous binaries back.");
    if let Err(e) = restore(&dir, &swapped) {
        eprintln!("{e:#}");
    }
    let back = match restart_unit() {
        Ok(restarts) => comes_up_as(&listen, &current, restarts).await?,
        Err(_) => false,
    };
    let restore_hint = if backup {
        ", and restore the backup with `havn restore` if its databases were migrated"
    } else {
        ""
    };
    if back {
        bail!("{tag} did not start; {current} runs again (see the gateway's log{restore_hint})");
    }
    bail!(
        "{tag} did not start, and {current}, put back, does not answer either: see the gateway's \
         log{restore_hint}"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn archive(entries: &[(&str, tar::EntryType)]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for (path, kind) in entries {
                let body = format!("#!/bin/sh\necho {path}\n");
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(*kind);
                if kind.is_file() {
                    header.set_size(body.len() as u64);
                } else {
                    header.set_size(0);
                    header.set_link_name("/etc/passwd").unwrap();
                }
                header.set_mode(0o755);
                header.set_path(path).unwrap();
                header.set_cksum();
                let data: &[u8] = if kind.is_file() { body.as_bytes() } else { &[] };
                tar.append(&header, data).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        out
    }

    fn files(names: &[&str]) -> Vec<(String, tar::EntryType)> {
        names
            .iter()
            .map(|n| (format!("dist/{n}"), tar::EntryType::Regular))
            .collect()
    }

    fn of(entries: &[(String, tar::EntryType)]) -> Vec<u8> {
        let borrowed: Vec<(&str, tar::EntryType)> =
            entries.iter().map(|(p, k)| (p.as_str(), *k)).collect();
        archive(&borrowed)
    }

    #[test]
    fn the_unit_s_state_and_the_gateway_s_environment_are_read() {
        let shown = "NRestarts=2\nActiveState=activating\n";
        assert_eq!(parse_unit_state(shown), ("activating".to_string(), 2));
        assert_eq!(parse_unit_state(""), (String::new(), 0));
        assert_eq!(
            field("ActiveStateX=no\nActiveState=active", "ActiveState"),
            "active"
        );
        assert!(
            unit_up("active")
                && unit_up("activating")
                && !unit_up("inactive")
                && !unit_up("failed")
        );
        let (config, home) = environment_in(
            b"PATH=/bin\0HAVN_CONFIG=/etc/havn/config.toml\0HOME=/root\0HAVN_CONFIGX=no",
        );
        assert_eq!(
            config.as_deref(),
            Some(std::ffi::OsStr::new("/etc/havn/config.toml"))
        );
        assert_eq!(home.as_deref(), Some(std::ffi::OsStr::new("/root")));
        assert_eq!(environment_in(b""), (None, None));
    }

    #[test]
    fn one_update_runs_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let first = Updating::take(dir.path()).unwrap();
        assert!(Updating::take(dir.path()).is_err());
        drop(first);
        assert!(Updating::take(dir.path()).is_ok());
    }

    #[test]
    fn an_archive_is_taken_only_with_the_release_s_checksum() {
        let bytes = b"havn";
        let sum: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        verify(
            bytes,
            &format!("{sum}  havn-x86_64-unknown-linux-musl.tar.gz\n"),
        )
        .unwrap();
        verify(bytes, &sum.to_uppercase()).unwrap();
        assert!(verify(b"other", &sum).is_err());
        assert!(verify(bytes, "").is_err());
    }

    #[test]
    fn the_binaries_are_unpacked_new_ones_too_the_gateway_last_and_nothing_else() {
        let to = tempfile::tempdir().unwrap();
        let mut names = BINARIES.to_vec();
        names.extend(["havn-foo", "LICENSE", "havn.service"]);
        let mut entries = files(&names);
        // What must not be taken: a link named as a binary, a path out.
        entries.push(("dist/havn-bar".into(), tar::EntryType::Symlink));
        let got = unpack(&of(&entries), to.path()).unwrap();
        assert_eq!(
            got,
            vec![
                "havn",
                "havn-foo",
                "havn-init",
                "havn-runtime",
                "havn-gateway"
            ]
        );
        assert!(!to.path().join("LICENSE").exists());
        assert!(!to.path().join("havn.service").exists());
        assert!(!to.path().join("havn-bar").exists());
        let short = tempfile::tempdir().unwrap();
        assert!(
            unpack(&of(&files(&BINARIES[..3])), short.path()).is_err(),
            "a missing binary"
        );
        assert!(!a_binary("havn-../x") && !a_binary("havn-") && !a_binary("havnx"));
    }

    #[test]
    fn binaries_are_swapped_with_the_old_kept_and_put_back_as_they_were() {
        let dir = tempfile::tempdir().unwrap();
        let from = tempfile::tempdir().unwrap();
        for name in BINARIES {
            std::fs::write(dir.path().join(name), format!("old {name}")).unwrap();
        }
        // Left by an earlier update; not this run's to put back.
        std::fs::write(dir.path().join("havn-foo.old"), "stale").unwrap();
        let mut names: Vec<String> = BINARIES.iter().map(|n| n.to_string()).collect();
        names.push("havn-foo".into());
        for name in &names {
            std::fs::write(from.path().join(name), format!("new {name}")).unwrap();
        }
        let swapped = swap(dir.path(), from.path(), &names).unwrap();
        assert_eq!(swapped.added, vec!["havn-foo".to_string()]);
        for name in &names {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("new {name}")
            );
        }
        for name in BINARIES {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(format!("{name}.old"))).unwrap(),
                format!("old {name}")
            );
        }
        restore(dir.path(), &swapped).unwrap();
        // Done again, every one is tried and the misses said together.
        let again = restore(dir.path(), &swapped).unwrap_err().to_string();
        assert!(
            again.contains("havn-init") && again.contains("havn-gateway"),
            "{again}"
        );
        for name in BINARIES {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("old {name}")
            );
        }
        assert!(
            !dir.path().join("havn-foo").exists(),
            "what it added is removed"
        );

        // One missing from what is installed: none is changed.
        std::fs::remove_file(from.path().join("havn-init")).unwrap();
        assert!(swap(dir.path(), from.path(), &names).is_err());
        for name in BINARIES {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("old {name}")
            );
            assert!(!dir.path().join(format!(".{name}.new")).exists());
        }
        assert!(!dir.path().join("havn-foo").exists());
    }
}
