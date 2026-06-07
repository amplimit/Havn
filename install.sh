#!/usr/bin/env bash
# havn — single-server installer (spec §12.1).
#
# Targets Ubuntu 24.04 LTS (also tested on Debian 12). Idempotent: safe to
# re-run after an existing install.
#
# Two install paths:
#   • prebuilt  — download a static binary from GitHub Releases (no toolchain,
#                 no compile). The default for `curl … | bash`.
#   • source    — clone + `cargo build`. The default when run from inside a
#                 checked-out havn repo (so local dev / CI build the tree under
#                 test), or when prebuilt is unavailable for the platform.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/amplimit/havn/main/install.sh | bash   # prebuilt fast path
#   ./install.sh                                        # in a clone → builds source
#   HAVN_VERSION=v0.1.0 ./install.sh                    # pin a specific release
#   HAVN_FROM_SOURCE=1  ./install.sh                    # force a source build
#   HAVN_PREBUILT=1     ./install.sh                    # force prebuilt even in a repo
#   HAVN_PROFILE=debug  ./install.sh                    # faster source build (CI smoke)
#   HAVN_SKIP_RUSTUP=1  ./install.sh                    # CI runner already has Rust
#   HAVN_SKIP_USER=1    ./install.sh                    # no system-user creation
#   HAVN_SKIP_SETUP=1   ./install.sh                    # no `havn setup` (CI)
#
# Exit codes: 0 ok; non-zero on any step failure (no partial state assumed).

set -euo pipefail

readonly PROFILE="${HAVN_PROFILE:-release}"
readonly INSTALL_PREFIX="${HAVN_PREFIX:-/usr/local/bin}"
readonly REPO_URL="${HAVN_REPO_URL:-https://github.com/amplimit/havn}"
readonly BINARIES="havn havn-gateway havn-runtime havn-init"

log() { printf '\033[1;36m▶\033[0m %s\n' "$*" >&2; }
ok()  { printf '\033[1;32m✓\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# Resolve `sudo` — empty string when running as root or when sudo is absent
# and we already have permission (the per-call check below catches misuse).
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    else
        die "must run as root or have sudo installed"
    fi
fi

# 1. OS sanity check.
[ "$(uname -s)" = "Linux" ] || die "havn only runs on Linux (spec §1.4)"

# ── Path selection ───────────────────────────────────────────────────
# In a checked-out repo we build that source by default — local dev and CI
# (scripts/smoke.sh) expect to validate the tree under test, not a release.
in_repo=false
if [ -f Cargo.toml ] && \
   grep -q 'name = "havn-gateway"' crates/havn-gateway/Cargo.toml 2>/dev/null; then
    in_repo=true
fi

use_source=false
if [ "${HAVN_FROM_SOURCE:-0}" = "1" ]; then
    use_source=true
elif $in_repo && [ "${HAVN_PREBUILT:-0}" != "1" ]; then
    use_source=true
fi

# Map `uname -m` → the Rust target triple we publish prebuilt artifacts for.
prebuilt_triple() {
    case "$(uname -m)" in
        x86_64|amd64)   echo "x86_64-unknown-linux-musl" ;;
        aarch64|arm64)  echo "aarch64-unknown-linux-musl" ;;
        *)              return 1 ;;
    esac
}

# Download + verify + extract the prebuilt tarball into $1 (a staging dir).
# Returns non-zero (without dying) so the caller can fall back to source.
try_prebuilt() {
    local stage="$1" triple asset base url
    triple="$(prebuilt_triple)" || { warn "no prebuilt for $(uname -m); building from source"; return 1; }

    # Prebuilt needs only curl + tar + sha256sum. Pull curl if a fresh box
    # lacks it (cheap — not the build-essential toolchain).
    if ! command -v curl >/dev/null 2>&1; then
        if command -v apt-get >/dev/null 2>&1; then
            $SUDO apt-get update -qq
            $SUDO apt-get install -y --no-install-recommends curl ca-certificates
        else
            warn "curl not found and apt unavailable; building from source"; return 1
        fi
    fi

    asset="havn-${triple}.tar.gz"
    base="${REPO_URL%/}/releases"
    if [ -n "${HAVN_VERSION:-}" ]; then
        url="${base}/download/${HAVN_VERSION}/${asset}"
    else
        url="${base}/latest/download/${asset}"
    fi

    log "downloading prebuilt: ${url}"
    if ! curl -fSL --proto '=https' --tlsv1.2 -o "${stage}/${asset}" "${url}"; then
        warn "no release asset at ${url}; building from source"; return 1
    fi
    if ! curl -fSL --proto '=https' --tlsv1.2 -o "${stage}/${asset}.sha256" "${url}.sha256"; then
        warn "checksum file missing for ${asset}; building from source"; return 1
    fi

    log "verifying checksum"
    ( cd "${stage}" && sha256sum -c "${asset}.sha256" ) >/dev/null 2>&1 \
        || die "checksum mismatch for ${asset} — refusing to install a corrupt/tampered binary"

    tar -xzf "${stage}/${asset}" -C "${stage}"
    for bin in $BINARIES; do
        [ -x "${stage}/${bin}" ] || die "prebuilt tarball missing ${bin}"
    done
    ok "fetched prebuilt binaries (${triple})"
    return 0
}

# ── Acquire binaries ─────────────────────────────────────────────────
STAGE="$(mktemp -d -t havn-bin.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT
SRC_DIR=""   # set on the source path so we know where artifacts landed

if ! $use_source; then
    if try_prebuilt "$STAGE"; then
        ARTIFACT_DIR="$STAGE"
    else
        use_source=true
    fi
fi

if $use_source; then
    # 2. apt prerequisites. Skip cleanly if apt is missing (e.g. Alpine / Arch).
    if command -v apt-get >/dev/null 2>&1; then
        log "installing build prerequisites via apt"
        $SUDO apt-get update -qq
        $SUDO apt-get install -y --no-install-recommends \
            build-essential pkg-config curl ca-certificates git
    else
        log "apt-get not found; assuming build-essential / pkg-config / git are already installed"
    fi

    # 3. rustup (skipped on CI runners that already have a toolchain).
    if [ "${HAVN_SKIP_RUSTUP:-0}" != "1" ] && ! command -v cargo >/dev/null 2>&1; then
        log "installing rustup (stable toolchain)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --default-toolchain stable --profile minimal
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
    command -v cargo >/dev/null 2>&1 || die "cargo not on PATH after rustup install"

    # 4. Resolve source tree. Use the current dir if it looks like the
    # workspace, else clone shallowly into a tempdir.
    if $in_repo; then
        SRC_DIR="$PWD"
        log "using checked-out repo at $SRC_DIR"
    else
        SRC_DIR="$(mktemp -d -t havn-src.XXXXXX)"
        log "cloning $REPO_URL → $SRC_DIR"
        git clone --depth 1 "$REPO_URL" "$SRC_DIR"
    fi

    # 5. Build the binaries. Profile defaults to release; HAVN_PROFILE=debug
    # is the fast path for CI smoke.
    cd "$SRC_DIR"
    log "cargo build --profile=$PROFILE --bins (this is the long step)"
    case "$PROFILE" in
        release) cargo build --release --bins ;;
        debug)   cargo build --bins ;;
        *)       die "HAVN_PROFILE must be 'release' or 'debug', got '$PROFILE'" ;;
    esac
    ARTIFACT_DIR="target/$([ "$PROFILE" = release ] && echo release || echo debug)"
fi

# 6. Install binaries.
log "installing binaries → $INSTALL_PREFIX"
$SUDO install -d -m 755 "$INSTALL_PREFIX"
# `havn-init` is the PID-1 shim (spec §4.1) — must live next to
# `havn-gateway` so the gateway's sibling-binary resolver finds it.
for bin in $BINARIES; do
    [ -x "$ARTIFACT_DIR/$bin" ] || die "missing build artifact: $ARTIFACT_DIR/$bin"
    $SUDO install -m 755 "$ARTIFACT_DIR/$bin" "$INSTALL_PREFIX/$bin"
done

# 7. Create the dedicated `havn-wakecheck` system user (spec §8.5 / §11).
# Wake-check subprocesses setuid to this UID so a malicious cron config can't
# touch the gateway's data directory or hold its credentials.
if [ "${HAVN_SKIP_USER:-0}" != "1" ]; then
    if id -u havn-wakecheck >/dev/null 2>&1; then
        ok "user havn-wakecheck already exists"
    elif command -v useradd >/dev/null 2>&1; then
        log "creating system user havn-wakecheck"
        $SUDO useradd --system --no-create-home \
            --shell /usr/sbin/nologin \
            --comment "havn cron wake-check sandbox UID (spec §8.5)" \
            havn-wakecheck
        ok "user havn-wakecheck created"
    else
        log "useradd not found; skipping wake-check user creation"
    fi
fi

# 8. Run `havn setup --non-interactive` so the data dir + config exist on first run.
if [ "${HAVN_SKIP_SETUP:-0}" != "1" ]; then
    log "running havn setup --non-interactive"
    "$INSTALL_PREFIX/havn" setup --non-interactive
fi

ok "install complete"
cat <<EOF >&2

next:
  havn start                                    # launch the gateway
  havn credential add anthropic <YOUR_API_KEY>  # in another shell, after gateway is up
  havn agent create my-first-agent
  open http://127.0.0.1:8080/healthz            # confirm the gateway responds
EOF
