#!/usr/bin/env bash
# scripts/smoke.sh — install → first-chat-ready end-to-end smoke test
# (spec §12.1: ≤ 10 minutes onboarding SLO).
#
# Times the wall-clock from install.sh start to "agent connected & WebChat
# endpoint will accept connections", and asserts it stays under the SLO.
# Skips the LLM round-trip itself: that requires a real provider key, which
# CI does not have. The infrastructure path (install + binaries + gateway +
# spawner + agent socket handshake) is what the SLO actually measures.
#
# Usage:
#   ./scripts/smoke.sh                      # full path: install + run
#   HAVN_SKIP_INSTALL=1 ./scripts/smoke.sh  # skip install (binaries on PATH)
#
# Exit codes: 0 ok; 1 SLO breach; 2 step failure.

set -euo pipefail

readonly SLO_SECONDS="${HAVN_SMOKE_SLO:-600}"
readonly GATEWAY_URL="${HAVN_GATEWAY:-http://127.0.0.1:8080}"

log() { printf '\033[1;36m▶\033[0m %s\n' "$*" >&2; }
ok()  { printf '\033[1;32m✓\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 2; }

command -v jq    >/dev/null 2>&1 || die "jq is required"
command -v curl  >/dev/null 2>&1 || die "curl is required"

START=$(date +%s)

# 1. install (uses the debug profile for a fast CI build; production runs
#    with the default release profile for runtime perf).
if [ "${HAVN_SKIP_INSTALL:-0}" != "1" ]; then
    log "running install.sh (HAVN_PROFILE=${HAVN_PROFILE:-debug})"
    HAVN_PROFILE="${HAVN_PROFILE:-debug}" \
    HAVN_SKIP_RUSTUP="${HAVN_SKIP_RUSTUP:-1}" \
        bash ./install.sh
fi

# 2. background-start the gateway.
log "starting gateway"
LOG_DIR="${RUNNER_TEMP:-/tmp}"
GATEWAY_LOG="$LOG_DIR/havn-gateway.log"
nohup havn start >"$GATEWAY_LOG" 2>&1 &
GW_PID=$!
trap 'kill "$GW_PID" 2>/dev/null || true' EXIT

# 3. wait for /healthz.
log "waiting for /healthz"
for _ in $(seq 1 60); do
    if curl -fs "$GATEWAY_URL/healthz" >/dev/null 2>&1; then
        ok "gateway healthy"
        break
    fi
    sleep 1
done
curl -fs "$GATEWAY_URL/healthz" >/dev/null \
    || { tail -50 "$GATEWAY_LOG" >&2; die "gateway never became healthy"; }

# 4. fetch the WS token (= current user id in single-user mode).
ME_JSON=$(curl -fs "$GATEWAY_URL/me")
WS_TOKEN=$(echo "$ME_JSON" | jq -r .ws_token)
[ -n "$WS_TOKEN" ] && [ "$WS_TOKEN" != "null" ] || die "GET /me returned no ws_token"

# 5. create + start an agent.
log "creating agent"
AGENT_JSON=$(curl -fs -X POST "$GATEWAY_URL/agents" \
    -H "Content-Type: application/json" \
    -d '{"name":"smoke-test-agent"}')
AGENT_ID=$(echo "$AGENT_JSON" | jq -r .id)
[ -n "$AGENT_ID" ] && [ "$AGENT_ID" != "null" ] || die "POST /agents returned no id"
ok "agent $AGENT_ID created"

log "starting agent runtime"
curl -fs -X POST "$GATEWAY_URL/agents/$AGENT_ID/start" -o /dev/null \
    || { tail -50 "$GATEWAY_LOG" >&2; die "POST /agents/.../start failed"; }

# 6. wait for the agent to complete its Hello/Welcome handshake. The WS
#    endpoint requires this before it returns 101 Switching Protocols.
log "waiting for agent socket handshake"
CONNECTED=false
for _ in $(seq 1 60); do
    CONNECTED=$(curl -fs "$GATEWAY_URL/agents/$AGENT_ID" | jq -r '.connected // false')
    [ "$CONNECTED" = "true" ] && break
    sleep 1
done
[ "$CONNECTED" = "true" ] \
    || { tail -100 "$GATEWAY_LOG" >&2; die "agent never connected to socket"; }
ok "agent connected (WebChat endpoint will now accept WS upgrades)"

# 7. verify the WebSocket upgrade is accepted. Curl will return non-zero on
#    a non-101 response; we use --max-time to bail out of the long-poll.
log "verifying WebSocket upgrade"
WS_KEY="$(printf 'havn-smoke-test-key-1' | base64)"
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' \
    --max-time 2 \
    -H 'Connection: Upgrade' \
    -H 'Upgrade: websocket' \
    -H 'Sec-WebSocket-Version: 13' \
    -H "Sec-WebSocket-Key: $WS_KEY" \
    -H 'Origin: http://localhost:3000' \
    "$GATEWAY_URL/ws/chat/$AGENT_ID?token=$WS_TOKEN" || true)
# 101 = Switching Protocols. curl --max-time aborts after upgrade so the
# exit code may be non-zero, but the HTTP status line is captured first.
if [ "$HTTP_CODE" != "101" ]; then
    tail -100 "$GATEWAY_LOG" >&2
    die "WebSocket upgrade returned $HTTP_CODE (expected 101)"
fi
ok "WebSocket upgrade accepted"

END=$(date +%s)
ELAPSED=$((END - START))
ok "first-chat-ready in ${ELAPSED}s (SLO: ${SLO_SECONDS}s)"

if [ "$ELAPSED" -gt "$SLO_SECONDS" ]; then
    printf '\033[1;31m✗\033[0m SLO BREACH: %ss > %ss\n' "$ELAPSED" "$SLO_SECONDS" >&2
    exit 1
fi

ok "SLO met"
