#!/usr/bin/env bash
# Two-device e2e smoke over the tailnet hub (loopback, no Cloudflare):
# two headless engines, and the hearth-rpc e2e_driver proving the doc-queued
# cross-device command path:
#
#   B queues a Run into the chat doc -> A (host/hub) executes via the mock
#   harness -> transcript + session status sync A -> hub rooms -> B.
#
# Engine A hosts rooms on 127.0.0.1:27655. Engine B dials that hub (its own
# bind of the same port is expected to fail on this one-machine smoke).
#
# Usage: scripts/e2e-smoke.sh
# Env:   HEARTH_E2E_KEEP_LOGS=1 to keep logs.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
HUB_PORT="${HEARTH_TAILNET_PORT:-27655}"
ORG="org1"
A_PORT=27801
B_PORT=27802
A_DIR=/tmp/e2e-a
B_DIR=/tmp/e2e-b
LOG_DIR="$(mktemp -d /tmp/hearth-e2e-logs.XXXXXX)"

A_PID=""
B_PID=""
STATUS=1

cleanup() {
  for pid in "$A_PID" "$B_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  sleep 1
  for pid in "$A_PID" "$B_PID"; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done
  rm -rf "$A_DIR" "$B_DIR"
  if [[ "$STATUS" -ne 0 ]]; then
    echo "--- engine A log (tail) ---"; tail -n 40 "$LOG_DIR/engine-a.log" 2>/dev/null || true
    echo "--- engine B log (tail) ---"; tail -n 40 "$LOG_DIR/engine-b.log" 2>/dev/null || true
  fi
  if [[ "${HEARTH_E2E_KEEP_LOGS:-0}" != "1" ]]; then
    rm -rf "$LOG_DIR"
  else
    echo "logs kept in $LOG_DIR"
  fi
}
trap cleanup EXIT

wait_for() { # wait_for <description> <timeout_s> <command...>
  local what="$1" timeout="$2"; shift 2
  local waited=0
  until "$@" >/dev/null 2>&1; do
    sleep 1
    waited=$((waited + 1))
    if [[ "$waited" -ge "$timeout" ]]; then
      echo "FAIL: timed out waiting for $what" >&2
      exit 1
    fi
  done
}

# ── 1. Build the binaries ────────────────────────────────────────────────────
echo "build: hearth + e2e_driver"
(cd "$ROOT" && cargo build -q -p hearth)
(cd "$ROOT" && cargo build -q -p hearth-rpc --example e2e_driver)
HEARTH="$ROOT/target/debug/hearth"
DRIVER="$ROOT/target/debug/examples/e2e_driver"

# ── 2. Two headless engines: A is the hub, B is a spoke ──────────────────────
rm -rf "$A_DIR" "$B_DIR"
mkdir -p "$A_DIR" "$B_DIR"

start_engine() { # start_engine <data_dir> <ipc_port> <name> <log> <hub?>
  local hub_flag="${5:-}"
  HEARTH_DATA_DIR="$1" HEARTH_IPC_PORT="$2" HEARTH_DEVICE_NAME="$3" \
    HEARTH_TAILNET_HOST=127.0.0.1 HEARTH_TAILNET_PORT="$HUB_PORT" \
    HEARTH_TAILNET_HUB="$hub_flag" HEARTH_ORG_ID="$ORG" \
    HEARTH_HARNESS=mock RUST_LOG=info \
    "$HEARTH" headless >"$4" 2>&1 &
}

start_engine "$A_DIR" "$A_PORT" "e2e-device-a" "$LOG_DIR/engine-a.log" "1"; A_PID=$!
wait_for "hub /health" 30 curl -sf -m 3 "http://127.0.0.1:${HUB_PORT}/health"
start_engine "$B_DIR" "$B_PORT" "e2e-device-b" "$LOG_DIR/engine-b.log" ""; B_PID=$!

wait_for "engine A ipc :$A_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$A_PORT"
wait_for "engine B ipc :$B_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$B_PORT"
echo "engines: A pid=$A_PID ipc=:$A_PORT hub=:$HUB_PORT  B pid=$B_PID ipc=:$B_PORT"

# ── 3. Drive the cross-device flow through both IPCs ─────────────────────────
"$DRIVER" "$A_PORT" "$B_PORT"
STATUS=$?
exit "$STATUS"
