#!/bin/bash
# Start AntiRustPaste with throwaway history, preferences and login-item paths.
#
# This script never touches the operator's clipboard, history, preferences,
# LaunchAgents or running processes. It stops only the process it started.
#
# Optional:
#   ANTIRUSTPASTE_SMOKE_PREFS=1  open Preferences at launch
#   ANTIRUSTPASTE_SMOKE_PANEL=1  open the history popup at launch
#   ANTIRUSTPASTE_SMOKE_IMAGES=1 open the popup on the Images tab
#   ANTIRUSTPASTE_SMOKE_COMMANDS=1 open the popup on the Commands tab with synthetic history
#   ANTIRUSTPASTE_SMOKE_SECONDS  seconds to keep the app running (default 3)
#   ANTIRUSTPASTE_BIN            binary to test (default target/debug/antirustpaste)
set -euo pipefail

cd "$(dirname "$0")/.."

BIN="${ANTIRUSTPASTE_BIN:-target/debug/antirustpaste}"
if [[ ! -x "$BIN" ]]; then
  cargo build
fi

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/antirustpaste-smoke.XXXXXX")"
export ANTIRUSTPASTE_DATA_DIR="$SCRATCH/data"
export ANTIRUSTPASTE_DEFAULTS_SUITE="com.antirustpaste.smoke.$$"
export ANTIRUSTPASTE_LAUNCH_AGENT_DIR="$SCRATCH/LaunchAgents"
export ANTIRUSTPASTE_THUMB_CACHE_DIR="$SCRATCH/thumbnails"
export ANTIRUSTPASTE_SHELL_HISTORY_DIR="$SCRATCH/shell"
mkdir -p "$ANTIRUSTPASTE_SHELL_HISTORY_DIR"
[[ "${ANTIRUSTPASTE_SMOKE_PREFS:-0}" == "1" ]] && export ANTIRUSTPASTE_OPEN_PREFS=1
[[ "${ANTIRUSTPASTE_SMOKE_PANEL:-0}" == "1" ]] && export ANTIRUSTPASTE_OPEN_PANEL=1
[[ "${ANTIRUSTPASTE_SMOKE_IMAGES:-0}" == "1" ]] && export ANTIRUSTPASTE_OPEN_IMAGES=1
if [[ "${ANTIRUSTPASTE_SMOKE_COMMANDS:-0}" == "1" ]]; then
  printf ': 1700000000:0;echo antirustpaste-smoke-1\n: 1700000001:0;echo antirustpaste-smoke-2\n' \
    >"$ANTIRUSTPASTE_SHELL_HISTORY_DIR/.zsh_history"
  printf 'echo antirustpaste-smoke-3\n' >"$ANTIRUSTPASTE_SHELL_HISTORY_DIR/.bash_history"
  export ANTIRUSTPASTE_OPEN_COMMANDS=1
fi

"$BIN" >"$SCRATCH/app.log" 2>&1 &
PID=$!

SMOKE_OK=0
cleanup() {
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  defaults delete "$ANTIRUSTPASTE_DEFAULTS_SUITE" >/dev/null 2>&1 || true
  if [[ "$SMOKE_OK" == "1" ]]; then
    rm -rf "$SCRATCH"
  fi
}
trap cleanup EXIT

sleep "${ANTIRUSTPASTE_SMOKE_SECONDS:-3}"

if ! kill -0 "$PID" 2>/dev/null; then
  echo "smoke: AntiRustPaste exited unexpectedly" >&2
  cat "$SCRATCH/app.log" >&2
  exit 1
fi

if grep -q "panicked at" "$SCRATCH/app.log"; then
  echo "smoke: AntiRustPaste logged a panic" >&2
  cat "$SCRATCH/app.log" >&2
  exit 1
fi

if [[ ! -d "$ANTIRUSTPASTE_DATA_DIR" ]]; then
  echo "smoke: history directory was not created" >&2
  cat "$SCRATCH/app.log" >&2
  exit 1
fi

SMOKE_OK=1
echo "smoke: ok"
