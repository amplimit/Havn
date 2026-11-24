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
//!
//! Then (issue #312) each channel adapter on the machine, as listed in
//! [`COMPONENTS`] or found by its systemd unit, is given its own latest
//! release the same way: checked, put in place with the old binary kept,
//! its units restarted and watched, and its old binary put back alone when
//! they do not come up. Packs are found and reported, not updated.

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

/// The channel adapters and packs on this machine, kept by `havn update`
/// and by their setup.
const COMPONENTS: &str = "/etc/havn/components.toml";

/// Where unit files are looked for, in the order systemd prefers them.
const UNIT_DIRS: [&str; 3] = [
    "/etc/systemd/system",
    "/usr/local/lib/systemd/system",
    "/lib/systemd/system",
];

/// Where packs install themselves, one directory each.
const PACKS: &str = "/opt/havn";

/// The channel adapters whose repository is known, by their binary's name
/// (the repository's name under amplimit).
const ADAPTERS: [&str; 3] = [
    "havn-channel-telegram",
    "havn-channel-discord",
    "havn-channel-lark",
];

/// Packs that install no directory of their own name: theirs, and the
/// directory under [`PACKS`] that shows one is installed.
const KNOWN_PACKS: [(&str, &str); 1] = [("havn-pack-browser", "playwright-cli")];

/// What an adapter logs when it has connected to the gateway.
const CONNECTED: &str = "havn WS connected";

/// How a release's notes name the oldest gateway it works with, on a line
/// of its own: `Requires Havn >= 0.2.12`.
const REQUIRES: &str = "requires havn >=";

/// How long a restarted adapter is watched: it must stay active for
/// `settle`, and, if the one before it had connected to the gateway, have
/// connected within `within`.
#[derive(Debug, Clone, Copy)]
struct Watch {
    settle: Duration,
    within: Duration,
    poll: Duration,
}

const WATCH: Watch = Watch {
    settle: Duration::from_secs(5),
    within: Duration::from_secs(60),
    poll: Duration::from_secs(1),
};

fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("havn-cli")
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .build()
        .context("building the HTTP client")
}

/// The tag and notes of the latest release at `api` (GitHub's address of a
/// repository's latest release); `None` when the repository has none.
async fn latest(http: &reqwest::Client, api: &str) -> anyhow::Result<Option<(String, String)>> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        body: Option<String>,
    }
    let response = http
        .get(api)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the latest release")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
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
    Ok(Some((release.tag_name, release.body.unwrap_or_default())))
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
/// `required` is missing.
fn unpack(archive: &[u8], to: &Path, required: &[&str]) -> anyhow::Result<Vec<String>> {
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
    for name in required {
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

/// `(ActiveState, NRestarts)` of `unit`.
fn unit_state(unit: &str) -> (String, u64) {
    parse_unit_state(
        &systemctl(&["show", "-p", "ActiveState", "-p", "NRestarts", unit]).unwrap_or_default(),
    )
}

/// Restart `unit`; how many times systemd had restarted it before.
fn restart_unit(unit: &str) -> anyhow::Result<u64> {
    // A start that failed before can leave the unit failed, or past
    // systemd's start limit: cleared first.
    let _ = systemctl(&["reset-failed", unit]);
    let (_, restarts) = unit_state(unit);
    let status = std::process::Command::new("systemctl")
        .args(["restart", unit])
        .status()
        .with_context(|| format!("running systemctl restart {unit}"))?;
    if !status.success() {
        bail!("systemctl restart {unit} failed");
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
        let (state, now) = unit_state(UNIT);
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

/// The directory of this `havn`, where the gateway's binaries are put.
fn install_dir() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()
        .context("finding this havn")?
        .canonicalize()
        .context("finding this havn")?
        .parent()
        .ok_or_else(|| anyhow!("this havn is in no directory"))?
        .to_path_buf())
}

/// One line of the report: a component, the version it was, the release
/// looked at, and what was done.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    from: String,
    to: String,
    result: String,
    failed: bool,
}

impl Row {
    fn new(name: &str, from: &str, to: &str, result: &str) -> Self {
        Self {
            name: name.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            result: result.to_string(),
            failed: false,
        }
    }
}

/// The gateway after its step: its line of the report, the version it
/// runs now, and the address it answers at.
#[derive(Debug)]
struct Gateway {
    row: Row,
    now: String,
    listen: String,
}

/// `havn update`: install release `version` of the gateway (the latest
/// when `None`), then, unless `only_gateway`, the latest release of each
/// channel adapter; with `check` only say which are out. With `backup`,
/// the installation is backed up first: the new gateway migrates its
/// databases, which the old binaries put back may not open. A release
/// older than the gateway's needs `downgrade`.
pub async fn run(
    version: Option<String>,
    check: bool,
    backup: bool,
    downgrade: bool,
    only_gateway: bool,
) -> anyhow::Result<()> {
    let http = client()?;
    let gateway = gateway(&http, version, check, backup, downgrade).await;
    if only_gateway {
        return gateway.map(|_| ());
    }
    let gateway = gateway.inspect_err(|_| {
        eprintln!("The channel adapters were not updated, as the gateway was not.");
    })?;
    components(&http, gateway, check).await
}

/// The gateway's step of [`run`].
async fn gateway(
    http: &reqwest::Client,
    version: Option<String>,
    check: bool,
    backup: bool,
    downgrade: bool,
) -> anyhow::Result<Gateway> {
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
        None => {
            latest(http, release::LATEST_RELEASE_API)
                .await?
                .ok_or_else(|| anyhow!("GitHub has no release of Havn"))?
                .0
        }
    };
    let stays = |result: &str| Gateway {
        row: Row::new("gateway", &current, &tag, result),
        now: current.clone(),
        listen: listen.clone(),
    };
    if check {
        let result = if release::newer(&tag, &current) {
            println!("{tag} is out (this is {current}): `havn update` installs it.");
            "newer release out"
        } else if asked {
            println!("{tag} is not newer than {current}.");
            "current"
        } else {
            println!("Havn {current} is the latest release.");
            "current"
        };
        return Ok(stays(result));
    }
    if !asked && !release::newer(&tag, &current) {
        println!("Havn {current} is the latest release.");
        return Ok(stays("current"));
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
    let dir = install_dir()?;
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
    let archive = fetch(http, &url).await?;
    let sums = fetch(http, &format!("{url}.sha256")).await?;
    verify(&archive, &String::from_utf8_lossy(&sums))?;
    let names = unpack(&archive, stage.path(), &BINARIES)?;

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

    let moved = |result: &str| Gateway {
        row: Row::new("gateway", &current, &tag, result),
        now: tag.clone(),
        listen: listen.clone(),
    };
    if pid.is_none() {
        println!("No gateway runs as the systemd unit {UNIT}: start it to use {tag}.");
        return Ok(moved("installed; not running"));
    }
    println!("Restarting the gateway…");
    let came_up = match restart_unit(UNIT) {
        Ok(restarts) => comes_up_as(&listen, &tag, restarts).await?,
        Err(e) => {
            eprintln!("{e:#}");
            false
        }
    };
    if came_up {
        println!("The gateway runs {tag}. Agents start on it as they are restarted.");
        println!("The release's havn.service is not installed over yours.");
        return Ok(moved("updated"));
    }
    eprintln!("The gateway did not come up as {tag}; putting the previous binaries back.");
    if let Err(e) = restore(&dir, &swapped) {
        eprintln!("{e:#}");
    }
    let back = match restart_unit(UNIT) {
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

/// What [`COMPONENTS`] holds: the channel adapters and packs on this
/// machine.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct Components {
    #[serde(default, rename = "adapter", skip_serializing_if = "Vec::is_empty")]
    adapters: Vec<Adapter>,
    #[serde(default, rename = "pack", skip_serializing_if = "Vec::is_empty")]
    packs: Vec<Pack>,
}

/// A channel adapter: a binary from the releases of `repo` (`owner/name`),
/// run by `units`, at `version` when last looked at.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct Adapter {
    name: String,
    #[serde(default)]
    repo: String,
    binary: PathBuf,
    #[serde(default)]
    units: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

/// A capability pack, installed at `path` by the setup of `repo`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct Pack {
    name: String,
    #[serde(default)]
    repo: String,
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

impl Components {
    /// The list at `path`; an empty one when there is none.
    fn read(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write the list to `path` in one rename.
    fn write(&self, path: &Path) -> anyhow::Result<()> {
        let text = format!(
            "# The channel adapters and packs on this machine, for havn update, which\n\
             # adds those it finds and keeps their versions here.\n\n{}",
            toml::to_string(self).context("writing the components")?
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
        }
        let fresh = path.with_extension("toml.tmp");
        std::fs::write(&fresh, text).with_context(|| format!("writing {}", fresh.display()))?;
        std::fs::rename(&fresh, path).with_context(|| format!("writing {}", path.display()))
    }

    /// Add what was `found` and is not listed yet: an adapter by its
    /// binary (a listed one gains the units found running it), a pack by
    /// its name or path.
    fn add(&mut self, found: Self) {
        for adapter in found.adapters {
            match self
                .adapters
                .iter_mut()
                .find(|a| a.binary == adapter.binary)
            {
                Some(listed) => {
                    if listed.repo.is_empty() {
                        listed.repo = adapter.repo;
                    }
                    for unit in adapter.units {
                        if !listed.units.contains(&unit) {
                            listed.units.push(unit);
                        }
                    }
                }
                None => self.adapters.push(adapter),
            }
        }
        for pack in found.packs {
            if !self
                .packs
                .iter()
                .any(|p| p.name == pack.name || p.path == pack.path)
            {
                self.packs.push(pack);
            }
        }
    }
}

/// The binaries a unit file's `ExecStart=` lines run, those named by an
/// absolute path.
fn exec_binaries(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let value = line
                .trim()
                .strip_prefix("ExecStart")?
                .trim_start()
                .strip_prefix('=')?;
            let value = value.trim().trim_start_matches(['-', '@', '+', '!', ':']);
            let first = value.split_whitespace().next()?.trim_matches('"');
            let path = PathBuf::from(first);
            path.is_absolute().then_some(path)
        })
        .collect()
}

/// The channel adapters that units in `unit_dirs` run (a unit in an
/// earlier directory hiding one of its name in a later one, as systemd
/// does; templates are left out), and the packs under `packs`.
fn discover(unit_dirs: &[&Path], packs: &Path) -> Components {
    let mut found = Components::default();
    let mut seen = std::collections::HashSet::new();
    for dir in unit_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".service") && !n.contains('@'))
            .collect();
        names.sort();
        for unit in names {
            if !seen.insert(unit.clone()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(dir.join(&unit)) else {
                continue;
            };
            for binary in exec_binaries(&text) {
                let Some(name) = binary.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !name.starts_with("havn-channel-") || !a_binary(name) {
                    continue;
                }
                match found.adapters.iter_mut().find(|a| a.binary == binary) {
                    Some(adapter) => adapter.units.push(unit.clone()),
                    None => found.adapters.push(Adapter {
                        name: name.to_string(),
                        repo: if ADAPTERS.contains(&name) {
                            format!("amplimit/{name}")
                        } else {
                            String::new()
                        },
                        binary,
                        units: vec![unit.clone()],
                        version: None,
                    }),
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(packs) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for path in dirs {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let known = KNOWN_PACKS.iter().find(|(_, dir)| *dir == name);
            if let Some((pack, _)) = known {
                found.packs.push(Pack {
                    name: (*pack).to_string(),
                    repo: format!("amplimit/{pack}"),
                    path,
                    version: None,
                });
            } else if path.join("SKILL.md").is_file() || path.join("setup.sh").is_file() {
                found.packs.push(Pack {
                    name: name.to_string(),
                    repo: String::new(),
                    path,
                    version: None,
                });
            }
        }
    }
    found
}

/// The oldest gateway a release's `notes` say it works with, from a line
/// `Requires Havn >= X.Y.Z`; `None` when they say none.
fn requires(notes: &str) -> Option<String> {
    notes.lines().find_map(|line| {
        let line = line.trim().trim_start_matches(['-', '*', '>', ' ']);
        let rest = line.get(..REQUIRES.len())?;
        if !rest.eq_ignore_ascii_case(REQUIRES) {
            return None;
        }
        let version = line[REQUIRES.len()..]
            .split_whitespace()
            .next()?
            .trim_end_matches(['.', ',']);
        release::parse_version(version)?;
        Some(version.trim_start_matches('v').to_string())
    })
}

/// What to do with a component at `installed`, given its latest release
/// `tag` with `notes`, beside a gateway at `gateway`.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Current,
    Skip(String),
    Install,
}

fn plan(installed: Option<&str>, tag: &str, notes: &str, gateway: &str) -> Plan {
    if installed.is_some_and(|v| release::parse_version(v).is_some() && !release::newer(tag, v)) {
        return Plan::Current;
    }
    match requires(notes) {
        Some(min) if release::newer(&min, gateway) => {
            Plan::Skip(format!("{tag} needs Havn {min} or later"))
        }
        _ => Plan::Install,
    }
}

/// A unit as `systemctl show` gives it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Shown {
    state: String,
    restarts: u64,
    invocation: String,
}

/// What updating a component asks of the machine, apart so tests can
/// stand in for systemd.
trait Host {
    /// The version `binary --version` says it is.
    fn version(&self, binary: &Path) -> Option<String>;
    /// The unit as it is now.
    fn show(&self, unit: &str) -> Shown;
    /// Restart the unit; how many times systemd had restarted it before.
    fn restart(&self, unit: &str) -> anyhow::Result<u64>;
    /// Whether the unit's run `invocation` logged `line`.
    fn logged(&self, invocation: &str, line: &str) -> bool;
}

/// This machine, through systemd.
#[derive(Debug)]
struct System;

impl Host for System {
    fn version(&self, binary: &Path) -> Option<String> {
        let mut child = std::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        // One that does not know --version could start running instead.
        let started = Instant::now();
        loop {
            match child.try_wait().ok()? {
                Some(_) => break,
                None if started.elapsed() > Duration::from_secs(10) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let mut out = String::new();
        child.stdout.take()?.read_to_string(&mut out).ok()?;
        version_in(&out)
    }

    fn show(&self, unit: &str) -> Shown {
        let shown = systemctl(&[
            "show",
            "-p",
            "ActiveState",
            "-p",
            "NRestarts",
            "-p",
            "InvocationID",
            unit,
        ])
        .unwrap_or_default();
        let (state, restarts) = parse_unit_state(&shown);
        Shown {
            state,
            restarts,
            invocation: field(&shown, "InvocationID"),
        }
    }

    fn restart(&self, unit: &str) -> anyhow::Result<u64> {
        restart_unit(unit)
    }

    fn logged(&self, invocation: &str, line: &str) -> bool {
        if invocation.is_empty() || !invocation.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        std::process::Command::new("journalctl")
            .args(["-o", "cat", "--no-pager"])
            .arg(format!("_SYSTEMD_INVOCATION_ID={invocation}"))
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(line))
    }
}

/// The version in `--version` output such as `havn-channel-telegram 0.2.11`.
fn version_in(out: &str) -> Option<String> {
    let version = out.lines().next()?.split_whitespace().last()?;
    release::parse_version(version)?;
    Some(version.trim_start_matches('v').to_string())
}

/// Restart `unit` and watch it: it must be active after `watch.settle`
/// without systemd having restarted it, and with `connect`, have logged
/// that it connected to the gateway, within `watch.within`.
async fn restart_watched(
    host: &impl Host,
    unit: &str,
    connect: bool,
    watch: Watch,
) -> Result<(), String> {
    let before = host.restart(unit).map_err(|e| format!("{e:#}"))?;
    let started = Instant::now();
    let mut invocation: Option<String> = None;
    loop {
        let now = host.show(unit);
        if !now.state.is_empty() && !unit_up(&now.state) {
            return Err(format!("{unit} is {}", now.state));
        }
        let rerun = invocation
            .as_ref()
            .is_some_and(|first| !now.invocation.is_empty() && *first != now.invocation);
        if now.restarts > before || rerun {
            return Err(format!("{unit} exited and systemd restarted it"));
        }
        if invocation.is_none() && !now.invocation.is_empty() {
            invocation = Some(now.invocation.clone());
        }
        let active = now.state == "active";
        let connected = !connect || (active && host.logged(&now.invocation, CONNECTED));
        if active && connected && started.elapsed() >= watch.settle {
            return Ok(());
        }
        if started.elapsed() >= watch.within {
            return Err(if active {
                format!("{unit} did not connect to the gateway")
            } else {
                format!("{unit} did not start")
            });
        }
        tokio::time::sleep(watch.poll).await;
    }
}

/// Put release `tag` of `adapter` in place from `archive` (checked against
/// `sums`; its binary is `name`), and restart and watch each of its units
/// that runs: one whose run had connected to the gateway, when
/// `gateway_up`, must connect again. On failure the old binary is put back
/// and the units restarted on it. What was done, or what failed.
async fn install(
    adapter: &Adapter,
    name: &str,
    tag: &str,
    archive: &[u8],
    sums: &str,
    gateway_up: bool,
    host: &impl Host,
    watch: Watch,
) -> Result<String, String> {
    let error = |e: anyhow::Error| format!("{e:#}");
    verify(archive, sums).map_err(error)?;
    let binary = adapter
        .binary
        .canonicalize()
        .with_context(|| format!("finding {}", adapter.binary.display()))
        .map_err(error)?;
    let (Some(dir), Some(file)) = (binary.parent(), binary.file_name().and_then(|n| n.to_str()))
    else {
        return Err(format!("{} is not a binary's path", binary.display()));
    };
    let stage = tempfile::Builder::new()
        .prefix("havn-update-")
        .tempdir()
        .context("making a directory to unpack into")
        .map_err(error)?;
    unpack(archive, stage.path(), &[name]).map_err(error)?;
    if file != name {
        std::fs::rename(stage.path().join(name), stage.path().join(file))
            .context("unpacking")
            .map_err(error)?;
    }
    let running: Vec<(&str, bool)> = adapter
        .units
        .iter()
        .filter_map(|unit| {
            let shown = host.show(unit);
            unit_up(&shown.state).then(|| {
                (
                    unit.as_str(),
                    gateway_up && host.logged(&shown.invocation, CONNECTED),
                )
            })
        })
        .collect();
    let swapped = swap(dir, stage.path(), &[file.to_string()]).map_err(error)?;

    let mut restarted = Vec::new();
    let mut failure = match host.version(&binary) {
        Some(v) if release::parse_version(&v) == release::parse_version(tag) => None,
        Some(v) => Some(format!("the new binary says it is {v}")),
        None => Some("the new binary does not run here".to_string()),
    };
    if failure.is_none() {
        for (unit, connect) in &running {
            restarted.push(*unit);
            if let Err(e) = restart_watched(host, unit, *connect, watch).await {
                failure = Some(e);
                break;
            }
        }
    }
    let Some(failure) = failure else {
        let stopped: Vec<&str> = adapter
            .units
            .iter()
            .map(String::as_str)
            .filter(|u| !running.iter().any(|(r, _)| r == u))
            .collect();
        return Ok(if running.is_empty() {
            "installed; no unit of it runs".to_string()
        } else if stopped.is_empty() {
            "updated".to_string()
        } else {
            format!("updated; not started: {}", stopped.join(", "))
        });
    };
    if let Err(e) = restore(dir, &swapped) {
        return Err(format!(
            "{failure}; putting the old binary back failed: {e:#}"
        ));
    }
    let mut down = Vec::new();
    for unit in restarted {
        if restart_watched(host, unit, false, watch).await.is_err() {
            down.push(unit);
        }
    }
    Err(if down.is_empty() {
        format!("{failure}; the old binary is back")
    } else {
        format!(
            "{failure}; the old binary is back, but {} does not run",
            down.join(", ")
        )
    })
}

/// Whether `repo` is `owner/name` as GitHub has them; its name.
fn repo_name(repo: &str) -> Option<&str> {
    let (owner, name) = repo.split_once('/')?;
    let fine = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    (fine(owner) && fine(name)).then_some(name)
}

/// The step of one channel adapter: look at its latest release and, unless
/// `check`, install it. `adapter.version` is kept up to date.
async fn adapter_step(
    http: &reqwest::Client,
    adapter: &mut Adapter,
    gateway: &str,
    check: bool,
    gateway_up: bool,
    host: &impl Host,
) -> Row {
    let mut row = Row::new(&adapter.name, "-", "-", "");
    if !adapter.binary.exists() {
        row.result = format!("skipped: {} is not there", adapter.binary.display());
        return row;
    }
    let installed = host
        .version(&adapter.binary)
        .or_else(|| adapter.version.clone());
    if let Some(v) = &installed {
        row.from.clone_from(v);
        adapter.version = Some(v.clone());
    }
    let Some(name) = repo_name(&adapter.repo) else {
        row.result = format!("skipped: no repo for it in {COMPONENTS}");
        return row;
    };
    let api = format!(
        "https://api.github.com/repos/{}/releases/latest",
        adapter.repo
    );
    let (tag, notes) = match latest(http, &api).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            row.result = "skipped: it has no release".to_string();
            return row;
        }
        Err(e) => {
            row.result = format!("failed: {e:#}");
            row.failed = true;
            return row;
        }
    };
    row.to.clone_from(&tag);
    match plan(installed.as_deref(), &tag, &notes, gateway) {
        Plan::Current => {
            row.result = "current".to_string();
            return row;
        }
        Plan::Skip(why) => {
            row.result = format!("skipped: {why}");
            return row;
        }
        Plan::Install if check => {
            row.result = "newer release out".to_string();
            return row;
        }
        Plan::Install => {}
    }
    let Some(arch) = release::archive_for(std::env::consts::ARCH)
        .and_then(|a| a.strip_prefix("havn-"))
        .and_then(|a| a.strip_suffix(".tar.gz"))
    else {
        row.result = format!("skipped: no release for {}", std::env::consts::ARCH);
        return row;
    };
    let url = format!(
        "https://github.com/{}/releases/download/{tag}/{name}-{arch}.tar.gz",
        adapter.repo
    );
    println!("Downloading {name} {tag}…");
    let fetched = async {
        let archive = fetch(http, &url).await?;
        let sums = fetch(http, &format!("{url}.sha256")).await?;
        anyhow::Ok((archive, sums))
    }
    .await;
    let (archive, sums) = match fetched {
        Ok(fetched) => fetched,
        Err(e) => {
            row.result = format!("failed: {e:#}");
            row.failed = true;
            return row;
        }
    };
    let sums = String::from_utf8_lossy(&sums);
    match install(
        adapter, name, &tag, &archive, &sums, gateway_up, host, WATCH,
    )
    .await
    {
        Ok(result) => {
            adapter.version = Some(tag.trim_start_matches('v').to_string());
            row.result = result;
        }
        Err(e) => {
            row.result = format!("failed: {e}");
            row.failed = true;
        }
    }
    row
}

/// The report: a table of `rows` under `head`.
fn table(head: [&str; 4], rows: &[Row]) -> String {
    let cells: Vec<[&str; 4]> = std::iter::once(head)
        .chain(rows.iter().map(|r| {
            [
                r.name.as_str(),
                r.from.as_str(),
                r.to.as_str(),
                r.result.as_str(),
            ]
        }))
        .collect();
    let mut widths = [0; 3];
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &cells {
        for (width, cell) in widths.iter().copied().zip(row) {
            out.push_str(&format!("{cell:<width$}  "));
        }
        out.push_str(row[3]);
        out.push('\n');
    }
    out
}

/// The channel adapters' and packs' step of [`run`], after the gateway's.
async fn components(http: &reqwest::Client, gateway: Gateway, check: bool) -> anyhow::Result<()> {
    let path = Path::new(COMPONENTS);
    let mut list = Components::read(path)?;
    let unit_dirs: Vec<&Path> = UNIT_DIRS.iter().map(Path::new).collect();
    list.add(discover(&unit_dirs, Path::new(PACKS)));
    // One update at a time, as for the gateway's binaries.
    let _updating = if check {
        None
    } else {
        Some(Updating::take(&install_dir()?)?)
    };
    let probe = reqwest::Client::builder().no_proxy().build()?;
    let gateway_up = !check && answering(&probe, &gateway.listen).await.is_some();

    let mut rows = vec![gateway.row];
    for adapter in &mut list.adapters {
        rows.push(adapter_step(http, adapter, &gateway.now, check, gateway_up, &System).await);
    }
    for pack in &list.packs {
        rows.push(Row::new(
            &pack.name,
            pack.version.as_deref().unwrap_or("-"),
            "-",
            "not updated by havn update: run its setup.sh",
        ));
    }
    let head = if check {
        ["component", "installed", "latest", "result"]
    } else {
        ["component", "from", "to", "result"]
    };
    println!();
    print!("{}", table(head, &rows));
    if check {
        return Ok(());
    }
    if (!list.adapters.is_empty() || !list.packs.is_empty())
        && let Err(e) = list.write(path)
    {
        eprintln!("{e:#}");
    }
    let failed: Vec<&str> = rows
        .iter()
        .filter(|r| r.failed)
        .map(|r| r.name.as_str())
        .collect();
    if !failed.is_empty() {
        bail!("not updated: {}", failed.join(", "));
    }
    Ok(())
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
        let got = unpack(&of(&entries), to.path(), &BINARIES).unwrap();
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
            unpack(&of(&files(&BINARIES[..3])), short.path(), &BINARIES).is_err(),
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

    #[test]
    fn units_that_run_an_adapter_and_packs_are_found() {
        assert_eq!(
            exec_binaries(
                "[Service]\nExecStartPre=/bin/true\nExecStart=-\"/usr/local/bin/havn-channel-x\" \
                 --config /etc/x.toml\nExecStart=relative --flag\n ExecStart = @/opt/y a\n"
            ),
            vec![
                PathBuf::from("/usr/local/bin/havn-channel-x"),
                PathBuf::from("/opt/y")
            ]
        );
        let etc = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let bin = "/usr/local/bin/havn-channel";
        for (dir, unit, binary) in [
            (etc.path(), "tg.service", format!("{bin}-telegram")),
            (etc.path(), "tg2.service", format!("{bin}-telegram")),
            (etc.path(), "odd.service", format!("{bin}-odd")),
            (etc.path(), "tpl@.service", format!("{bin}-lark")),
            (
                etc.path(),
                "havn.service",
                "/usr/local/bin/havn-gateway".into(),
            ),
            (etc.path(), "dc.timer", format!("{bin}-discord")),
            // Hidden by the one of its name in the earlier directory.
            (
                lib.path(),
                "tg.service",
                "/opt/havn-channel-telegram".into(),
            ),
            (
                lib.path(),
                "dc.service",
                "/usr/bin/havn-channel-discord".into(),
            ),
        ] {
            let text = format!("[Service]\nExecStart={binary} --config /etc/c.toml\n");
            std::fs::write(dir.join(unit), text).unwrap();
        }

        let packs = tempfile::tempdir().unwrap();
        for dir in ["playwright-cli", "chromium", "maps", "empty"] {
            std::fs::create_dir(packs.path().join(dir)).unwrap();
        }
        std::fs::write(packs.path().join("maps/SKILL.md"), "skill").unwrap();
        std::fs::write(packs.path().join("notes"), "a file").unwrap();

        let found = discover(&[etc.path(), lib.path()], packs.path());
        let adapters: Vec<(&str, &str, &Path, Vec<&str>)> = found
            .adapters
            .iter()
            .map(|a| {
                (
                    a.name.as_str(),
                    a.repo.as_str(),
                    a.binary.as_path(),
                    a.units.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            adapters,
            vec![
                (
                    "havn-channel-odd",
                    "",
                    Path::new("/usr/local/bin/havn-channel-odd"),
                    vec!["odd.service"]
                ),
                (
                    "havn-channel-telegram",
                    "amplimit/havn-channel-telegram",
                    Path::new("/usr/local/bin/havn-channel-telegram"),
                    vec!["tg.service", "tg2.service"]
                ),
                (
                    "havn-channel-discord",
                    "amplimit/havn-channel-discord",
                    Path::new("/usr/bin/havn-channel-discord"),
                    vec!["dc.service"]
                ),
            ]
        );
        let packs: Vec<(&str, &str)> = found
            .packs
            .iter()
            .map(|p| (p.name.as_str(), p.repo.as_str()))
            .collect();
        assert_eq!(
            packs,
            vec![
                ("maps", ""),
                ("havn-pack-browser", "amplimit/havn-pack-browser")
            ]
        );
        assert_eq!(
            discover(&[Path::new("/nonexistent")], Path::new("/nonexistent")),
            Components::default()
        );
    }

    #[test]
    fn the_list_keeps_what_it_says_and_gains_what_was_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("havn/components.toml");
        assert_eq!(Components::read(&path).unwrap(), Components::default());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[[adapter]]\nname = \"tg\"\nrepo = \"\"\n\
             binary = \"/usr/local/bin/havn-channel-telegram\"\nunits = [\"tg.service\"]\n\
             version = \"0.2.9\"\n\n[[pack]]\nname = \"maps\"\nrepo = \"someone/maps\"\n\
             path = \"/opt/havn/maps\"\n",
        )
        .unwrap();
        let mut list = Components::read(&path).unwrap();
        list.add(Components {
            adapters: vec![
                Adapter {
                    name: "havn-channel-telegram".into(),
                    repo: "amplimit/havn-channel-telegram".into(),
                    binary: "/usr/local/bin/havn-channel-telegram".into(),
                    units: vec!["tg.service".into(), "tg2.service".into()],
                    version: None,
                },
                Adapter {
                    name: "havn-channel-discord".into(),
                    repo: "amplimit/havn-channel-discord".into(),
                    binary: "/usr/local/bin/havn-channel-discord".into(),
                    units: vec!["dc.service".into()],
                    version: None,
                },
            ],
            packs: vec![Pack {
                name: "other-name".into(),
                repo: String::new(),
                path: "/opt/havn/maps".into(),
                version: None,
            }],
        });
        assert_eq!(list.adapters.len(), 2);
        let tg = &list.adapters[0];
        assert_eq!(tg.name, "tg", "its name as listed");
        assert_eq!(tg.repo, "amplimit/havn-channel-telegram");
        assert_eq!(tg.units, vec!["tg.service", "tg2.service"]);
        assert_eq!(tg.version.as_deref(), Some("0.2.9"));
        assert_eq!(list.packs.len(), 1, "the same pack by its path");
        list.write(&path).unwrap();
        assert_eq!(Components::read(&path).unwrap(), list);
        assert!(Components::read(&dir.path().join("havn")).is_err());
        std::fs::write(&path, "[[adapter]]\nname = 1\n").unwrap();
        assert!(Components::read(&path).is_err());
    }

    #[test]
    fn a_release_is_installed_only_when_newer_and_the_gateway_is_new_enough() {
        assert_eq!(
            requires("## Changes\n* Requires Havn >= 0.2.12.\n"),
            Some("0.2.12".into())
        );
        assert_eq!(requires("requires havn >= v0.3.0"), Some("0.3.0".into()));
        assert_eq!(requires("Requires Havn >= soon\nRequires Havn"), None);
        assert_eq!(requires(""), None);
        assert_eq!(plan(Some("0.2.11"), "v0.2.11", "", "0.2.12"), Plan::Current);
        assert_eq!(plan(Some("0.2.12"), "v0.2.11", "", "0.2.12"), Plan::Current);
        assert_eq!(plan(Some("0.2.9"), "v0.2.11", "", "0.2.12"), Plan::Install);
        assert_eq!(plan(None, "v0.2.11", "", "0.2.12"), Plan::Install);
        assert_eq!(
            plan(
                Some("0.2.9"),
                "v0.2.13",
                "Requires Havn >= 0.2.13",
                "0.2.12"
            ),
            Plan::Skip("v0.2.13 needs Havn 0.2.13 or later".into())
        );
        assert_eq!(
            plan(
                Some("0.2.9"),
                "v0.2.13",
                "Requires Havn >= 0.2.12",
                "0.2.12"
            ),
            Plan::Install
        );
        assert_eq!(
            version_in("havn-channel-telegram 0.2.11\n"),
            Some("0.2.11".into())
        );
        assert_eq!(version_in("error: unexpected argument"), None);
        assert_eq!(
            repo_name("amplimit/havn-channel-lark"),
            Some("havn-channel-lark")
        );
        for bad in ["", "amplimit", "a/b/c", "../x", "a/..", "a/b?c"] {
            assert_eq!(repo_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_report_is_a_table() {
        let mut failed = Row::new("havn-channel-lark", "0.2.9", "v0.2.11", "failed: x");
        failed.failed = true;
        let rows = [Row::new("gateway", "0.2.11", "v0.2.12", "updated"), failed];
        assert_eq!(
            table(["component", "from", "to", "result"], &rows),
            "component          from    to       result\n\
             gateway            0.2.11  v0.2.12  updated\n\
             havn-channel-lark  0.2.9   v0.2.11  failed: x\n"
        );
    }

    /// A machine whose one adapter unit runs a binary that holds its
    /// version as text: the version `bad` fails to start, and one that is
    /// not `old` connects only when `connects`.
    struct Fake {
        binary: PathBuf,
        old: &'static str,
        bad: &'static str,
        connects: bool,
        running: bool,
        runs: std::cell::Cell<u32>,
        restarted: std::cell::RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(binary: &Path) -> Self {
            Self {
                binary: binary.to_path_buf(),
                old: "0.2.9",
                bad: "",
                connects: true,
                running: true,
                runs: std::cell::Cell::new(0),
                restarted: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn installed(&self) -> String {
            std::fs::read_to_string(&self.binary)
                .unwrap()
                .trim()
                .to_string()
        }
    }

    impl Host for Fake {
        fn version(&self, binary: &Path) -> Option<String> {
            version_in(&std::fs::read_to_string(binary).ok()?)
        }

        fn show(&self, _unit: &str) -> Shown {
            let state = if !self.running && self.runs.get() == 0 {
                "inactive"
            } else if self.runs.get() > 0 && self.installed() == self.bad {
                "failed"
            } else {
                "active"
            };
            Shown {
                state: state.into(),
                restarts: 0,
                invocation: format!("{:032x}", self.runs.get()),
            }
        }

        fn restart(&self, unit: &str) -> anyhow::Result<u64> {
            self.runs.set(self.runs.get() + 1);
            self.restarted.borrow_mut().push(unit.to_string());
            Ok(0)
        }

        fn logged(&self, _invocation: &str, line: &str) -> bool {
            line == CONNECTED && (self.connects || self.installed() == self.old)
        }
    }

    const QUICK: Watch = Watch {
        settle: Duration::ZERO,
        within: Duration::from_millis(30),
        poll: Duration::from_millis(1),
    };

    /// An adapter installed at version 0.2.9 in `dir`, with the archive and
    /// checksum of its release `version`.
    fn adapter_at(dir: &Path, version: &str) -> (Adapter, Vec<u8>, String) {
        let binary = dir.join("havn-channel-test");
        std::fs::write(&binary, "0.2.9").unwrap();
        let mut out = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for (path, body) in [("havn-channel-test", version), ("config.example.toml", "x")] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_path(path).unwrap();
                header.set_cksum();
                tar.append(&header, body.as_bytes()).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        let sum: String = Sha256::digest(&out)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let adapter = Adapter {
            name: "havn-channel-test".into(),
            repo: "amplimit/havn-channel-test".into(),
            binary,
            units: vec!["test.service".into()],
            version: None,
        };
        (adapter, out, sum)
    }

    async fn install_on(
        fake: &Fake,
        adapter: &Adapter,
        archive: &[u8],
        sums: &str,
    ) -> Result<String, String> {
        install(
            adapter,
            "havn-channel-test",
            "v0.2.11",
            archive,
            sums,
            true,
            fake,
            QUICK,
        )
        .await
    }

    #[tokio::test]
    async fn an_adapter_is_installed_restarted_and_kept_when_it_comes_up() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake::new(&adapter.binary);
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums).await,
            Ok("updated".into())
        );
        assert_eq!(fake.installed(), "0.2.11");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("havn-channel-test.old")).unwrap(),
            "0.2.9"
        );
        assert_eq!(*fake.restarted.borrow(), vec!["test.service"]);
        assert!(!dir.path().join("config.example.toml").exists());

        // A unit that does not run is not started.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            running: false,
            ..Fake::new(&adapter.binary)
        };
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums).await,
            Ok("installed; no unit of it runs".into())
        );
        assert_eq!(fake.installed(), "0.2.11");
        assert!(fake.restarted.borrow().is_empty());
    }

    #[tokio::test]
    async fn an_adapter_that_does_not_come_up_gets_its_old_binary_back() {
        let dir = tempfile::tempdir().unwrap();

        // It fails to start: back to the old one, restarted on it.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            bad: "0.2.11",
            ..Fake::new(&adapter.binary)
        };
        let failed = install_on(&fake, &adapter, &archive, &sums)
            .await
            .unwrap_err();
        assert!(
            failed.contains("test.service is failed") && failed.contains("old binary is back"),
            "{failed}"
        );
        assert_eq!(fake.installed(), "0.2.9");
        assert_eq!(
            *fake.restarted.borrow(),
            vec!["test.service", "test.service"]
        );

        // It runs but does not connect, as the old one had.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            connects: false,
            ..Fake::new(&adapter.binary)
        };
        let failed = install_on(&fake, &adapter, &archive, &sums)
            .await
            .unwrap_err();
        assert!(
            failed.contains("did not connect to the gateway"),
            "{failed}"
        );
        assert_eq!(fake.installed(), "0.2.9");

        // Not the release it should be: put back before any restart.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.10");
        let fake = Fake::new(&adapter.binary);
        let failed = install_on(&fake, &adapter, &archive, &sums)
            .await
            .unwrap_err();
        assert!(failed.contains("says it is 0.2.10"), "{failed}");
        assert_eq!(fake.installed(), "0.2.9");
        assert!(fake.restarted.borrow().is_empty());

        // A download that is not the release's: nothing is touched.
        let (adapter, archive, _) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake::new(&adapter.binary);
        assert!(install_on(&fake, &adapter, &archive, "00").await.is_err());
        assert_eq!(fake.installed(), "0.2.9");
        assert!(fake.restarted.borrow().is_empty());
    }
}
