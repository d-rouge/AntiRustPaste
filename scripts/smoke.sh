#!/bin/bash
# Start RustClip with throwaway history, preferences and login-item paths.
#
# This script never touches the operator's clipboard, history, preferences,
# LaunchAgents or running processes. It stops only the process it started.
#
# Optional:
#   RUSTCLIP_SMOKE_PREFS=1  open Preferences at launch
#   RUSTCLIP_SMOKE_PANEL=1  open the history popup at launch
#   RUSTCLIP_SMOKE_IMAGES=1 open the popup on the Images tab
#   RUSTCLIP_SMOKE_SECONDS  seconds to keep the app running (default 3)
#   RUSTCLIP_BIN            binary to test (default target/debug/rustclip)
set -euo pipefail

cd "$(dirname "$0")/.."

BIN="${RUSTCLIP_BIN:-target/debug/rustclip}"
if [[ ! -x "$BIN" ]]; then
  cargo build
fi

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rustclip-smoke.XXXXXX")"
export RUSTCLIP_DATA_DIR="$SCRATCH/data"
export RUSTCLIP_DEFAULTS_SUITE="com.rustclip.smoke.$$"
export RUSTCLIP_LAUNCH_AGENT_DIR="$SCRATCH/LaunchAgents"
export RUSTCLIP_THUMB_CACHE_DIR="$SCRATCH/thumbnails"
[[ "${RUSTCLIP_SMOKE_PREFS:-0}" == "1" ]] && export RUSTCLIP_OPEN_PREFS=1
[[ "${RUSTCLIP_SMOKE_PANEL:-0}" == "1" ]] && export RUSTCLIP_OPEN_PANEL=1
[[ "${RUSTCLIP_SMOKE_IMAGES:-0}" == "1" ]] && export RUSTCLIP_OPEN_IMAGES=1

"$BIN" >"$SCRATCH/app.log" 2>&1 &
PID=$!

SMOKE_OK=0
cleanup() {
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  defaults delete "$RUSTCLIP_DEFAULTS_SUITE" >/dev/null 2>&1 || true
  if [[ "$SMOKE_OK" == "1" ]]; then
    rm -rf "$SCRATCH"
  fi
}
trap cleanup EXIT

sleep "${RUSTCLIP_SMOKE_SECONDS:-3}"

if ! kill -0 "$PID" 2>/dev/null; then
  echo "smoke: RustClip exited unexpectedly" >&2
  cat "$SCRATCH/app.log" >&2
  exit 1
fi

if grep -q "panicked at" "$SCRATCH/app.log"; then
  echo "smoke: RustClip logged a panic" >&2
  cat "$SCRATCH/app.log" >&2
  exit 1
fi

if [[ ! -d "$RUSTCLIP_DATA_DIR" ]]; then
  echo "smoke: history directory was not created" >&2
  cat "$SCRATCH/app.log" >&2
  exit 1
fi

SMOKE_OK=1
echo "smoke: ok"
