//! Havn's releases (issue #236): where they are, and which is newer.

/// The GitHub API address of Havn's latest release.
pub const LATEST_RELEASE_API: &str = "https://api.github.com/repos/amplimit/havn/releases/latest";

/// Where a release's files are downloaded from: this, then the tag, then
/// the file's name.
pub const RELEASE_DOWNLOADS: &str = "https://github.com/amplimit/havn/releases/download";

/// `major.minor.patch` of a version or tag (`v0.2.9` or `0.2.9`); `None`
/// for anything else, a pre-release (`0.3.0-rc1`) among them.
#[must_use]
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let mut parts = text.split('.');
    let mut next = || -> Option<u64> {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// Whether release `tag` is newer than version `current`; `false` when
/// either cannot be read.
#[must_use]
pub fn newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(tag), Some(current)) => tag > current,
        _ => false,
    }
}

/// The name of a release's archive for the machine `arch`
/// (`std::env::consts::ARCH`); `None` where there is none.
#[must_use]
pub fn archive_for(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("havn-x86_64-unknown-linux-musl.tar.gz"),
        "aarch64" => Some("havn-aarch64-unknown-linux-musl.tar.gz"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_and_compared_by_their_numbers() {
        assert_eq!(parse_version("v0.2.9"), Some((0, 2, 9)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("v0.3.0-rc1"), None);
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("0.2.9.1"), None);
        assert_eq!(parse_version("latest"), None);
        assert!(newer("v0.2.10", "0.2.9"), "not compared as text");
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(!newer("v0.2.9", "0.2.9"));
        assert!(!newer("v0.2.8", "0.2.9"));
        assert!(!newer("v0.3.0-rc1", "0.2.9"), "no pre-release");
        assert!(!newer("garbage", "0.2.9"));
    }

    #[test]
    fn an_archive_is_named_for_each_machine_built_for() {
        assert_eq!(
            archive_for("x86_64"),
            Some("havn-x86_64-unknown-linux-musl.tar.gz")
        );
        assert_eq!(
            archive_for("aarch64"),
            Some("havn-aarch64-unknown-linux-musl.tar.gz")
        );
        assert_eq!(archive_for("riscv64"), None);
    }
}
