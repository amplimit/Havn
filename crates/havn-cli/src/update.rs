//! `havn update` (issue #236): install a release of Havn over this one.
//!
//! It downloads the release's archive for this machine and its checksum,
//! puts each binary in place of the one beside this `havn` (keeping the old
//! one as `<name>.old`), restarts the gateway (through systemd when the
//! `havn` unit runs, else as `havn gateway restart` does) and waits for it
//! to answer as the new release. A gateway that does not is given back its
//! old binaries and restarted again.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use havn_core::release;
use sha2::{Digest as _, Sha256};

use crate::paths;

/// The binaries a release brings, all beside one another.
pub const BINARIES: [&str; 4] = ["havn", "havn-gateway", "havn-runtime", "havn-init"];

/// How long the restarted gateway has to answer as the new release.
const START_WITHIN: Duration = Duration::from_secs(90);

/// Bytes of an archive the command takes.
const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("havn-cli")
        .timeout(Duration::from_secs(300))
        .build()
        .context("building the HTTP client")
}

/// The latest release's tag.
async fn latest(http: &reqwest::Client) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = http
        .get(release::LATEST_RELEASE_API)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the latest release")?
        .error_for_status()
        .context("asking GitHub for the latest release")?
        .json()
        .await
        .context("reading the latest release")?;
    Ok(release.tag_name)
}

async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?
        .bytes()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        bail!("{url} is larger than an archive of Havn can be");
    }
    Ok(bytes.to_vec())
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

/// Unpack the release's binaries from `archive` (a `.tar.gz`) into `to`;
/// an error when one of [`BINARIES`] is missing. Nothing else is taken.
fn unpack(archive: &[u8], to: &Path) -> anyhow::Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        let path = entry.path().context("reading the archive")?.into_owned();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !BINARIES.contains(&name) || !entry.header().entry_type().is_file() {
            continue;
        }
        let mut body = Vec::new();
        entry
            .read_to_end(&mut body)
            .with_context(|| format!("reading {name} from the archive"))?;
        std::fs::write(to.join(name), body).with_context(|| format!("unpacking {name}"))?;
    }
    for name in BINARIES {
        if !to.join(name).is_file() {
            bail!("the archive has no {name}");
        }
    }
    Ok(())
}

/// Put each binary of `from` in place of the one in `dir`, the old one kept
/// as `<name>.old`; all or none.
fn swap(dir: &Path, from: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut done: Vec<&str> = Vec::new();
    let result = (|| -> anyhow::Result<()> {
        for name in BINARIES {
            let fresh = dir.join(format!(".{name}.new"));
            std::fs::copy(from.join(name), &fresh)
                .with_context(|| format!("copying {name} into {}", dir.display()))?;
            std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755))?;
            let current = dir.join(name);
            if current.exists() {
                std::fs::rename(&current, dir.join(format!("{name}.old")))
                    .with_context(|| format!("keeping the old {name}"))?;
            }
            done.push(name);
            std::fs::rename(&fresh, &current)
                .with_context(|| format!("putting {name} in place"))?;
        }
        Ok(())
    })();
    if result.is_err() {
        for name in done {
            let old = dir.join(format!("{name}.old"));
            if old.exists() {
                let _ = std::fs::rename(&old, dir.join(name));
            }
        }
        for name in BINARIES {
            let _ = std::fs::remove_file(dir.join(format!(".{name}.new")));
        }
    }
    result
}

/// Put back the binaries [`swap`] kept as `<name>.old` in `dir`.
fn restore(dir: &Path) -> anyhow::Result<()> {
    for name in BINARIES {
        let old = dir.join(format!("{name}.old"));
        if old.exists() {
            std::fs::rename(&old, dir.join(name))
                .with_context(|| format!("putting the old {name} back"))?;
        }
    }
    Ok(())
}

/// How the gateway runs, so it is restarted the same way.
enum Running {
    Systemd,
    Detached,
    Not,
}

fn running() -> Running {
    let systemd = std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "havn"])
        .status()
        .is_ok_and(|s| s.success());
    if systemd {
        return Running::Systemd;
    }
    let cfg = paths::load_config().unwrap_or_default();
    match paths::read_pid(&paths::pid_file(&cfg)) {
        Ok(Some(pid)) if paths::pid_alive(pid) => Running::Detached,
        _ => Running::Not,
    }
}

fn restart(how: &Running) -> anyhow::Result<()> {
    match how {
        Running::Systemd => {
            // A start that failed (the release before) can leave the unit
            // failed, or past systemd's start limit: cleared first.
            let _ = std::process::Command::new("systemctl")
                .args(["reset-failed", "havn"])
                .status();
            let status = std::process::Command::new("systemctl")
                .args(["restart", "havn"])
                .status()
                .context("running systemctl restart havn")?;
            if !status.success() {
                bail!("systemctl restart havn failed");
            }
            Ok(())
        }
        Running::Detached => crate::gateway::restart(true),
        Running::Not => Ok(()),
    }
}

/// Whether the gateway answers as release `tag` within [`START_WITHIN`].
async fn answers_as(http: &reqwest::Client, tag: &str) -> bool {
    let cfg = paths::load_config().unwrap_or_default();
    let url = format!("http://{}/auth/info", paths::listen_addr(&cfg));
    let wanted = release::parse_version(tag);
    let started = Instant::now();
    while started.elapsed() < START_WITHIN {
        let answered = http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()
            .and_then(|r| r.error_for_status().ok());
        if let Some(response) = answered
            && let Ok(info) = response.json::<serde_json::Value>().await
            && info
                .get("version")
                .and_then(serde_json::Value::as_str)
                .and_then(release::parse_version)
                == wanted
        {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

/// `havn update`: install release `version` (the latest when `None`), or
/// with `check` only say whether a newer one is out. With `backup`, the
/// installation is backed up first: the new gateway migrates its
/// databases, which the old binaries put back may not open.
pub async fn run(version: Option<String>, check: bool, backup: bool) -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let http = client()?;
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
    if check || !asked {
        if !release::newer(&tag, current) {
            println!("Havn {current} is the latest release.");
            return Ok(());
        }
        if check {
            println!("{tag} is out (this is {current}): `havn update` installs it.");
            return Ok(());
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
    let probe = dir.join(".havn-update-probe");
    std::fs::write(&probe, b"").with_context(|| {
        format!(
            "cannot write to {}: run it as root (sudo havn update)",
            dir.display()
        )
    })?;
    let _ = std::fs::remove_file(&probe);

    if backup {
        let home = std::env::var_os("HOME").map_or_else(|| dir.clone(), PathBuf::from);
        let output = home.join(format!(
            "havn-backup-before-{tag}-{}.tar.gz",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        ));
        let install = crate::backup::Install::from_config()?;
        crate::backup::backup(&install, &output)
            .await
            .context("backing up first (--no-backup skips it)")?;
        println!("Backed up to {}.", output.display());
    }
    println!("Downloading Havn {tag} for {}…", std::env::consts::ARCH);
    let url = format!("{}/{tag}/{archive_name}", release::RELEASE_DOWNLOADS);
    let archive = fetch(&http, &url).await?;
    let sums = fetch(&http, &format!("{url}.sha256")).await?;
    verify(&archive, &String::from_utf8_lossy(&sums))?;
    let stage = std::env::temp_dir().join(format!("havn-update-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&stage).context("making a directory to unpack into")?;
    let unpacked = unpack(&archive, &stage);
    let swapped = unpacked.and_then(|()| swap(&dir, &stage));
    let _ = std::fs::remove_dir_all(&stage);
    swapped?;
    println!(
        "Installed {tag} in {} (the previous binaries are kept as *.old).",
        dir.display()
    );

    let how = running();
    if matches!(how, Running::Not) {
        println!("The gateway is not running: start it to use {tag}.");
        return Ok(());
    }
    println!("Restarting the gateway…");
    let restarted = restart(&how);
    if restarted.is_ok() && answers_as(&http, &tag).await {
        println!("The gateway runs {tag}. Agents start on it as they are restarted.");
        println!("Channel adapters (Telegram, Lark) are updated on their own.");
        return Ok(());
    }
    eprintln!(
        "The gateway did not come back as {tag}; putting the previous binaries back (if it \
         migrated its databases first, restore the backup with `havn restore`)."
    );
    restore(&dir)?;
    if let Err(e) = restart(&how) {
        return Err(e.context(format!(
            "{tag} did not start, and the previous release, put back, did not start either: see \
             the gateway's log"
        )));
    }
    if !answers_as(&http, current).await {
        bail!(
            "{tag} did not start; the previous binaries are back but the gateway does not answer \
             as {current}: see the gateway's log, and restore the backup with `havn restore` if \
             its databases were migrated"
        );
    }
    match restarted {
        Err(e) => Err(e.context(format!(
            "{tag} was not started; the previous release is back"
        ))),
        Ok(()) => {
            bail!("{tag} did not start; the previous release is back (see the gateway's log)")
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn archive(names: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for name in names {
                let body = format!("#!/bin/sh\necho {name}\n");
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, format!("dist/{name}"), body.as_bytes())
                    .unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        out
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
    fn the_binaries_are_unpacked_and_nothing_else() {
        let to = tempfile::tempdir().unwrap();
        let mut names = BINARIES.to_vec();
        names.push("LICENSE");
        unpack(&archive(&names), to.path()).unwrap();
        for name in BINARIES {
            assert!(to.path().join(name).is_file(), "{name}");
        }
        assert!(!to.path().join("LICENSE").exists());
        let short = tempfile::tempdir().unwrap();
        assert!(
            unpack(&archive(&BINARIES[..3]), short.path()).is_err(),
            "a missing binary"
        );
    }

    #[test]
    fn binaries_are_swapped_with_the_old_kept_and_put_back() {
        let dir = tempfile::tempdir().unwrap();
        let from = tempfile::tempdir().unwrap();
        for name in BINARIES {
            std::fs::write(dir.path().join(name), format!("old {name}")).unwrap();
            std::fs::write(from.path().join(name), format!("new {name}")).unwrap();
        }
        swap(dir.path(), from.path()).unwrap();
        for name in BINARIES {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("new {name}")
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join(format!("{name}.old"))).unwrap(),
                format!("old {name}")
            );
        }
        restore(dir.path()).unwrap();
        for name in BINARIES {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("old {name}")
            );
        }

        // One missing from what is installed: none is changed.
        std::fs::remove_file(from.path().join("havn-init")).unwrap();
        assert!(swap(dir.path(), from.path()).is_err());
        for name in BINARIES {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("old {name}")
            );
            assert!(!dir.path().join(format!(".{name}.new")).exists());
        }
    }
}
