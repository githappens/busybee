#!/usr/bin/env bash
# Real Pueue launches through sortie/isolated.sh. Run under the development
# shell, which supplies pueued and pueue; a missing binary fails the check
# rather than skipping it.
#
# A sentinel pueued with its own private configuration stands in for a
# developer's daemon: it must keep its pid and its task list throughout, and
# the launcher's daemon must never see it.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd -P)
self="$here/$(basename "$0")"
launcher="$here/isolated.sh"
deadline=10
failures=0

ok() { printf 'ok - %s\n' "$1"; }
not_ok() {
  printf 'not ok - %s\n' "$1" >&2
  [ -z "${2-}" ] || printf '%s\n' "$2" >&2
  failures=$((failures + 1))
}

# Polls a command for up to $deadline seconds.
wait_for() {
  local end=$((SECONDS + deadline))
  until "$@"; do
    [ "$SECONDS" -lt "$end" ] || return 1
    sleep 0.1
  done
}
# A reaped or zombie process counts as dead; kill -0 succeeds on a zombie.
dead() {
  local stat
  stat=$(ps -o stat= -p "$1" 2>/dev/null || true)
  [ -z "$stat" ] || [[ $stat == Z* ]]
}

# TERM, then KILL once the deadline passes. Returns non-zero if the process
# outlived both.
stop_pid() {
  local pid=$1
  dead "$pid" && return 0
  kill -TERM "$pid" 2>/dev/null || true
  wait_for dead "$pid" && return 0
  kill -KILL "$pid" 2>/dev/null || true
  wait_for dead "$pid"
}

# --- Scenario: one launcher daemon in workspace $1, run as a child process so
# its cleanup can be observed from outside. BZI_FORCE_FAIL=1 aborts it after
# the daemon is up. Prints the daemon pid to $1/daemon.pid.
if [ "${1-}" = --scenario ]; then
  ws=$2
  pid=
  cleanup() {
    status=$?
    if [ -n "$pid" ] && ! stop_pid "$pid"; then
      echo "scenario: launcher pueued $pid survived TERM and KILL" >&2
      status=1
    fi
    exit "$status"
  }
  trap cleanup EXIT
  cd "$ws"
  CLAUDE_PROJECT_DIR=$ws "$launcher" pueued >"$ws/pueued.log" 2>&1 &
  pid=$!
  printf '%s\n' "$pid" >"$ws/daemon.pid"
  sock="$ws/build/sortie-agent-state/pueue/pueue.sock"
  if ! wait_for test -S "$sock"; then
    echo "scenario: pueued never bound $sock" >&2
    cat "$ws/pueued.log" >&2
    exit 1
  fi
  CLAUDE_PROJECT_DIR=$ws "$launcher" pueue add --stashed -- echo owned-task >/dev/null
  CLAUDE_PROJECT_DIR=$ws "$launcher" pueue status --json >"$ws/status.json"
  if [ "${BZI_FORCE_FAIL-}" = 1 ]; then
    echo "scenario: forced failure" >&2
    exit 3
  fi
  exit 0
fi

for tool in pueued pueue jq; do
  if ! command -v "$tool" >/dev/null; then
    not_ok "$tool is on PATH" "run this check inside the development shell (nix develop)"
    exit 1
  fi
done

# Short private roots keep every socket path within the platform limit.
tmp=$(mktemp -d /tmp/bzi.XXXXXX)
tmp=$(cd "$tmp" && pwd -P)
sentinel_pid=
finish() {
  status=$?
  if [ -n "$sentinel_pid" ]; then stop_pid "$sentinel_pid" || true; fi
  rm -rf "$tmp"
  exit "$status"
}
trap finish EXIT

# Nothing may fall back to the invoking user's configuration or data.
export HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/home/.config"
export XDG_DATA_HOME="$tmp/home/.local/share" XDG_RUNTIME_DIR="$tmp/home/run"
unset PUEUE_CONFIG_PATH BUSYBEE_STATE_DIR
mkdir -p "$HOME"

new_workspace() {
  mkdir -p "$tmp/$1"
  : >"$tmp/$1/flake.nix"
  printf '%s\n' "$tmp/$1"
}

# --- Sentinel daemon with a known task list.
sentinel="$tmp/sentinel"
mkdir -p "$sentinel/data"
cat >"$sentinel/pueue.yml" <<EOF
shared:
  pueue_directory: $sentinel/data
  runtime_directory: $sentinel/data
  alias_file: $sentinel/data/aliases.yml
  use_unix_socket: true
  unix_socket_path: $sentinel/data/pueue.sock
EOF
pueued --config "$sentinel/pueue.yml" >"$sentinel/pueued.log" 2>&1 &
sentinel_pid=$!
if ! wait_for test -S "$sentinel/data/pueue.sock"; then
  not_ok 'sentinel pueued starts' "$(cat "$sentinel/pueued.log")"
  exit 1
fi
pueue --config "$sentinel/pueue.yml" add --stashed -- echo sentinel-task >/dev/null
sentinel_tasks() {
  pueue --config "$sentinel/pueue.yml" status --json \
    | jq -c '[.tasks[] | {id, command, status}]'
}
sentinel_before=$(sentinel_tasks)
sentinel_unchanged() {
  ! dead "$sentinel_pid" && [ "$(sentinel_tasks)" = "$sentinel_before" ]
}

# --- isolated_launcher_uses_config_file
ws=$(new_workspace config)
fake_bin="$tmp/fake-bin"
mkdir -p "$fake_bin"
cat >"$fake_bin/pueue" <<'EOF'
#!/bin/sh
printf '%s\n' "$PUEUE_CONFIG_PATH"
EOF
chmod +x "$fake_bin/pueue"
config=$(cd "$ws" && PATH="$fake_bin:$PATH" CLAUDE_PROJECT_DIR=$ws "$launcher" pueue)
state="$ws/build/sortie-agent-state/pueue"
expected=$(cat <<EOF
shared:
  pueue_directory: "$state"
  runtime_directory: "$state"
  alias_file: "$state/aliases.yml"
  use_unix_socket: true
  unix_socket_path: "$state/pueue.sock"
EOF
)
if [ "$config" != "$ws/build/sortie-agent-state/pueue.yml" ]; then
  not_ok 'isolated_launcher_uses_config_file' "PUEUE_CONFIG_PATH=$config"
elif [ -L "$config" ] || [ ! -f "$config" ]; then
  not_ok 'isolated_launcher_uses_config_file' "$config is not a regular file"
elif [ "$(cat "$config")" != "$expected" ]; then
  not_ok 'isolated_launcher_uses_config_file' "$(diff <(printf '%s\n' "$expected") "$config" || true)"
else
  # Pueue's own parser: the daemon must accept the file and bind where it says.
  pueued --config "$config" >"$ws/pueued.log" 2>&1 &
  parse_pid=$!
  if wait_for test -S "$state/pueue.sock"; then
    ok 'isolated_launcher_uses_config_file'
  else
    not_ok 'isolated_launcher_uses_config_file' "pueued did not bind $state/pueue.sock: $(cat "$ws/pueued.log")"
  fi
  stop_pid "$parse_pid" || not_ok 'isolated_launcher_uses_config_file' "pueued $parse_pid did not stop"
fi

# --- isolated_launcher_reaches_only_owned_pueue
ws=$(new_workspace owned)
if ! out=$(bash "$self" --scenario "$ws" 2>&1); then
  not_ok 'isolated_launcher_reaches_only_owned_pueue' "$out"
elif ! owned=$(jq -c '[.tasks[] | .command]' "$ws/status.json") ||
     [ "$owned" != '["echo owned-task"]' ]; then
  not_ok 'isolated_launcher_reaches_only_owned_pueue' "launcher daemon tasks: $owned"
elif [ ! -f "$ws/build/sortie-agent-state/pueue/state.json" ]; then
  not_ok 'isolated_launcher_reaches_only_owned_pueue' \
    "no state.json in the workspace: $(ls -A "$ws/build/sortie-agent-state/pueue")"
elif ! sentinel_unchanged; then
  not_ok 'isolated_launcher_reaches_only_owned_pueue' \
    "sentinel changed: $sentinel_before -> $(sentinel_tasks)"
elif [ -n "$(ls -A "$HOME")" ]; then
  not_ok 'isolated_launcher_reaches_only_owned_pueue' \
    "wrote outside the workspace: $(cd "$HOME" && find . -mindepth 1)"
else
  ok 'isolated_launcher_reaches_only_owned_pueue'
fi

# --- isolated_launcher_cleans_up
cleanup_case() {
  local name=$1 want=$2 ws pid rc=0 out
  ws=$(new_workspace "cleanup-$name")
  local start=$SECONDS
  out=$(BZI_FORCE_FAIL=$want bash "$self" --scenario "$ws" 2>&1) || rc=$?
  pid=$(cat "$ws/daemon.pid" 2>/dev/null || true)
  if [ "$want" = 1 ] && [ "$rc" -ne 3 ]; then
    not_ok "isolated_launcher_cleans_up ($name)" "did not reach the forced failure (exit $rc): $out"
  elif [ "$want" = 0 ] && [ "$rc" -ne 0 ]; then
    not_ok "isolated_launcher_cleans_up ($name)" "$out"
  elif [ -z "$pid" ]; then
    not_ok "isolated_launcher_cleans_up ($name)" "no daemon pid recorded: $out"
  elif ! dead "$pid"; then
    not_ok "isolated_launcher_cleans_up ($name)" "launcher pueued $pid still running"
    kill -KILL "$pid" 2>/dev/null || true
  elif [ $((SECONDS - start)) -gt $((3 * deadline)) ]; then
    not_ok "isolated_launcher_cleans_up ($name)" "took $((SECONDS - start))s"
  elif ! sentinel_unchanged; then
    not_ok "isolated_launcher_cleans_up ($name)" "sentinel changed or stopped"
  else
    ok "isolated_launcher_cleans_up ($name)"
  fi
}
cleanup_case success 0
cleanup_case forced-failure 1

if [ "$failures" -ne 0 ]; then
  printf '%s isolated-launcher test(s) failed\n' "$failures" >&2
  exit 1
fi
