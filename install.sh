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
#   HAVN_SKIP_SYSTEMD=1 ./install.sh                    # no systemd unit install
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

# `command -v`, plus the sbin directories an unprivileged PATH often lacks
# (iptables and ip live there on Debian / Ubuntu).
have_tool() {
    command -v "$1" >/dev/null 2>&1 && return 0
    local dir
    for dir in /usr/local/sbin /usr/sbin /sbin; do
        [ -x "$dir/$1" ] && return 0
    done
    return 1
}

APT_UPDATED=false
apt_install() {
    if ! $APT_UPDATED; then
        $SUDO apt-get update -qq
        APT_UPDATED=true
    fi
    $SUDO apt-get install -y --no-install-recommends "$@"
}

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
            apt_install curl ca-certificates
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
    missing=""
    for tool in cc pkg-config curl git; do
        command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
    done
    # A `cc` without libc headers / startup files (no libc6-dev) passes the
    # check above but fails the bundled C builds (sqlite, aws-lc) late.
    if command -v cc >/dev/null 2>&1; then
        printf '#include <stdio.h>\nint main(void) { return puts("") < 0; }\n' >"$STAGE/probe.c"
        cc "$STAGE/probe.c" -o "$STAGE/probe" >/dev/null 2>&1 || missing="$missing libc-headers"
    fi
    if [ -z "$missing" ]; then
        log "build prerequisites already installed; skipping apt"
    elif command -v apt-get >/dev/null 2>&1; then
        log "installing build prerequisites via apt (missing:$missing)"
        apt_install build-essential pkg-config curl ca-certificates git
    else
        warn "apt-get not found; install these yourself if the build fails:$missing (C compiler, libc headers, pkg-config, curl, git)"
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

# 7. Network tools the gateway runs for agent egress (veth + NAT, spec §4.1).
# Without them the gateway still starts, but every agent has no outbound
# network.
missing_net=""
for tool in iptables ip nsenter; do
    have_tool "$tool" || missing_net="$missing_net $tool"
done
if [ -z "$missing_net" ]; then
    ok "network tools present (iptables, ip, nsenter)"
elif command -v apt-get >/dev/null 2>&1; then
    log "installing network tools via apt (missing:$missing_net)"
    apt_install iptables iproute2 util-linux \
        || warn "apt-get failed; agents will have no outbound network until iptables, iproute2 and util-linux are installed"
else
    warn "missing:$missing_net — agents will have no outbound network; install iptables (nft or legacy backend), iproute2 (ip) and util-linux (nsenter)"
fi

# 8. Create the dedicated `havn-wakecheck` system user (spec §8.5 / §11).
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

# 9. Reference systemd unit, installed but not enabled. The vendor path keeps
# operator changes (drop-ins, or a copy in /etc/systemd/system) intact on
# re-install.
readonly UNIT_DIR="/usr/local/lib/systemd/system"
unit_installed=false
if [ "${HAVN_SKIP_SYSTEMD:-0}" != "1" ] && [ -d /run/systemd/system ]; then
    if $use_source; then
        unit_src="$SRC_DIR/contrib/systemd/havn.service"
    else
        unit_src="$STAGE/havn.service"
    fi
    # The prefix goes verbatim into ExecStart= and the sed replacement.
    case "$INSTALL_PREFIX" in
        *[[:space:]]* | *'&'* | *'|'* | *'\'*) prefix_ok=false ;;
        /*) prefix_ok=true ;;
        *) prefix_ok=false ;;
    esac
    if ! $prefix_ok; then
        warn "HAVN_PREFIX '$INSTALL_PREFIX' must be an absolute path without whitespace, '&', '|' or '\\'; skipping the systemd unit"
    elif [ -f "$unit_src" ]; then
        sed "s|^ExecStart=/usr/local/bin/|ExecStart=${INSTALL_PREFIX%/}/|" "$unit_src" >"$STAGE/unit"
        $SUDO install -d -m 755 "$UNIT_DIR"
        $SUDO install -m 644 "$STAGE/unit" "$UNIT_DIR/havn.service"
        $SUDO systemctl daemon-reload || warn "systemctl daemon-reload failed"
        unit_installed=true
        ok "installed $UNIT_DIR/havn.service (not enabled)"
    else
        warn "this release ships no havn.service; see contrib/systemd/havn.service in the repository"
    fi
fi

# 10. Run `havn setup --non-interactive` so the data dir + config exist on first run.
# The systemd unit runs the gateway with HOME=/root, so when it is installed
# setup also runs for root with that HOME (unless this run already is that);
# otherwise the unit's gateway would start on an empty config and database.
# The invoking user's setup still runs for `havn start` from their shell.
root_setup=false
if $unit_installed && { [ "$(id -u)" -ne 0 ] || [ "${HOME:-}" != /root ] \
        || [ -n "${XDG_CONFIG_HOME:-}" ] || [ -n "${HAVN_CONFIG:-}" ]; }; then
    root_setup=true
fi
if [ "${HAVN_SKIP_SETUP:-0}" != "1" ]; then
    log "running havn setup --non-interactive"
    "$INSTALL_PREFIX/havn" setup --non-interactive
    if $root_setup; then
        log "running havn setup --non-interactive for root (HOME=/root, as the systemd unit uses)"
        $SUDO env -u XDG_CONFIG_HOME -u HAVN_CONFIG HOME=/root \
            "$INSTALL_PREFIX/havn" setup --non-interactive
    fi
fi

ok "install complete"
cat <<EOF >&2

next:
  havn start                                    # launch the gateway (as root)
  havn credential add anthropic <YOUR_API_KEY>  # in another shell, after gateway is up
  havn agent create my-first-agent
  open http://127.0.0.1:8080/healthz            # confirm the gateway responds
  havn doctor                                   # check isolation and network prerequisites
EOF
if $unit_installed; then
    root_setup_hint=""
    if [ "${HAVN_SKIP_SETUP:-0}" = "1" ]; then
        root_setup_hint="  sudo env HOME=/root havn setup --non-interactive   # config + data under /root, as the unit uses
"
    fi
    cat <<EOF >&2

or run the gateway under systemd (it uses root's config under /root):
${root_setup_hint}  sudo install -d -m 700 -o root -g root /etc/havn
  sudo install -m 600 -o root -g root /dev/null /etc/havn/havn.env
  sudoedit /etc/havn/havn.env                        # add: HAVN_AGE_KEY='<passphrase>' (no ' or \\ inside)
  sudo systemctl enable --now havn
EOF
fi
