#!/usr/bin/env bash
# scripts/readme-deploy.sh — deploy havn the way README.md's Quick Start
# says, from this checkout, then chat with an agent through the
# dashboard's own server in a headless browser.
#
# Steps, as in the README: install.sh (which builds this checkout instead
# of downloading a release), `havn setup`, `havn start` as root,
# `havn credential add anthropic ...`, and the dashboard's
# `npm ci && npm run build && npm start`. CI has no LLM key, so the
# credential is a dummy and `[llm.base_url]` points anthropic calls at a
# mock provider that scripts/dashboard-chat.mjs runs for the chat.
#
# Usage:
#   PLAYWRIGHT_DIR=<dir with node_modules/playwright> ./scripts/readme-deploy.sh
#
# Settings:
#   HAVN_PROFILE        install.sh build profile (default debug)
#   HAVN_SKIP_INSTALL=1 binaries are already on PATH; skip install.sh
#   HAVN_DEPLOY_SUDO    how to become root for the gateway (default
#                       "sudo -H"; empty runs it as the current user, which
#                       needs HAVN_SPAWNER=subprocess)
#   HAVN_MOCK_LLM_PORT  mock provider port (default 18090)
#
# Exit codes: 0 ok; 2 step failure.

set -euo pipefail

readonly GATEWAY_URL="http://127.0.0.1:8080"
readonly DASHBOARD_URL="http://127.0.0.1:3000"
readonly MOCK_PORT="${HAVN_MOCK_LLM_PORT:-18090}"
readonly MOCK_KEY="sk-ant-readme-deploy-mock"
readonly AGE_KEY="readme-deploy-passphrase"
read -r -a AS_ROOT <<<"${HAVN_DEPLOY_SUDO-sudo -H}"

log() { printf '\033[1;36m▶\033[0m %s\n' "$*" >&2; }
ok()  { printf '\033[1;32m✓\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 2; }

[ -n "${PLAYWRIGHT_DIR:-}" ] || die "PLAYWRIGHT_DIR must name a directory with node_modules/playwright"
command -v node >/dev/null 2>&1 || die "node is required"
command -v curl >/dev/null 2>&1 || die "curl is required"
command -v jq >/dev/null 2>&1 || die "jq is required"

LOG_DIR="${RUNNER_TEMP:-/tmp}"
GATEWAY_LOG="$LOG_DIR/readme-gateway.log"
DASHBOARD_LOG="$LOG_DIR/readme-dashboard.log"
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

wait_for() {
    local what=$1 url=$2 tries=$3
    for _ in $(seq 1 "$tries"); do
        curl -fs -o /dev/null "$url" && { ok "$what is up"; return 0; }
        sleep 1
    done
    return 1
}

# Install: `curl ... | bash` runs install.sh, which in a checkout builds it.
if [ "${HAVN_SKIP_INSTALL:-0}" != "1" ]; then
    log "install.sh (HAVN_PROFILE=${HAVN_PROFILE:-debug})"
    HAVN_PROFILE="${HAVN_PROFILE:-debug}" bash ./install.sh
fi

# README: `sudo -i`, then `havn setup --non-interactive`.
log "havn setup --non-interactive"
"${AS_ROOT[@]}" havn setup --non-interactive >/dev/null
CONFIG="$("${AS_ROOT[@]}" sh -c 'printf %s "$HOME/.config/havn/config.toml"')"
# The one deviation from the README: send anthropic calls to the mock.
if ! "${AS_ROOT[@]}" grep -q '^\[llm\.base_url\]' "$CONFIG"; then
    printf '\n[llm.base_url]\nanthropic = "http://127.0.0.1:%s"\n' "$MOCK_PORT" \
        | "${AS_ROOT[@]}" tee -a "$CONFIG" >/dev/null
fi

# README: `export HAVN_AGE_KEY=...`, `havn start`.
log "havn start"
"${AS_ROOT[@]}" env HAVN_AGE_KEY="$AGE_KEY" HAVN_SPAWNER="${HAVN_SPAWNER:-}" \
    havn start >"$GATEWAY_LOG" 2>&1 &
GW_PID=$!
DASH_PID=""
cleanup() {
    if [ -n "$DASH_PID" ]; then
        kill -- "-$DASH_PID" 2>/dev/null || true
    fi
    for id in $(curl -fs --max-time 5 "$GATEWAY_URL/agents" 2>/dev/null | jq -r '.[].id' || true); do
        curl -fs --max-time 15 -X POST "$GATEWAY_URL/agents/$id/stop" -o /dev/null || true
    done
    "${AS_ROOT[@]}" kill "$GW_PID" 2>/dev/null || true
}
trap cleanup EXIT

wait_for gateway "$GATEWAY_URL/healthz" 60 \
    || { tail -50 "$GATEWAY_LOG" >&2; die "gateway never became healthy"; }

# README: `havn credential add anthropic sk-ant-...`.
log "havn credential add anthropic"
"${AS_ROOT[@]}" havn credential add anthropic "$MOCK_KEY" >/dev/null \
    || { tail -50 "$GATEWAY_LOG" >&2; die "credential add failed"; }

# README: `cd havn/apps/dashboard && npm install && npm run build && npm start`;
# `npm ci` installs the same lockfile without rewriting it.
log "dashboard: npm ci && npm run build && npm start"
(
    cd apps/dashboard
    npm ci --no-audit --no-fund
    npm run build
) >"$DASHBOARD_LOG" 2>&1 || { tail -50 "$DASHBOARD_LOG" >&2; die "dashboard build failed"; }
# Its own process group, so cleanup also stops the node process npm starts.
setsid bash -c 'cd apps/dashboard && exec npm start' >>"$DASHBOARD_LOG" 2>&1 &
DASH_PID=$!
wait_for dashboard "$DASHBOARD_URL/" 60 \
    || { tail -50 "$DASHBOARD_LOG" >&2; die "dashboard never came up"; }

log "chatting through the dashboard in a headless browser"
HAVN_DASHBOARD_URL="$DASHBOARD_URL" HAVN_MOCK_LLM_PORT="$MOCK_PORT" HAVN_MOCK_LLM_KEY="$MOCK_KEY" \
    node scripts/dashboard-chat.mjs \
    || { tail -80 "$GATEWAY_LOG" >&2; tail -30 "$DASHBOARD_LOG" >&2; die "dashboard chat failed"; }
ok "README deployment answers a chat through the dashboard"
