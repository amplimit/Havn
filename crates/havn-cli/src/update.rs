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

/// The words with which a release's notes say what it needs, as in
/// `Requires Havn >= 0.2.12` (the form the releases write), read in lower
/// case; the Chinese ones are looked for in the text.
const REQUIREMENT_WORDS: [&str; 9] = [
    "require",
    "requires",
    "required",
    "requirement",
    "requirements",
    "need",
    "needs",
    "needed",
    "minimum",
];
const REQUIREMENT_WORDS_ZH: [&str; 2] = ["需要", "要求"];

/// Words that, just before a requirement word, say it is not one (`longer`
/// as in `no longer requires`).
const NEGATIONS: [&str; 8] = [
    "not", "no", "never", "longer", "doesn't", "don't", "isn't", "aren't",
];

/// What may stand between `Havn` and the version it asks for; `>` asks for
/// a newer one than that.
const BEFORE_VERSION: [&str; 13] = [
    "gateway", "versions", "version", "版本", "at least", "between", ">=", "=>", "==", ">", "=",
    "~", "^",
];

/// Where the adapters' releases install their binary. One found elsewhere
/// was put there otherwise (built from source, a package): it is updated
/// only when [`COMPONENTS`] lists it.
const RELEASE_DIR: &str = "/usr/local/bin";

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
/// regular files named as [`a_binary`] or as one of `required`, by their
/// base name, at most [`MAX_BINARY_BYTES`] each. Their names, sorted; an
/// error when one of `required` is missing.
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
        let wanted = a_binary(&name) || required.contains(&name.as_str());
        if !wanted || !entry.header().entry_type().is_file() {
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
#[derive(Debug)]
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
/// it is never missing. A new binary is mode 755, or with `keep` (an
/// adapter's) has the old one's owner, mode and file capabilities. All or
/// none.
fn swap(dir: &Path, from: &Path, names: &[String], keep: bool) -> anyhow::Result<Swapped> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
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
            let current = dir.join(name);
            let was = std::fs::metadata(&current).ok().filter(|_| keep);
            if let Some(was) = &was {
                let made = std::fs::metadata(&fresh)?;
                if (made.uid(), made.gid()) != (was.uid(), was.gid()) {
                    std::os::unix::fs::chown(&fresh, Some(was.uid()), Some(was.gid()))
                        .with_context(|| format!("giving the new {name} the old one's owner"))?;
                }
            }
            let mode = was
                .as_ref()
                .map_or(0o755, |m| m.permissions().mode() & 0o777);
            std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(mode))?;
            if was.is_some() {
                copy_capabilities(&current, &fresh)
                    .with_context(|| format!("giving the new {name} the old one's capabilities"))?;
            }
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

/// Give `to` the file capabilities (`security.capability`) `from` has.
#[cfg(target_os = "linux")]
fn copy_capabilities(from: &Path, to: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let name = c"security.capability";
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    let mut value = [0u8; 256];
    // SAFETY: both strings are NUL-terminated and `value` is as long as said.
    let len = unsafe {
        libc::getxattr(
            from.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if len < 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENODATA | libc::ENOTSUP) => Ok(()),
            _ => Err(e.into()),
        };
    }
    // SAFETY: as above; `len` bytes of `value` were written.
    let set = unsafe {
        libc::setxattr(
            to.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            len as usize,
            0,
        )
    };
    if set != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn copy_capabilities(_from: &Path, _to: &Path) -> anyhow::Result<()> {
    Ok(())
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
/// runs now (with `--check`, the one it would), the address it answers
/// at, and the update's lock, held for the adapters.
#[derive(Debug)]
struct Gateway {
    row: Row,
    now: String,
    listen: String,
    updating: Option<Updating>,
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
    // The adapters' list is read first: a bad one stops the update before
    // the gateway's.
    let gathered = if only_gateway {
        None
    } else {
        let unit_dirs: Vec<&Path> = UNIT_DIRS.iter().map(Path::new).collect();
        Some(gather(
            Path::new(COMPONENTS),
            &unit_dirs,
            Path::new(PACKS),
            &System,
            check,
        )?)
    };
    let gateway = gateway(&http, version, check, backup, downgrade).await;
    let Some(gathered) = gathered else {
        return gateway.map(|_| ());
    };
    let gateway = gateway.inspect_err(|_| {
        eprintln!("The channel adapters were not updated, as the gateway was not.");
    })?;
    // One update at a time: the gateway's lock, or one taken now.
    let _updating = match gateway.updating {
        Some(lock) => Some(lock),
        None if check => None,
        None => Some(Updating::take(&install_dir()?)?),
    };
    let probe = reqwest::Client::builder().no_proxy().build()?;
    let gateway_up = !check && answering(&probe, &gateway.listen).await.is_some();
    let mut rows = vec![gateway.row];
    rows.extend(
        update_components(
            &http,
            &Github::default(),
            gathered,
            &gateway.now,
            gateway_up,
            check,
            &System,
            WATCH,
        )
        .await,
    );
    let head = if check {
        ["component", "installed", "latest", "result"]
    } else {
        ["component", "from", "to", "result"]
    };
    println!();
    print!("{}", table(head, &rows));
    let failed: Vec<&str> = rows
        .iter()
        .filter(|r| r.failed)
        .map(|r| r.name.as_str())
        .collect();
    if !check && !failed.is_empty() {
        bail!("not updated: {}", failed.join(", "));
    }
    Ok(())
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
    let stays = |result: &str, now: &str| Gateway {
        row: Row::new("gateway", &current, &tag, result),
        now: now.to_string(),
        listen: listen.clone(),
        updating: None,
    };
    if check {
        return Ok(if release::newer(&tag, &current) {
            println!("{tag} is out (this is {current}): `havn update` installs it.");
            stays("newer release out", tag.trim_start_matches('v'))
        } else if asked {
            println!("{tag} is not newer than {current}.");
            stays("current", &current)
        } else {
            println!("Havn {current} is the latest release.");
            stays("current", &current)
        });
    }
    if !asked && !release::newer(&tag, &current) {
        println!("Havn {current} is the latest release.");
        return Ok(stays("current", &current));
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
    let updating = Updating::take(&dir)?;

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
    let swapped = swap(&dir, stage.path(), &names, false)?;
    println!(
        "Installed {tag} in {} (the previous binaries are kept as *.old).",
        dir.display()
    );

    let moved = |result: &str| Gateway {
        row: Row::new("gateway", &current, &tag, result),
        now: tag.clone(),
        listen: listen.clone(),
        updating: Some(updating),
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

/// Where GitHub's API and its release downloads are.
#[derive(Debug, Clone)]
struct Github {
    api: String,
    downloads: String,
}

impl Default for Github {
    fn default() -> Self {
        Self {
            api: "https://api.github.com".to_string(),
            downloads: "https://github.com".to_string(),
        }
    }
}

/// What [`COMPONENTS`] holds: the channel adapters and packs on this
/// machine.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Components {
    #[serde(default, rename = "adapter", skip_serializing_if = "Vec::is_empty")]
    adapters: Vec<Adapter>,
    #[serde(default, rename = "pack", skip_serializing_if = "Vec::is_empty")]
    packs: Vec<Pack>,
}

/// A channel adapter: a binary from the releases of `repo` (`owner/name`),
/// run by `units`, at `version` when last looked at; left alone with
/// `skip`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Adapter {
    name: String,
    #[serde(default)]
    repo: String,
    binary: PathBuf,
    #[serde(default)]
    units: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    skip: bool,
}

/// A capability pack, installed at `path` by the setup of `repo`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Pack {
    name: String,
    #[serde(default)]
    repo: String,
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    skip: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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

    /// Write the list to `path` in one rename, keeping the file's mode
    /// (0600 for a new one; its directory, if missing, made 0700).
    fn write(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
        let text = format!(
            "# The channel adapters and packs on this machine (see Upgrading in Havn's\n\
             # README). havn update rewrites this file: comments are not kept.\n\n{}",
            toml::to_string(self).context("writing the components")?
        );
        if let Some(dir) = path.parent()
            && !dir.exists()
        {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("making {}", dir.display()))?;
        }
        let mode = std::fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o7777);
        let fresh = path.with_extension("toml.tmp");
        let _ = std::fs::remove_file(&fresh);
        let written = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&fresh)?;
            file.write_all(text.as_bytes())?;
            drop(file);
            std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(mode))?;
            std::fs::rename(&fresh, path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&fresh);
        }
        written.with_context(|| format!("writing {}", path.display()))
    }

    /// Add what was `found` and is not listed: an adapter by its binary (a
    /// listed one gains the units found running it), a pack by its name or
    /// path. A found adapter whose binary is not in [`RELEASE_DIR`] is not
    /// added but returned, with why.
    fn add(&mut self, found: Self) -> Vec<(Adapter, String)> {
        let mut unmanaged = Vec::new();
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
                None if adapter.binary.parent() == Some(Path::new(RELEASE_DIR)) => {
                    self.adapters.push(adapter);
                }
                None => {
                    let why = format!(
                        "{} is not in {RELEASE_DIR}; list it in {COMPONENTS} to update it",
                        adapter.binary.display()
                    );
                    unmanaged.push((adapter, why));
                }
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
        unmanaged
    }
}

/// What systemd says a unit runs: its own name (an alias shows the unit's),
/// the binaries of its `ExecStart=` and, when it runs them inside another
/// root, its `RootDirectory=` or `RootImage=` line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Exec {
    id: String,
    binaries: Vec<PathBuf>,
    root: Option<String>,
}

/// What a unit runs, from `systemctl show -p Id -p ExecStart -p
/// RootDirectory -p RootImage` output `shown` (drop-ins applied). Each
/// command is a `{ path=… ; argv[]=… ; … }`.
fn parse_exec(shown: &str) -> Exec {
    let binaries = shown
        .lines()
        .filter_map(|line| line.strip_prefix("ExecStart="))
        .flat_map(|value| value.split("{ path=").skip(1))
        .filter_map(|rest| rest.split([' ', ';']).next())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect();
    let root = ["RootDirectory", "RootImage"].iter().find_map(|key| {
        let value = field(shown, key);
        (!value.is_empty() && value != "/").then(|| format!("{key}={value}"))
    });
    Exec {
        id: field(shown, "Id"),
        binaries,
        root,
    }
}

/// Whether the file at `path` names a channel adapter's binary.
fn names_adapter(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.contains("havn-channel-"))
}

/// The units in `unit_dirs` (templates left out) whose file, or a drop-in
/// for it, names a channel adapter's binary: those systemd may run one in.
fn candidate_units(unit_dirs: &[&Path]) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for dir in unit_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        names.extend(
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".service") && !n.contains('@')),
        );
    }
    names
        .into_iter()
        .filter(|unit| {
            unit_dirs.iter().any(|dir| {
                names_adapter(&dir.join(unit))
                    || std::fs::read_dir(dir.join(format!("{unit}.d"))).is_ok_and(|drop_ins| {
                        drop_ins.flatten().any(|d| {
                            d.path().extension().is_some_and(|e| e == "conf")
                                && names_adapter(&d.path())
                        })
                    })
            })
        })
        .collect()
}

/// What to do about a unit that runs an adapter inside `root` (its
/// `RootDirectory=` or `RootImage=` line), when it is `listed` under an
/// adapter whose binary it does not run, or is not listed.
fn rooted_advice(root: &str, listed: bool) -> String {
    match (root.starts_with("RootImage="), listed) {
        (true, false) => format!(
            "havn update cannot change a binary inside an image: list it in {COMPONENTS} with \
             skip = true to leave it alone"
        ),
        (true, true) => format!(
            "havn update cannot change a binary inside an image: take it out of those units and \
             list it in {COMPONENTS} on its own with skip = true"
        ),
        (false, false) => {
            format!("list it in {COMPONENTS}, with its binary as this machine has it, to update it")
        }
        (false, true) => format!(
            "list it in {COMPONENTS} on its own, with its binary as this machine has it, to \
             update it"
        ),
    }
}

/// The channel adapters that units in `unit_dirs` run, as systemd resolves
/// the units (drop-ins applied, an alias once), and the packs under
/// `packs`; apart, those a unit runs inside another root, with why.
fn discover(
    unit_dirs: &[&Path],
    packs: &Path,
    host: &impl Host,
) -> (Components, Vec<(Adapter, String)>) {
    let mut found = Components::default();
    let mut rooted = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for unit in candidate_units(unit_dirs) {
        let exec = host.exec(&unit);
        let unit = if exec.id.is_empty() { unit } else { exec.id };
        if !seen.insert(unit.clone()) {
            continue;
        }
        for binary in exec.binaries {
            let Some(name) = binary.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("havn-channel-") || !a_binary(name) {
                continue;
            }
            // Its binary is not the one at that path here, and what this
            // machine has of it (a directory, an image) cannot be told
            // reliably from the unit: it is left to the list.
            if let Some(root) = &exec.root {
                rooted.push((
                    Adapter {
                        name: name.to_string(),
                        repo: String::new(),
                        binary,
                        units: vec![unit.clone()],
                        version: None,
                        skip: false,
                    },
                    format!(
                        "{unit} runs it under {root}; {}",
                        rooted_advice(root, false)
                    ),
                ));
                continue;
            }
            match found.adapters.iter_mut().find(|a| a.binary == binary) {
                Some(adapter) => {
                    if !adapter.units.contains(&unit) {
                        adapter.units.push(unit.clone());
                    }
                }
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
                    skip: false,
                }),
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
            let (name, repo) = match known {
                Some((pack, _)) => ((*pack).to_string(), format!("amplimit/{pack}")),
                None if path.join("SKILL.md").is_file() || path.join("setup.sh").is_file() => {
                    (name.to_string(), String::new())
                }
                None => continue,
            };
            found.packs.push(Pack {
                name,
                repo,
                path,
                version: None,
                skip: false,
            });
        }
    }
    (found, rooted)
}

/// `line` in lower case, without Markdown and brackets, `≥` as `>=`, its
/// spaces one each.
fn plainly(line: &str) -> String {
    let plain: String = line
        .to_lowercase()
        .replace(['≥', '≧'], ">=")
        .chars()
        .map(|c| {
            if matches!(c, '`' | '*' | '_' | ':' | '(' | ')' | '[' | ']') {
                ' '
            } else {
                c
            }
        })
        .collect();
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where `plain` names Havn: `havn` as a word of its own, or with a
/// version after a `-`.
fn havn_words(plain: &str) -> Vec<usize> {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '-';
    plain
        .match_indices("havn")
        .map(|(at, _)| at)
        .filter(|at| {
            let after = &plain[at + 4..];
            let versioned = after.strip_prefix('-').is_some_and(|rest| {
                rest.trim_start_matches('v')
                    .starts_with(|c: char| c.is_ascii_digit())
            });
            !plain[..*at].ends_with(word) && (versioned || !after.starts_with(word))
        })
        .collect()
}

/// Where `plain` first says something is required, other than that it is
/// not.
fn required_at(plain: &str) -> Option<usize> {
    let mut words: Vec<(usize, String)> = Vec::new();
    let mut start = None;
    for (at, c) in plain.char_indices().chain([(plain.len(), ' ')]) {
        let inside = c.is_alphanumeric() || matches!(c, '-' | '\'' | '’');
        match (inside, start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                words.push((from, plain[from..at].replace('’', "'")));
                start = None;
            }
            _ => {}
        }
    }
    let english = words.iter().enumerate().find_map(|(i, (at, w))| {
        (REQUIREMENT_WORDS.contains(&w.as_str())
            && !(i > 0 && NEGATIONS.contains(&words[i - 1].1.as_str())))
        .then_some(*at)
    });
    let chinese = REQUIREMENT_WORDS_ZH
        .iter()
        .flat_map(|word| plain.match_indices(word))
        .map(|(at, _)| at)
        .filter(|at| !plain[..*at].ends_with('不') && !plain[..*at].ends_with("不再"))
        .min();
    english.into_iter().chain(chinese).min()
}

/// The oldest version `rest`, what follows `Havn`, asks for: `0.2.12` for
/// `0.2.12`, `v0.2.12+`, `>= 0.2.12`, `version 0.2.12` or `> 0.2.11`. `None`
/// when none can be read there, as for a pre-release or build such as
/// `0.3.0-beta`.
fn version_after(rest: &str) -> Option<(u64, u64, u64)> {
    let mut rest = rest.trim_start();
    let mut newer = false;
    while let Some((lead, after)) = BEFORE_VERSION.iter().find_map(|lead| {
        let after = rest.strip_prefix(lead)?;
        let whole = !lead.ends_with(|c: char| c.is_ascii_alphabetic())
            || !after.starts_with(|c: char| c.is_alphanumeric());
        whole.then_some((*lead, after))
    }) {
        newer |= lead == ">";
        rest = after.trim_start();
    }
    let rest = rest.strip_prefix('v').unwrap_or(rest);
    let taken = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let suffix = &rest[taken..];
    let suffix = suffix.strip_prefix(['+', '-']).unwrap_or(suffix);
    if suffix.starts_with(|c: char| c.is_alphanumeric()) {
        return None;
    }
    let (major, minor, patch) = release::parse_version(rest[..taken].trim_end_matches('.'))?;
    Some((major, minor, patch + u64::from(newer)))
}

/// What each mention of Havn in `plain` from `from` on asks for.
fn asked(plain: &str, from: usize) -> Vec<Option<(u64, u64, u64)>> {
    havn_words(plain)
        .into_iter()
        .filter(|at| *at >= from)
        .map(|at| version_after(plain[at + 4..].trim_start_matches('-')))
        .collect()
}

/// The text of `line`, trimmed, when it is an item of a Markdown list.
fn list_item(line: &str) -> Option<&str> {
    if let Some((n, rest)) = line.split_once(['.', ')'])
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
        && rest.starts_with(' ')
    {
        return Some(rest.trim_start());
    }
    ["- ", "* ", "+ "]
        .iter()
        .find_map(|mark| line.strip_prefix(mark))
        .map(str::trim_start)
}

/// What a release's `notes` say of the oldest gateway it works with:
/// `None` when nothing does, else the highest version they ask for, or the
/// first line that asks for one but names none that can be read. A line
/// asks when it says something is required and then names Havn (`Requires
/// Havn >= X.Y.Z`, `Minimum Havn version: X.Y.Z`, `需要 Havn X.Y.Z`), or
/// is an item naming Havn in a list under a heading or a line ending in
/// `:` that says what is required; one naming Havn that ends in `:` takes
/// its version from the items. What a line asks for is the version right
/// after `Havn`; others on it are not looked at.
fn requires(notes: &str) -> Option<Result<String, String>> {
    // The list a heading (until the next) or a line (until a blank or other
    // line after its items) opened: whether it had items yet, or other
    // lines before them, whether it named Havn, and it, while no version
    // has been found for it.
    struct Scope {
        heading: bool,
        items: bool,
        text: bool,
        named: bool,
        waiting: Option<String>,
    }
    let mut highest = None;
    let mut scope: Option<Scope> = None;
    for line in notes.lines() {
        let raw = line.trim();
        let heading = raw.starts_with('#');
        // A line that begins in bold, such as `**Full Changelog**: …`, is a
        // heading too, as far as a list goes.
        let bold = raw.starts_with("**") || raw.starts_with("__");
        let item = list_item(raw);
        // A line's list may begin after other lines, but not after a blank
        // one that follows them.
        let ends = scope.as_ref().is_some_and(|s| {
            heading
                || bold
                || (!s.heading
                    && if s.items {
                        raw.is_empty() || item.is_none()
                    } else {
                        raw.is_empty() && s.text
                    })
        });
        if ends && let Some(waiting) = scope.take().and_then(|s| s.waiting) {
            return Some(Err(waiting));
        }
        if raw.is_empty() {
            continue;
        }
        let plain = plainly(raw);
        let required = required_at(&plain);
        let opens = required.is_some()
            && (heading
                || raw.trim_end_matches(['*', '_', '`']).ends_with(':')
                || plain
                    .split_whitespace()
                    .all(|w| REQUIREMENT_WORDS.contains(&w)));
        if opens {
            if let Some(waiting) = scope.take().and_then(|s| s.waiting) {
                return Some(Err(waiting));
            }
            let mentions = required.map_or_else(Vec::new, |at| asked(&plain, at));
            let found = mentions.iter().flatten().max().copied();
            highest = highest.max(found);
            scope = Some(Scope {
                heading,
                items: false,
                text: false,
                named: !mentions.is_empty(),
                waiting: (!mentions.is_empty() && found.is_none()).then(|| raw.to_string()),
            });
            continue;
        }
        if item.is_none()
            && let Some(s) = scope.as_mut()
        {
            s.text = true;
        }
        let mentions = match (scope.as_mut(), item) {
            (Some(s), Some(text)) => {
                s.items = true;
                let mentions = asked(&plain, 0);
                // Under a line that named Havn, the item is its version.
                if mentions.is_empty() && s.named {
                    version_after(&plainly(text))
                        .map(Some)
                        .into_iter()
                        .collect()
                } else {
                    mentions
                }
            }
            _ => required.map_or_else(Vec::new, |at| asked(&plain, at)),
        };
        if mentions.is_empty() {
            continue;
        }
        let Some(found) = mentions.iter().flatten().max().copied() else {
            return Some(Err(raw.to_string()));
        };
        highest = highest.max(Some(found));
        if item.is_some()
            && let Some(s) = scope.as_mut()
        {
            s.waiting = None;
        }
    }
    if let Some(waiting) = scope.and_then(|s| s.waiting) {
        return Some(Err(waiting));
    }
    highest.map(|(major, minor, patch)| Ok(format!("{major}.{minor}.{patch}")))
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
        Some(Ok(min)) if release::newer(&min, gateway) => {
            Plan::Skip(format!("{tag} needs Havn {min} or later"))
        }
        Some(Err(line)) => Plan::Skip(format!(
            "{tag} says it needs a Havn whose version cannot be read: {line:?}"
        )),
        _ => Plan::Install,
    }
}

/// A unit as `systemctl show` gives it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Shown {
    state: String,
    restarts: u64,
    invocation: String,
    pid: u32,
}

/// What updating a component asks of the machine, apart so tests can
/// stand in for systemd.
trait Host {
    /// The version `binary --version` says it is; as nobody, when
    /// `unprivileged`.
    fn version(&self, binary: &Path, unprivileged: bool) -> Option<String>;
    /// What the unit runs.
    fn exec(&self, unit: &str) -> Exec;
    /// The unit as it is now.
    fn show(&self, unit: &str) -> Shown;
    /// Restart the unit; how many times systemd had restarted it before.
    fn restart(&self, unit: &str) -> anyhow::Result<u64>;
    /// Whether the unit's run `invocation` logged `line`.
    fn logged(&self, invocation: &str, line: &str) -> bool;
    /// The binary process `pid` runs, and whether it is the file at
    /// `binary` (seen from another root, its path is not that); `None`
    /// when the file it runs cannot be looked at.
    fn exe(&self, pid: u32, binary: &Path) -> Option<(PathBuf, bool)>;
}

/// The user `--version` runs as, unprivileged.
const NOBODY: u32 = 65534;

/// This machine, through systemd.
#[derive(Debug)]
struct System;

impl Host for System {
    fn version(&self, binary: &Path, unprivileged: bool) -> Option<String> {
        let mut command = std::process::Command::new(binary);
        command
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unprivileged && unsafe { libc::geteuid() } == 0 {
            use std::os::unix::process::CommandExt as _;
            command.uid(NOBODY).gid(NOBODY);
        }
        let mut child = command.spawn().ok()?;
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

    fn exec(&self, unit: &str) -> Exec {
        parse_exec(
            &systemctl(&[
                "show",
                "-p",
                "Id",
                "-p",
                "ExecStart",
                "-p",
                "RootDirectory",
                "-p",
                "RootImage",
                unit,
            ])
            .unwrap_or_default(),
        )
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
            "-p",
            "MainPID",
            unit,
        ])
        .unwrap_or_default();
        let (state, restarts) = parse_unit_state(&shown);
        Shown {
            state,
            restarts,
            invocation: field(&shown, "InvocationID"),
            pid: field(&shown, "MainPID").parse().unwrap_or(0),
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

    fn exe(&self, pid: u32, binary: &Path) -> Option<(PathBuf, bool)> {
        use std::os::unix::fs::MetadataExt as _;
        let link = format!("/proc/{pid}/exe");
        let path = std::fs::read_link(&link).ok()?;
        // The link opens the file the process runs, whatever its root.
        let runs = std::fs::metadata(&link).ok()?;
        let same = std::fs::metadata(binary)
            .is_ok_and(|is| runs.dev() == is.dev() && runs.ino() == is.ino());
        Some((path, same))
    }
}

/// The version in `--version` output such as `havn-channel-telegram 0.2.11`.
fn version_in(out: &str) -> Option<String> {
    let version = out.lines().next()?.split_whitespace().last()?;
    release::parse_version(version)?;
    Some(version.trim_start_matches('v').to_string())
}

/// Why a restarted unit is not taken as up; `another` when it runs another
/// file of the binary's name (one of its own root's).
#[derive(Debug)]
struct Down {
    why: String,
    another: bool,
}

impl From<String> for Down {
    fn from(why: String) -> Self {
        Self {
            why,
            another: false,
        }
    }
}

/// Restart `unit` and watch it: after `watch.settle` it must be active,
/// running `binary`, without systemd having restarted it, and with
/// `connect`, have logged that it connected to the gateway, within
/// `watch.within`.
async fn restart_watched(
    host: &impl Host,
    unit: &str,
    binary: &Path,
    connect: bool,
    watch: Watch,
) -> Result<(), Down> {
    let before = host.restart(unit).map_err(|e| format!("{e:#}"))?;
    let started = Instant::now();
    let mut invocation: Option<String> = None;
    loop {
        let now = host.show(unit);
        if !now.state.is_empty() && !unit_up(&now.state) {
            return Err(format!("{unit} is {}", now.state).into());
        }
        let rerun = invocation
            .as_ref()
            .is_some_and(|first| !now.invocation.is_empty() && *first != now.invocation);
        if now.restarts > before || rerun {
            return Err(format!("{unit} exited and systemd restarted it").into());
        }
        if invocation.is_none() && !now.invocation.is_empty() {
            invocation = Some(now.invocation.clone());
        }
        let active = now.state == "active";
        // Just after the restart the process may not have exec'd the binary
        // yet (it is still systemd's): another binary fails only once it
        // has had `watch.settle`. It is the same file, not the same path:
        // a unit with its own root sees the binary at another, and another
        // binary at this one.
        let exe = if active && now.pid > 0 {
            host.exe(now.pid, binary)
        } else {
            None
        };
        let runs_it = exe.as_ref().is_some_and(|(_, same)| *same);
        let settled = started.elapsed() >= watch.settle;
        if let Some((exe, _)) = &exe
            && !runs_it
            && settled
        {
            let why = if exe == binary && !binary.exists() {
                format!("{unit} runs {}, which is gone", binary.display())
            } else if exe == binary {
                format!(
                    "{unit} runs another file at {} than this machine has there (it has a root or \
                     mounts of its own)",
                    exe.display()
                )
            } else {
                format!("{unit} runs {}, not {}", exe.display(), binary.display())
            };
            return Err(Down {
                why,
                another: binary.exists() && exe.file_name() == binary.file_name(),
            });
        }
        let connected = !connect || (active && host.logged(&now.invocation, CONNECTED));
        if runs_it && connected && settled {
            return Ok(());
        }
        if started.elapsed() >= watch.within {
            return Err(if !active {
                format!("{unit} did not start")
            } else if exe.is_none() {
                format!("cannot tell what {unit} runs")
            } else {
                format!("{unit} did not connect to the gateway")
            }
            .into());
        }
        tokio::time::sleep(watch.poll).await;
    }
}

/// The units of `adapter` by their own name, an alias once; and apart, with
/// what the table says of them, those that run inside another root and do
/// not run the file at `binary` or an older copy of it (the list names them
/// under it, as v0.2.13 could write it, but they run a binary of their
/// own), or that could not be looked at, not running: they are left alone.
fn units_of(adapter: &Adapter, binary: &Path, host: &impl Host) -> (Vec<String>, Vec<String>) {
    let old = binary.with_file_name(format!(
        "{}.old",
        binary
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
    ));
    let deleted = format!("{} (deleted)", binary.display());
    let mut units = Vec::new();
    let mut apart = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for unit in &adapter.units {
        let exec = host.exec(unit);
        let unit = if exec.id.is_empty() {
            unit.clone()
        } else {
            exec.id
        };
        if !seen.insert(unit.clone()) {
            continue;
        }
        if let Some(root) = &exec.root {
            let shown = host.show(&unit);
            if shown.state != "active" || shown.pid == 0 {
                let state = if shown.state.is_empty() {
                    "not running"
                } else {
                    shown.state.as_str()
                };
                apart.push(format!(
                    "not checked: {unit} runs under {root} and is {state}, so what it runs cannot \
                     be told; it is left as it is"
                ));
                continue;
            }
            // One not restarted since the binary was last replaced runs the
            // one before, now `<binary>.old` or deleted.
            let runs = host.exe(shown.pid, binary).map(|(path, same)| {
                same || path.as_os_str() == deleted.as_str()
                    || host.exe(shown.pid, &old).is_some_and(|(_, same)| same)
            });
            match runs {
                Some(true) => {}
                Some(false) => {
                    apart.push(format!(
                        "not managed: {unit} runs another binary under {root}, though \
                         {COMPONENTS} lists it under {}; {}",
                        adapter.name,
                        rooted_advice(root, true)
                    ));
                    continue;
                }
                None => {
                    apart.push(format!(
                        "not checked: cannot tell what {unit} runs under {root}; it is left as \
                         it is"
                    ));
                    continue;
                }
            }
        }
        units.push(unit);
    }
    (units, apart)
}

/// Put release `tag` of `adapter` in place from `archive` (checked against
/// `sums`; its binary is `name`), and restart and watch each of `units`
/// that runs, once each: one whose run had connected to the gateway, when
/// `gateway_up`, must connect again. On failure the old binary is put back
/// and the units restarted on it. A unit with its own root that, restarted,
/// runs a binary of its own is not a failure: it is left as it is. What was
/// done, with what the table says of such units, or what failed.
async fn install(
    adapter: &Adapter,
    units: &[String],
    name: &str,
    tag: &str,
    archive: &[u8],
    sums: &str,
    gateway_up: bool,
    host: &impl Host,
    watch: Watch,
) -> Result<(String, Vec<String>), String> {
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
    let mut running: Vec<(&str, bool)> = Vec::new();
    for unit in units {
        let shown = host.show(unit);
        if unit_up(&shown.state) {
            let connect = gateway_up && host.logged(&shown.invocation, CONNECTED);
            running.push((unit.as_str(), connect));
        }
    }
    let swapped = swap(dir, stage.path(), &[file.to_string()], true).map_err(error)?;

    let mut restarted = Vec::new();
    let mut apart = Vec::new();
    let mut failure = match host.version(&binary, false) {
        Some(v) if release::parse_version(&v) == release::parse_version(tag) => None,
        Some(v) => Some(format!("the new binary says it is {v}")),
        None => Some("the new binary does not run here".to_string()),
    };
    if failure.is_none() {
        for (unit, connect) in &running {
            let Err(down) = restart_watched(host, unit, &binary, *connect, watch).await else {
                restarted.push(*unit);
                continue;
            };
            let root = if down.another {
                host.exec(unit).root
            } else {
                None
            };
            if let Some(root) = root {
                apart.push(format!(
                    "not managed: {unit} runs another binary under {root} once restarted, though \
                     {COMPONENTS} lists it under {}; {}",
                    adapter.name,
                    rooted_advice(&root, true)
                ));
                continue;
            }
            restarted.push(*unit);
            failure = Some(down.why);
            break;
        }
    }
    let Some(failure) = failure else {
        let stopped: Vec<&str> = units
            .iter()
            .map(String::as_str)
            .filter(|u| !running.iter().any(|(r, _)| r == u))
            .collect();
        let done = if running.is_empty() {
            "installed; no unit of it runs".to_string()
        } else if restarted.is_empty() {
            "installed; no unit of it runs it".to_string()
        } else if stopped.is_empty() {
            "updated".to_string()
        } else {
            format!("updated; not started: {}", stopped.join(", "))
        };
        return Ok((done, apart));
    };
    if let Err(e) = restore(dir, &swapped) {
        return Err(format!(
            "{failure}; putting the old binary back failed: {e:#}"
        ));
    }
    let mut down = Vec::new();
    for unit in restarted {
        if restart_watched(host, unit, &binary, false, watch)
            .await
            .is_err()
        {
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

/// Whether GitHub has the repository whose API address is `api`.
async fn exists(http: &reqwest::Client, api: &str) -> anyhow::Result<bool> {
    let response = http
        .get(api)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the repository")?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND => Ok(false),
        status if status.is_success() => Ok(true),
        status => bail!("GitHub answered {status} for the repository"),
    }
}

/// The step of one channel adapter: look at its latest release and, unless
/// `check`, install it, restarting those of `units` that run; what the
/// table says of units left as they are goes to `apart`.
/// `adapter.version` is kept up to date. With `check` the version recorded
/// is taken before asking the binary, which then runs as nobody.
async fn adapter_step(
    http: &reqwest::Client,
    github: &Github,
    adapter: &mut Adapter,
    units: &[String],
    apart: &mut Vec<String>,
    gateway: &str,
    check: bool,
    gateway_up: bool,
    host: &impl Host,
    watch: Watch,
) -> Row {
    let recorded = adapter.version.clone();
    let mut row = Row::new(&adapter.name, recorded.as_deref().unwrap_or("-"), "-", "");
    let failed = |mut row: Row, why: String| {
        row.result = format!("failed: {why}");
        row.failed = true;
        row
    };
    if adapter.skip {
        row.result = format!("skipped: skip = true in {COMPONENTS}");
        return row;
    }
    if !adapter.binary.exists() {
        row.result = format!("skipped: {} is not there", adapter.binary.display());
        return row;
    }
    let installed = if check {
        recorded.or_else(|| host.version(&adapter.binary, true))
    } else {
        host.version(&adapter.binary, false).or(recorded)
    };
    if let Some(v) = &installed {
        row.from.clone_from(v);
        adapter.version = Some(v.clone());
    }
    let Some(name) = repo_name(&adapter.repo) else {
        row.result = format!("skipped: no repo for it in {COMPONENTS}");
        return row;
    };
    let repo = format!("{}/repos/{}", github.api, adapter.repo);
    let (tag, notes) = match latest(http, &format!("{repo}/releases/latest")).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return match exists(http, &repo).await {
                Ok(true) => {
                    row.result = "skipped: it has no release".to_string();
                    row
                }
                Ok(false) => failed(row, format!("repository {} not found", adapter.repo)),
                Err(e) => failed(row, format!("{e:#}")),
            };
        }
        Err(e) => return failed(row, format!("{e:#}")),
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
        "{}/{}/releases/download/{tag}/{name}-{arch}.tar.gz",
        github.downloads, adapter.repo
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
        Err(e) => return failed(row, format!("{e:#}")),
    };
    let sums = String::from_utf8_lossy(&sums);
    match install(
        adapter, units, name, &tag, &archive, &sums, gateway_up, host, watch,
    )
    .await
    {
        Ok((result, left)) => {
            adapter.version = Some(tag.trim_start_matches('v').to_string());
            row.result = result;
            apart.extend(left);
            row
        }
        Err(e) => failed(row, e),
    }
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

/// What the adapters' step works on, gathered before the gateway's: the
/// list as read from `path`, the list with what was found added, the
/// adapters found but not managed with why, and why the list could not be
/// read.
#[derive(Debug)]
struct Gathered {
    path: PathBuf,
    read: Components,
    list: Components,
    unmanaged: Vec<(Adapter, String)>,
    unread: Option<String>,
}

/// Read the list at `path` and add what units in `unit_dirs` and `packs`
/// show; with `check`, a list that cannot be read is said, not an error.
fn gather(
    path: &Path,
    unit_dirs: &[&Path],
    packs: &Path,
    host: &impl Host,
    check: bool,
) -> anyhow::Result<Gathered> {
    let (read, unread) = match Components::read(path) {
        Ok(list) => (list, None),
        Err(e) if check => (Components::default(), Some(format!("{e:#}"))),
        Err(e) => return Err(e.context("--only gateway updates the gateway without it")),
    };
    let mut list = read.clone();
    let (found, rooted) = discover(unit_dirs, packs, host);
    let mut unmanaged = list.add(found);
    // A unit with its own root is said only when the list does not name it.
    let listed = |unit: &str| {
        list.adapters
            .iter()
            .flat_map(|a| &a.units)
            .any(|u| u == unit || host.exec(u).id == unit)
    };
    unmanaged.extend(
        rooted
            .into_iter()
            .filter(|(adapter, _)| !adapter.units.iter().any(|u| listed(u))),
    );
    Ok(Gathered {
        path: path.to_path_buf(),
        read,
        list,
        unmanaged,
        unread,
    })
}

/// The adapters' and packs' step of [`run`], after the gateway's (which
/// runs `gateway` now): a row for each. The list is written back when it
/// changed, unless `check`.
async fn update_components(
    http: &reqwest::Client,
    github: &Github,
    mut gathered: Gathered,
    gateway: &str,
    gateway_up: bool,
    check: bool,
    host: &impl Host,
    watch: Watch,
) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Some(why) = &gathered.unread {
        rows.push(Row::new(
            "components.toml",
            "-",
            "-",
            &format!("not read: {why}"),
        ));
    }
    for adapter in &mut gathered.list.adapters {
        let (units, mut apart) = if adapter.skip || !adapter.binary.exists() {
            (Vec::new(), Vec::new())
        } else {
            let binary = adapter
                .binary
                .canonicalize()
                .unwrap_or_else(|_| adapter.binary.clone());
            units_of(adapter, &binary, host)
        };
        rows.push(
            adapter_step(
                http, github, adapter, &units, &mut apart, gateway, check, gateway_up, host, watch,
            )
            .await,
        );
        for result in apart {
            rows.push(Row::new(&adapter.name, "-", "-", &result));
        }
    }
    for (adapter, why) in &gathered.unmanaged {
        rows.push(Row::new(
            &adapter.name,
            "-",
            "-",
            &format!("not managed: {why}"),
        ));
    }
    for pack in &gathered.list.packs {
        let result = if pack.skip {
            format!("skipped: skip = true in {COMPONENTS}")
        } else {
            "not updated by havn update: run its setup.sh".to_string()
        };
        rows.push(Row::new(
            &pack.name,
            pack.version.as_deref().unwrap_or("-"),
            "-",
            &result,
        ));
    }
    if !check
        && gathered.list != gathered.read
        && let Err(e) = gathered.list.write(&gathered.path)
    {
        eprintln!("{e:#}");
    }
    rows
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
        let swapped = swap(dir.path(), from.path(), &names, false).unwrap();
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
        assert!(swap(dir.path(), from.path(), &names, false).is_err());
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
    fn a_new_binary_keeps_the_old_one_s_mode_and_an_adapter_s_own_name_is_unpacked() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let from = tempfile::tempdir().unwrap();
        let old = dir.path().join("havn-channel-x");
        std::fs::write(&old, "old").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o750)).unwrap();
        for name in ["havn-channel-x", "havn-channel-y"] {
            std::fs::write(from.path().join(name), "new").unwrap();
        }
        let names = vec!["havn-channel-x".to_string(), "havn-channel-y".to_string()];
        swap(dir.path(), from.path(), &names, true).unwrap();
        let mode = |name: &str| {
            std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("havn-channel-x"), 0o750);
        assert_eq!(mode("havn-channel-y"), 0o755, "none before it");
        // The gateway's binaries are 755, as before.
        swap(dir.path(), from.path(), &names, false).unwrap();
        assert_eq!(mode("havn-channel-x"), 0o755);

        let to = tempfile::tempdir().unwrap();
        let entries = files(&["tg-bridge", "README"]);
        assert_eq!(
            unpack(&of(&entries), to.path(), &["tg-bridge"]).unwrap(),
            vec!["tg-bridge"]
        );
        assert!(unpack(&of(&entries), to.path(), &[]).unwrap().is_empty());
    }

    #[test]
    fn units_that_run_an_adapter_as_systemd_has_them_and_packs_are_found() {
        let shown = "Id=tg.service\nExecStart={ path=/usr/local/bin/havn-channel-telegram ; \
                     argv[]=/usr/local/bin/havn-channel-telegram --config /etc/c.toml ; \
                     ignore_errors=no ; pid=0 }\nExecStart={ path=/opt/y ; argv[]=/opt/y }\n";
        assert_eq!(
            parse_exec(&format!("{shown}RootDirectory=\nRootImage=\n")),
            Exec {
                id: "tg.service".to_string(),
                binaries: vec![
                    PathBuf::from("/usr/local/bin/havn-channel-telegram"),
                    PathBuf::from("/opt/y")
                ],
                root: None,
            }
        );
        assert_eq!(parse_exec(""), Exec::default());
        let argv = "ExecStart={ path=/usr/bin/env ; argv[]=/usr/bin/env x                     --state-path=/usr/local/bin/havn-channel-x ; ignore_errors=no }
";
        assert_eq!(
            parse_exec(argv).binaries,
            vec![PathBuf::from("/usr/bin/env")]
        );
        assert_eq!(
            parse_exec(&format!("{shown}RootDirectory=/srv/jail\nRootImage=\n")).root,
            Some("RootDirectory=/srv/jail".to_string())
        );
        assert_eq!(
            parse_exec(&format!("{shown}RootDirectory=\nRootImage=/srv/tg.raw\n")).root,
            Some("RootImage=/srv/tg.raw".to_string())
        );
        assert_eq!(parse_exec("RootDirectory=/\n").root, None, "the host's own");

        let etc = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let mentions = "[Service]\nExecStart=/usr/local/bin/havn-channel-telegram\n";
        for (dir, unit, text) in [
            (etc.path(), "tg.service", mentions),
            (etc.path(), "alias.service", mentions),
            (
                etc.path(),
                "tg2.service",
                "[Service]\nExecStart=/usr/bin/true\n",
            ),
            (
                etc.path(),
                "odd.service",
                "ExecStart=/opt/havn-channel-odd\n",
            ),
            (
                etc.path(),
                "plain.service",
                "[Service]\nExecStart=/usr/bin/true\n",
            ),
            (etc.path(), "tpl@.service", mentions),
            (etc.path(), "dc.timer", mentions),
            (etc.path(), "jail.service", mentions),
            (
                lib.path(),
                "dc.service",
                "ExecStart=/usr/local/bin/havn-channel-discord\n",
            ),
        ] {
            std::fs::write(dir.join(unit), text).unwrap();
        }
        // A drop-in that makes a unit run an adapter.
        std::fs::create_dir(etc.path().join("tg2.service.d")).unwrap();
        std::fs::write(
            etc.path().join("tg2.service.d/override.conf"),
            "[Service]\nExecStart=\nExecStart=/usr/local/bin/havn-channel-telegram\n",
        )
        .unwrap();
        assert_eq!(
            candidate_units(&[etc.path(), lib.path()]),
            vec![
                "alias.service",
                "dc.service",
                "jail.service",
                "odd.service",
                "tg.service",
                "tg2.service"
            ]
        );

        let packs = tempfile::tempdir().unwrap();
        for dir in ["playwright-cli", "chromium", "maps", "empty"] {
            std::fs::create_dir(packs.path().join(dir)).unwrap();
        }
        std::fs::write(packs.path().join("maps/SKILL.md"), "skill").unwrap();
        std::fs::write(packs.path().join("notes"), "a file").unwrap();

        let telegram = "/usr/local/bin/havn-channel-telegram";
        let fake = Fake {
            execs: vec![
                ("tg.service", "tg.service", telegram),
                ("alias.service", "tg.service", telegram),
                ("tg2.service", "tg2.service", telegram),
                ("odd.service", "odd.service", "/opt/havn-channel-odd"),
                (
                    "plain.service",
                    "plain.service",
                    "/usr/local/bin/havn-channel-plain",
                ),
                (
                    "dc.service",
                    "dc.service",
                    "/usr/local/bin/havn-channel-discord",
                ),
                ("jail.service", "jail.service", telegram),
            ],
            roots: vec![("jail.service", "RootDirectory=/srv/jail")],
            ..Fake::new(Path::new("/nonexistent"))
        };
        let (found, rooted) = discover(&[etc.path(), lib.path()], packs.path(), &fake);
        let adapters: Vec<(&str, &str, Vec<&str>)> = found
            .adapters
            .iter()
            .map(|a| {
                (
                    a.name.as_str(),
                    a.repo.as_str(),
                    a.units.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            adapters,
            vec![
                (
                    "havn-channel-telegram",
                    "amplimit/havn-channel-telegram",
                    vec!["tg.service", "tg2.service"]
                ),
                (
                    "havn-channel-discord",
                    "amplimit/havn-channel-discord",
                    vec!["dc.service"]
                ),
                ("havn-channel-odd", "", vec!["odd.service"]),
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

        // A unit that runs it inside another root is not one of telegram's.
        let rooted: Vec<(&str, Vec<&str>, &str)> = rooted
            .iter()
            .map(|(a, why)| {
                (
                    a.name.as_str(),
                    a.units.iter().map(String::as_str).collect(),
                    why.as_str(),
                )
            })
            .collect();
        assert_eq!(
            rooted,
            vec![(
                "havn-channel-telegram",
                vec!["jail.service"],
                "jail.service runs it under RootDirectory=/srv/jail; list it in \
                 /etc/havn/components.toml, with its binary as this machine has it, to update it"
            )]
        );

        // Only one in the releases' directory is taken without being listed.
        let mut list = Components::default();
        let unmanaged = list.add(found);
        assert_eq!(list.adapters.len(), 2);
        assert_eq!(unmanaged.len(), 1);
        assert_eq!(unmanaged[0].0.binary, Path::new("/opt/havn-channel-odd"));
        assert_eq!(
            unmanaged[0].1,
            "/opt/havn-channel-odd is not in /usr/local/bin; list it in \
             /etc/havn/components.toml to update it"
        );
    }

    fn listed(name: &str, binary: &str, units: &[&str]) -> Adapter {
        Adapter {
            name: name.into(),
            repo: format!("amplimit/{name}"),
            binary: binary.into(),
            units: units.iter().map(|u| (*u).to_string()).collect(),
            version: None,
            skip: false,
        }
    }

    #[test]
    fn the_list_keeps_what_it_says_gains_what_was_found_and_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("havn/components.toml");
        assert_eq!(Components::read(&path).unwrap(), Components::default());
        let mut list = Components {
            adapters: vec![Adapter {
                name: "tg".into(),
                repo: String::new(),
                version: Some("0.2.9".into()),
                skip: true,
                ..listed(
                    "tg",
                    "/usr/local/bin/havn-channel-telegram",
                    &["tg.service"],
                )
            }],
            packs: vec![Pack {
                name: "maps".into(),
                repo: "someone/maps".into(),
                path: "/opt/havn/maps".into(),
                version: None,
                skip: false,
            }],
        };
        list.write(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# The channel adapters") && text.contains("skip = true"));
        assert_eq!(Components::read(&path).unwrap(), list);

        list.add(Components {
            adapters: vec![
                listed(
                    "havn-channel-telegram",
                    "/usr/local/bin/havn-channel-telegram",
                    &["tg.service", "tg2.service"],
                ),
                listed(
                    "havn-channel-discord",
                    "/usr/local/bin/havn-channel-discord",
                    &["dc.service"],
                ),
            ],
            packs: vec![Pack {
                name: "other-name".into(),
                repo: String::new(),
                path: "/opt/havn/maps".into(),
                version: None,
                skip: false,
            }],
        });
        assert_eq!(list.adapters.len(), 2);
        let tg = &list.adapters[0];
        assert_eq!(tg.name, "tg", "its name as listed");
        assert_eq!(tg.repo, "amplimit/havn-channel-telegram");
        assert_eq!(tg.units, vec!["tg.service", "tg2.service"]);
        assert_eq!(tg.version.as_deref(), Some("0.2.9"));
        assert_eq!(list.packs.len(), 1, "the same pack by its path");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        list.write(&path).unwrap();
        assert_eq!(mode(&path), 0o640, "an existing file's mode is kept");
        assert_eq!(Components::read(&path).unwrap(), list);

        assert!(Components::read(path.parent().unwrap()).is_err());
        std::fs::write(&path, "[[adapter]]\nname = 1\n").unwrap();
        assert!(Components::read(&path).is_err());
        std::fs::write(
            &path,
            "[[adapter]]\nname = \"x\"\nbinary = \"/usr/local/bin/x\"\nunit = \"x.service\"\n",
        )
        .unwrap();
        assert!(Components::read(&path).is_err(), "an unknown key");
    }

    #[test]
    fn a_release_is_installed_only_when_newer_and_the_gateway_is_new_enough() {
        for (notes, wanted) in [
            ("## Changes\n* Requires Havn >= 0.2.12.\n", "0.2.12"),
            ("**Requires Havn ≥ `v0.3.0`**", "0.3.0"),
            ("Requires: Havn >= 0.2.13", "0.2.13"),
            (
                "This release requires Havn >= 0.2.13 (for threads).",
                "0.2.13",
            ),
            ("requires havn>=0.2.14", "0.2.14"),
            ("- Requires _Havn_ 0.2.15 or later", "0.2.15"),
            ("Requires Havn version 0.2.13", "0.2.13"),
            ("requires havn 0.2.13", "0.2.13"),
            ("Requires Havn v0.2.13+", "0.2.13"),
            ("Requires at least Havn 0.2.16.", "0.2.16"),
            ("Requires Havn version >= v0.2.17", "0.2.17"),
            ("Requires Havn (>= 0.2.18)", "0.2.18"),
            (
                "This release requires Havn at least version 0.2.19.",
                "0.2.19",
            ),
            (
                "Requires Havn [v0.2.20](https://x/releases/v0.2.20)",
                "0.2.20",
            ),
            (
                "It requires a restart and requires Havn >= 0.2.21",
                "0.2.21",
            ),
            ("Requires Havn > 0.2.13", "0.2.14"),
            ("Requires Havn gateway >= 0.2.14", "0.2.14"),
            ("Requires the Havn gateway => 0.2.14", "0.2.14"),
            ("Requires:\n- Havn >= 0.2.14\n- a bot token", "0.2.14"),
            (
                "## Requirements\n\n- Havn ~0.2.14\n- Rust\n\n## Changes\n- Havn 0.3.0 too",
                "0.2.14",
            ),
            ("Requires Havn:\n1. 0.2.14 or later", "0.2.14"),
            ("Minimum Havn version: 0.2.14", "0.2.14"),
            ("Needs Havn ^0.2.14", "0.2.14"),
            ("需要 Havn ≧ 0.2.14", "0.2.14"),
            ("要求Havn版本 v0.2.14", "0.2.14"),
            ("Requires Havn 0.2.12\nRequires Havn >= 0.2.14", "0.2.14"),
            ("Requires Havn >= 0.2.12, < 0.3.0", "0.2.12"),
            ("Requires Havn 0.2.12 up to 0.2.20", "0.2.12"),
            // Only the version right after Havn is what it asks for.
            (
                "Needs Rust 1.85.0 to build; works with Havn 0.2.10",
                "0.2.10",
            ),
            ("Requires Havn 0.2.14, tested on 0.2.30", "0.2.14"),
            (
                "Requires Havn 0.2.14 or later (0.2.16 recommended)",
                "0.2.14",
            ),
            ("Requires Havn 0.2.14 to 0.2.99", "0.2.14"),
            ("Requires Havn 0.2.14 - 0.2.99", "0.2.14"),
            ("Requires Havn between 0.2.14 and 0.3.0", "0.2.14"),
            ("Requires Havn 0.2.14 or 0.3.0", "0.2.14"),
            ("Requires Havn >= 0.2.14 (Havn's new API)", "0.2.14"),
            ("Requires:\n\n- Havn >= 0.2.14", "0.2.14"),
            ("Requires:\nSee below\n- Havn >= 0.2.14", "0.2.14"),
            ("Requires:\n\nSee below\n- Havn >= 0.2.14", "0.2.14"),
            ("Requires Havn-0.2.14", "0.2.14"),
            ("Requires Havn-v0.2.14+", "0.2.14"),
            ("Requires Havn 0.2.14+", "0.2.14"),
        ] {
            assert_eq!(requires(notes), Some(Ok(wanted.to_string())), "{notes}");
        }
        // A line that asks but names no version skips the release, even
        // when it did not mean to.
        for unreadable in [
            "Requires Havn >= soon",
            "Requires Havn version next",
            "Requires at least Havn v2",
            "This requires Havn to restart.",
            "Requires Havn version checks to pass",
            "Requires Havn version checks to pass (from 0.2.12)",
            "requires Havn 1.0.0-beta",
            "Requires Havn 0.2.14+build.5",
            "Requires Havn 0.2.14-0.2.99",
        ] {
            assert_eq!(
                requires(unreadable),
                Some(Err(unreadable.to_string())),
                "{unreadable}"
            );
        }
        assert_eq!(
            requires("Requires Havn >= 0.2.12\n## Requirements\n- Havn, running"),
            Some(Err("- Havn, running".to_string()))
        );
        assert_eq!(
            requires("Requires Havn:\n- a bot token\n\nThanks"),
            Some(Err("Requires Havn:".to_string())),
            "its list names no version"
        );
        assert_eq!(requires(""), None);
        assert_eq!(requires("Works with any Havn"), None);
        for unrelated in [
            "Logging in requires havn-gateway auth.",
            "Prerequires havn >= 0.2.12",
            "Requireshavn >= 0.2.12",
            "No longer requires Havn 0.3.0",
            "Havn 0.2.12 is not required",
            "不再需要 Havn 0.3.0",
            "## Requirements\n- Rust 1.80",
            "Requires:\n- a bot token\n\n- Havn 0.3.0 is out",
            "## Changes\n- Works with Havn 0.3.0",
            "Havn 0.2.13 requires a restart",
            // A list closed by a blank line after its text, or by bold.
            "Requirements:\nNone beyond the last release.\n\nWhat changed\n- Havn 0.2.99 support for \
             threads",
            "Requires:\n**What changed**\n- Havn 0.2.99 support for threads",
            "Requires:\n\n**Full Changelog**: https://x/compare/v0.2.13...v0.2.14\n- Havn 0.2.99 \
             support for threads",
        ] {
            assert_eq!(requires(unrelated), None, "{unrelated}");
        }
        assert_eq!(
            requires("Requires havn-gateway auth\nRequires Havn >= 0.2.12"),
            Some(Ok("0.2.12".to_string())),
            "a later line still says"
        );

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
        assert!(matches!(
            plan(Some("0.2.9"), "v0.2.13", "Requires Havn >= next", "0.2.12"),
            Plan::Skip(why) if why.contains("cannot be read")
        ));
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

    /// A machine whose adapter units run a binary that holds its version as
    /// text. On the new binary (not `old`), the units in `failing` fail,
    /// systemd restarts them on its own with `flapping`, and they connect
    /// only with `connects`. They run `elsewhere` when it is set, ran
    /// before when `running`, and the old binary goes missing on the first
    /// restart with `lose_old`. For the first `executor` looks after each
    /// restart, the process is still systemd's. With `inside`, the binary
    /// is seen at that path, as from a unit's own root; the units in
    /// `apart` run another file at the binary's path (those in `after` once
    /// restarted), and until a restart those in `pids` run as that process
    /// (none, stopped, for 0).
    /// `execs` are the units' `ExecStart=`, and `roots` their
    /// `RootDirectory=` or `RootImage=`.
    struct Fake {
        binary: PathBuf,
        old: &'static str,
        failing: Vec<&'static str>,
        flapping: bool,
        connects: bool,
        running: bool,
        elsewhere: Option<PathBuf>,
        inside: Option<PathBuf>,
        lose_old: bool,
        executor: u32,
        looks: std::cell::Cell<u32>,
        execs: Vec<(&'static str, &'static str, &'static str)>,
        roots: Vec<(&'static str, &'static str)>,
        apart: Vec<&'static str>,
        pids: Vec<(&'static str, u32)>,
        after: Vec<&'static str>,
        restarted: std::cell::RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(binary: &Path) -> Self {
            Self {
                binary: binary.to_path_buf(),
                old: "0.2.9",
                failing: Vec::new(),
                flapping: false,
                connects: true,
                running: true,
                elsewhere: None,
                inside: None,
                lose_old: false,
                executor: 0,
                looks: std::cell::Cell::new(0),
                execs: Vec::new(),
                roots: Vec::new(),
                apart: Vec::new(),
                pids: Vec::new(),
                after: Vec::new(),
                restarted: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn installed(&self) -> String {
            std::fs::read_to_string(&self.binary)
                .unwrap_or_default()
                .trim()
                .to_string()
        }

        fn fresh(&self) -> bool {
            self.installed() != self.old
        }
    }

    impl Host for Fake {
        fn version(&self, binary: &Path, _unprivileged: bool) -> Option<String> {
            version_in(&std::fs::read_to_string(binary).ok()?)
        }

        fn exec(&self, unit: &str) -> Exec {
            self.execs.iter().find(|(u, ..)| *u == unit).map_or_else(
                Exec::default,
                |(_, id, binary)| Exec {
                    id: (*id).to_string(),
                    binaries: vec![PathBuf::from(binary)],
                    root: self
                        .roots
                        .iter()
                        .find(|(u, _)| *u == unit)
                        .map(|(_, root)| (*root).to_string()),
                },
            )
        }

        fn show(&self, unit: &str) -> Shown {
            let started = !self.restarted.borrow().is_empty();
            let before = self
                .pids
                .iter()
                .find(|(u, _)| !started && *u == unit)
                .map(|(_, pid)| *pid);
            let state = if !started {
                if self.running && before != Some(0) {
                    "active"
                } else {
                    "inactive"
                }
            } else if self.fresh() && self.failing.contains(&unit) {
                "failed"
            } else {
                "active"
            };
            Shown {
                state: state.into(),
                restarts: u64::from(started && self.fresh() && self.flapping),
                invocation: format!("{:032x}", self.restarted.borrow().len()),
                pid: if self.apart.contains(&unit) || (started && self.after.contains(&unit)) {
                    APART
                } else {
                    before.unwrap_or(42)
                },
            }
        }

        fn restart(&self, unit: &str) -> anyhow::Result<u64> {
            if self.lose_old {
                let _ = std::fs::remove_file(self.binary.with_file_name("havn-channel-test.old"));
            }
            self.restarted.borrow_mut().push(unit.to_string());
            self.looks.set(0);
            Ok(0)
        }

        fn logged(&self, _invocation: &str, line: &str) -> bool {
            line == CONNECTED && (self.connects || !self.fresh())
        }

        fn exe(&self, pid: u32, binary: &Path) -> Option<(PathBuf, bool)> {
            let old = self.binary.with_file_name("havn-channel-test.old");
            match pid {
                APART => return Some((self.binary.clone(), false)),
                DELETED => {
                    return Some((format!("{} (deleted)", self.binary.display()).into(), false));
                }
                OLDER => return Some(("/srv/bind/havn-channel-test".into(), binary == old)),
                _ => {}
            }
            let looks = self.looks.get();
            self.looks.set(looks + 1);
            if looks < self.executor {
                return Some((EXECUTOR.into(), false));
            }
            Some(match &self.elsewhere {
                Some(path) => (path.clone(), false),
                None => (
                    self.inside.clone().unwrap_or_else(|| self.binary.clone()),
                    binary == self.binary,
                ),
            })
        }
    }

    const EXECUTOR: &str = "/usr/lib/systemd/systemd-executor";

    /// The process of a unit in [`Fake::apart`].
    const APART: u32 = 43;

    /// A process that runs the binary before it was replaced, read as
    /// deleted, and one that runs it as `<binary>.old`, seen from its root.
    const DELETED: u32 = 44;
    const OLDER: u32 = 45;

    const QUICK: Watch = Watch {
        settle: Duration::ZERO,
        within: Duration::from_millis(30),
        poll: Duration::from_millis(1),
    };

    /// The archive of an adapter's release `version` and its checksum.
    fn release_of(version: &str) -> (Vec<u8>, String) {
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
        (out, sum)
    }

    /// An adapter installed at version 0.2.9 in `dir`, with the archive and
    /// checksum of its release `version`.
    fn adapter_at(dir: &Path, version: &str) -> (Adapter, Vec<u8>, String) {
        let binary = dir.canonicalize().unwrap().join("havn-channel-test");
        std::fs::write(&binary, "0.2.9").unwrap();
        let (archive, sum) = release_of(version);
        let adapter = Adapter {
            binary,
            ..listed("havn-channel-test", "", &["test.service"])
        };
        (adapter, archive, sum)
    }

    async fn install_on(
        fake: &Fake,
        adapter: &Adapter,
        archive: &[u8],
        sums: &str,
        gateway_up: bool,
        watch: Watch,
    ) -> Result<String, String> {
        let (units, _) = units_of(adapter, &adapter.binary, fake);
        install(
            adapter,
            &units,
            "havn-channel-test",
            "v0.2.11",
            archive,
            sums,
            gateway_up,
            fake,
            watch,
        )
        .await
        .map(|(done, apart)| {
            std::iter::once(done)
                .chain(apart)
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    #[tokio::test]
    async fn an_adapter_is_installed_restarted_and_kept_when_it_comes_up() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake::new(&adapter.binary);
        let watch = Watch {
            settle: Duration::from_millis(20),
            within: Duration::from_millis(500),
            poll: Duration::from_millis(1),
        };
        let started = Instant::now();
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums, true, watch).await,
            Ok("updated".into())
        );
        assert!(started.elapsed() >= watch.settle, "watched for a while");
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
            install_on(&fake, &adapter, &archive, &sums, true, QUICK).await,
            Ok("installed; no unit of it runs".into())
        );
        assert_eq!(fake.installed(), "0.2.11");
        assert!(fake.restarted.borrow().is_empty());

        // With no gateway answering, it need not connect.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            connects: false,
            ..Fake::new(&adapter.binary)
        };
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums, false, QUICK).await,
            Ok("updated".into())
        );

        // Still systemd's for a moment after each restart: not a failure.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            executor: 3,
            ..Fake::new(&adapter.binary)
        };
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums, true, watch).await,
            Ok("updated".into())
        );
        assert_eq!(fake.installed(), "0.2.11");

        // A unit with its own root runs the file at another path.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            inside: Some("/usr/local/bin/havn-channel-test".into()),
            ..Fake::new(&adapter.binary)
        };
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums, true, QUICK).await,
            Ok("updated".into())
        );
        assert_eq!(fake.installed(), "0.2.11");

        // A unit under two names is restarted once.
        let (mut adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        adapter.units.push("alias.service".into());
        let fake = Fake {
            execs: vec![("alias.service", "test.service", "")],
            ..Fake::new(&adapter.binary)
        };
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums, true, QUICK).await,
            Ok("updated".into())
        );
        assert_eq!(*fake.restarted.borrow(), vec!["test.service"]);
    }

    #[test]
    fn what_a_process_runs_is_told_by_its_file_not_its_path() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (runs, _) = System.exe(child.id(), Path::new("/nonexistent")).unwrap();
        // The same file at another path, as a unit's own root would show it.
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("sleep");
        std::os::unix::fs::symlink(&runs, &elsewhere).unwrap();
        let seen = [
            System.exe(child.id(), &runs),
            System.exe(child.id(), &elsewhere),
            System.exe(child.id(), &std::env::current_exe().unwrap()),
            System.exe(child.id(), Path::new("/nonexistent")),
        ];
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            seen.map(|s| s.unwrap()),
            [
                (runs.clone(), true),
                (runs.clone(), true),
                (runs.clone(), false),
                (runs, false)
            ]
        );
    }

    #[tokio::test]
    async fn a_unit_of_its_own_root_is_restarted_only_when_it_runs_this_binary() {
        let dir = tempfile::tempdir().unwrap();
        let rooted = |adapter: &Adapter, pids: Vec<(&'static str, u32)>| Fake {
            execs: vec![("bind.service", "bind.service", "")],
            roots: vec![("bind.service", "RootDirectory=/srv/bind")],
            pids,
            ..Fake::new(&adapter.binary)
        };

        // It runs the binary as it was before the last update: deleted, or
        // kept as `.old`. It is restarted on this one.
        for pid in [DELETED, OLDER] {
            let (mut adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
            adapter.units.push("bind.service".into());
            let fake = rooted(&adapter, vec![("bind.service", pid)]);
            assert_eq!(
                units_of(&adapter, &adapter.binary, &fake),
                (vec!["test.service".into(), "bind.service".into()], vec![]),
                "{pid}"
            );
            assert_eq!(
                install_on(&fake, &adapter, &archive, &sums, true, QUICK).await,
                Ok("updated".into())
            );
            assert_eq!(
                *fake.restarted.borrow(),
                vec!["test.service", "bind.service"]
            );
        }

        // Taken as on the binary before, but once restarted it runs one of
        // its own root's: left as it is, the host's unit updated.
        let (mut adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        adapter.units.push("bind.service".into());
        let fake = Fake {
            after: vec!["bind.service"],
            ..rooted(&adapter, vec![("bind.service", DELETED)])
        };
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums, true, QUICK).await,
            Ok(
                "updated\nnot managed: bind.service runs another binary under \
                RootDirectory=/srv/bind once restarted, though /etc/havn/components.toml lists \
                it under havn-channel-test; list it in /etc/havn/components.toml on its own, \
                with its binary as this machine has it, to update it"
                    .into()
            )
        );
        assert_eq!(fake.installed(), "0.2.11");
        assert_eq!(
            *fake.restarted.borrow(),
            vec!["test.service", "bind.service"]
        );

        // When it is the only one, no unit runs the binary put in place.
        let (mut adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        adapter.units = vec!["bind.service".into()];
        let fake = Fake {
            after: vec!["bind.service"],
            ..rooted(&adapter, vec![("bind.service", DELETED)])
        };
        let done = install_on(&fake, &adapter, &archive, &sums, true, QUICK)
            .await
            .unwrap();
        assert!(
            done.starts_with("installed; no unit of it runs it\nnot managed: bind.service"),
            "{done}"
        );
        assert_eq!(fake.installed(), "0.2.11");

        // So when the host's unit then fails, only it goes back.
        let (mut adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        adapter.units.insert(0, "bind.service".into());
        let fake = Fake {
            after: vec!["bind.service"],
            failing: vec!["test.service"],
            ..rooted(&adapter, vec![("bind.service", DELETED)])
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(
            failed.starts_with("test.service is failed") && failed.ends_with("old binary is back"),
            "{failed}"
        );
        assert_eq!(fake.installed(), "0.2.9");
        assert_eq!(
            *fake.restarted.borrow(),
            vec!["bind.service", "test.service", "test.service"]
        );

        // Stopped, so what it runs cannot be told: left as it is.
        let (mut adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        adapter.units.push("bind.service".into());
        let fake = rooted(&adapter, vec![("bind.service", 0)]);
        assert_eq!(
            units_of(&adapter, &adapter.binary, &fake),
            (
                vec!["test.service".into()],
                vec![
                    "not checked: bind.service runs under RootDirectory=/srv/bind and is \
                     inactive, so what it runs cannot be told; it is left as it is"
                        .into()
                ]
            )
        );
        assert_eq!(
            install_on(&fake, &adapter, &archive, &sums, true, QUICK).await,
            Ok("updated".into())
        );
        assert_eq!(*fake.restarted.borrow(), vec!["test.service"]);

        // A binary of its own: left alone, its adapter's other units not.
        let (mut adapter, _, _) = adapter_at(dir.path(), "0.2.11");
        adapter.units.push("bind.service".into());
        let fake = Fake {
            apart: vec!["bind.service"],
            ..rooted(&adapter, Vec::new())
        };
        let (units, apart) = units_of(&adapter, &adapter.binary, &fake);
        assert_eq!(units, vec!["test.service"]);
        assert!(
            apart[0].starts_with(
                "not managed: bind.service runs another binary under RootDirectory=/srv/bind"
            ),
            "{apart:?}"
        );
    }

    async fn fails(fake: &Fake, adapter: &Adapter, archive: &[u8], sums: &str) -> String {
        install_on(fake, adapter, archive, sums, true, QUICK)
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn an_adapter_that_does_not_come_up_gets_its_old_binary_back() {
        let dir = tempfile::tempdir().unwrap();

        // It fails to start: back to the old one, restarted on it.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            failing: vec!["test.service"],
            ..Fake::new(&adapter.binary)
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(
            failed.contains("test.service is failed") && failed.ends_with("old binary is back"),
            "{failed}"
        );
        assert_eq!(fake.installed(), "0.2.9");
        assert_eq!(
            *fake.restarted.borrow(),
            vec!["test.service", "test.service"]
        );

        // systemd restarts it on its own.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            flapping: true,
            ..Fake::new(&adapter.binary)
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(failed.contains("systemd restarted it"), "{failed}");
        assert_eq!(fake.installed(), "0.2.9");

        // It runs but does not connect, as the old one had.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            connects: false,
            ..Fake::new(&adapter.binary)
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(
            failed.contains("did not connect to the gateway"),
            "{failed}"
        );
        assert_eq!(fake.installed(), "0.2.9");

        // The unit runs another binary (a drop-in, say).
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            elsewhere: Some("/usr/bin/havn-channel-test".into()),
            ..Fake::new(&adapter.binary)
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(
            failed.contains("runs /usr/bin/havn-channel-test, not"),
            "{failed}"
        );
        assert_eq!(fake.installed(), "0.2.9");

        // Another file at the same path: the one in the unit's own root.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            elsewhere: Some(adapter.binary.clone()),
            ..Fake::new(&adapter.binary)
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(failed.contains("than this machine has there"), "{failed}");
        assert_eq!(fake.installed(), "0.2.9");

        // Still systemd's when it has had its time: not the binary.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            executor: u32::MAX,
            ..Fake::new(&adapter.binary)
        };
        let watch = Watch {
            settle: Duration::from_millis(20),
            within: Duration::from_millis(500),
            poll: Duration::from_millis(1),
        };
        let started = Instant::now();
        let failed = install_on(&fake, &adapter, &archive, &sums, true, watch)
            .await
            .unwrap_err();
        assert!(
            failed.contains(&format!("runs {EXECUTOR}, not")),
            "{failed}"
        );
        assert!(started.elapsed() >= watch.settle, "not before the settle");
        assert_eq!(fake.installed(), "0.2.9");

        // A unit of its own root listed under it is left alone, going back
        // too.
        let (mut adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        adapter.units.push("jail.service".into());
        let fake = Fake {
            failing: vec!["test.service"],
            execs: vec![("jail.service", "jail.service", "")],
            roots: vec![("jail.service", "RootDirectory=/srv/jail")],
            apart: vec!["jail.service"],
            ..Fake::new(&adapter.binary)
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(
            failed.starts_with("test.service is failed") && failed.ends_with("old binary is back"),
            "{failed}"
        );
        assert_eq!(
            *fake.restarted.borrow(),
            vec!["test.service", "test.service"]
        );

        // The second unit fails after the first came up: both go back.
        let (mut adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        adapter.units.push("second.service".into());
        let fake = Fake {
            failing: vec!["second.service"],
            ..Fake::new(&adapter.binary)
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(failed.starts_with("second.service is failed"), "{failed}");
        assert_eq!(fake.installed(), "0.2.9");
        assert_eq!(
            *fake.restarted.borrow(),
            vec![
                "test.service",
                "second.service",
                "test.service",
                "second.service"
            ]
        );

        // The old binary cannot be put back.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake {
            failing: vec!["test.service"],
            lose_old: true,
            ..Fake::new(&adapter.binary)
        };
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(
            failed.contains("putting the old binary back failed"),
            "{failed}"
        );
        assert_eq!(fake.restarted.borrow().len(), 1, "not restarted again");

        // Not the release it should be: put back before any restart.
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.10");
        let fake = Fake::new(&adapter.binary);
        let failed = fails(&fake, &adapter, &archive, &sums).await;
        assert!(failed.contains("says it is 0.2.10"), "{failed}");
        assert_eq!(fake.installed(), "0.2.9");
        assert!(fake.restarted.borrow().is_empty());

        // A download that is not the release's: nothing is touched.
        let (adapter, archive, _) = adapter_at(dir.path(), "0.2.11");
        let fake = Fake::new(&adapter.binary);
        fails(&fake, &adapter, &archive, "00").await;
        assert_eq!(fake.installed(), "0.2.9");
        assert!(fake.restarted.borrow().is_empty());
    }

    /// An HTTP server answering each path in `routes` with its status and
    /// body, and 404 to any other; its address.
    async fn serve(routes: Vec<(String, u16, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let (status, body) = routes
                    .iter()
                    .find(|(p, ..)| p == path)
                    .map_or((404, &[][..]), |(_, status, body)| (*status, &body[..]));
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body).await;
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn the_adapters_in_the_list_are_updated_and_the_list_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, archive, sums) = adapter_at(dir.path(), "0.2.11");
        let bin = adapter.binary.parent().unwrap().to_path_buf();
        for name in ["gone", "empty", "later", "held"] {
            std::fs::write(bin.join(format!("havn-channel-{name}")), "0.2.9").unwrap();
        }
        let arch = release::archive_for(std::env::consts::ARCH)
            .unwrap()
            .strip_prefix("havn-")
            .unwrap()
            .strip_suffix(".tar.gz")
            .unwrap();
        let download = format!(
            "/amplimit/havn-channel-test/releases/download/v0.2.11/havn-channel-test-{arch}.tar.gz"
        );
        let base = serve(vec![
            (
                "/repos/amplimit/havn-channel-test/releases/latest".into(),
                200,
                br#"{"tag_name":"v0.2.11","body":"Requires Havn >= 0.2.12"}"#.to_vec(),
            ),
            (download.clone(), 200, archive),
            (
                format!("{download}.sha256"),
                200,
                format!("{sums}  havn-channel-test.tar.gz\n").into_bytes(),
            ),
            ("/repos/amplimit/empty".into(), 200, b"{}".to_vec()),
            (
                "/repos/amplimit/later/releases/latest".into(),
                200,
                br#"{"tag_name":"v0.3.0","body":"Requires Havn >= 0.3.0"}"#.to_vec(),
            ),
        ])
        .await;
        let github = Github {
            api: base.clone(),
            downloads: base,
        };
        let path = dir.path().join("etc/components.toml");
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        let entry = |name: &str, repo: &str, extra: &str| {
            format!(
                "[[adapter]]\nname = \"{name}\"\nrepo = \"{repo}\"\nbinary = \"{}\"\n{extra}\n",
                bin.join(format!("havn-channel-{name}")).display()
            )
        };
        std::fs::write(
            &path,
            [
                entry(
                    "test",
                    "amplimit/havn-channel-test",
                    "units = [\"test.service\", \"old.service\"]",
                ),
                entry("gone", "amplimit/gone", ""),
                entry("empty", "amplimit/empty", ""),
                entry("later", "amplimit/later", ""),
                entry(
                    "held",
                    "amplimit/held",
                    "units = [\"image.service\"]\nskip = true",
                ),
            ]
            .concat(),
        )
        .unwrap();
        let units = tempfile::tempdir().unwrap();
        for (unit, binary) in [
            ("odd.service", "/opt/havn-channel-odd"),
            ("jail.service", "/usr/local/bin/havn-channel-test"),
            ("image.service", "/usr/local/bin/havn-channel-held"),
            ("disk.service", "/usr/local/bin/havn-channel-disk"),
        ] {
            std::fs::write(units.path().join(unit), format!("ExecStart={binary}\n")).unwrap();
        }
        let fake = Fake {
            execs: vec![
                ("odd.service", "odd.service", "/opt/havn-channel-odd"),
                (
                    "jail.service",
                    "jail.service",
                    "/usr/local/bin/havn-channel-test",
                ),
                (
                    "image.service",
                    "image.service",
                    "/usr/local/bin/havn-channel-held",
                ),
                (
                    "disk.service",
                    "disk.service",
                    "/usr/local/bin/havn-channel-disk",
                ),
                ("old.service", "old.service", ""),
            ],
            roots: vec![
                ("jail.service", "RootDirectory=/srv/jail"),
                ("image.service", "RootImage=/srv/held.raw"),
                ("disk.service", "RootImage=/srv/disk.raw"),
                ("old.service", "RootDirectory=/srv/old"),
            ],
            apart: vec!["old.service"],
            ..Fake::new(&adapter.binary)
        };
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let nothing = Path::new("/nonexistent");

        let gathered = gather(&path, &[units.path()], nothing, &fake, false).unwrap();
        let rows = update_components(
            &http, &github, gathered, "0.2.12", true, false, &fake, QUICK,
        )
        .await;
        let got: Vec<(&str, &str, &str, &str, bool)> = rows
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.from.as_str(),
                    r.to.as_str(),
                    r.result.as_str(),
                    r.failed,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("test", "0.2.9", "v0.2.11", "updated", false),
                (
                    "test",
                    "-",
                    "-",
                    "not managed: old.service runs another binary under RootDirectory=/srv/old, \
                     though /etc/havn/components.toml lists it under test; list it in \
                     /etc/havn/components.toml on its own, with its binary as this machine has \
                     it, to update it",
                    false
                ),
                (
                    "gone",
                    "0.2.9",
                    "-",
                    "failed: repository amplimit/gone not found",
                    true
                ),
                ("empty", "0.2.9", "-", "skipped: it has no release", false),
                (
                    "later",
                    "0.2.9",
                    "v0.3.0",
                    "skipped: v0.3.0 needs Havn 0.3.0 or later",
                    false
                ),
                (
                    "held",
                    "-",
                    "-",
                    "skipped: skip = true in /etc/havn/components.toml",
                    false
                ),
                (
                    "havn-channel-odd",
                    "-",
                    "-",
                    "not managed: /opt/havn-channel-odd is not in /usr/local/bin; list it in \
                     /etc/havn/components.toml to update it",
                    false
                ),
                (
                    "havn-channel-disk",
                    "-",
                    "-",
                    "not managed: disk.service runs it under RootImage=/srv/disk.raw; havn \
                     update cannot change a binary inside an image: list it in \
                     /etc/havn/components.toml with skip = true to leave it alone",
                    false
                ),
                (
                    "havn-channel-test",
                    "-",
                    "-",
                    "not managed: jail.service runs it under RootDirectory=/srv/jail; list it \
                     in /etc/havn/components.toml, with its binary as this machine has it, to \
                     update it",
                    false
                ),
            ]
        );
        assert_eq!(
            *fake.restarted.borrow(),
            vec!["test.service"],
            "not the unit of its own root"
        );
        let kept = Components::read(&path).unwrap();
        let versions: Vec<Option<&str>> =
            kept.adapters.iter().map(|a| a.version.as_deref()).collect();
        assert_eq!(
            versions,
            vec![
                Some("0.2.11"),
                Some("0.2.9"),
                Some("0.2.9"),
                Some("0.2.9"),
                None
            ]
        );
        assert_eq!(kept.adapters.len(), 5, "the unmanaged one is not added");

        // Checked again: the recorded versions, and nothing written.
        let before = std::fs::read_to_string(&path).unwrap();
        let gathered = gather(&path, &[units.path()], nothing, &fake, true).unwrap();
        let rows = update_components(
            &http, &github, gathered, "0.2.12", false, true, &fake, QUICK,
        )
        .await;
        assert_eq!(rows[0].result, "current");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // A list that cannot be read stops an update, not a check.
        let unreadable = dir.path().join("etc");
        assert!(gather(&unreadable, &[], nothing, &fake, false).is_err());
        let gathered = gather(&unreadable, &[], nothing, &fake, true).unwrap();
        let rows = update_components(
            &http, &github, gathered, "0.2.12", false, true, &fake, QUICK,
        )
        .await;
        assert!(rows[0].result.starts_with("not read: reading"), "{rows:?}");
    }
}
