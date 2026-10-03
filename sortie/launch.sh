#!/usr/bin/env bash
# The lab dispatcher's launcher (sortie/README.md). No host install.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [ "${BUSYBEE_SORTIE_IN_SHELL:-}" != 1 ]; then
  export BUSYBEE_SORTIE_IN_SHELL=1
  exec nix develop "$root#agent" -c bash "$root/sortie/launch.sh" "$@"
fi

runner=codex
mode=run
while [ "$#" -gt 0 ]; do
  case "$1" in
    --agent) runner=${2:?--agent requires claude, codex, or cursor}; shift 2 ;;
    --validate) mode=validate; shift ;;
    --dry-run) mode=dry-run; shift ;;
    *) echo "Usage: sortie/run-lab.sh [--agent claude|codex|cursor] [--validate|--dry-run]" >&2; exit 2 ;;
  esac
done

case "$runner" in
  claude) kind=claude-code; agent_command=claude ;;
  codex) kind=codex; agent_command='codex app-server' ;;
  cursor) kind=agent-client-protocol; agent_command=${BUSYBEE_CURSOR_COMMAND:-agent acp} ;;
  *) echo "Unsupported agent: $runner" >&2; exit 2 ;;
esac
export BUSYBEE_SORTIE_AGENT_KIND=$kind
export SORTIE_AGENT_KIND=$kind SORTIE_AGENT_COMMAND=$agent_command
workflow=LAB_WORKFLOW.md
export BUSYBEE_SORTIE_STATE="$root/build/sortie-lab"
export BUSYBEE_SORTIE_CLONE_URL=${BUSYBEE_SORTIE_CLONE_URL:-https://github.com/githappens/busybee.git}
export BUSYBEE_SORTIE_TRUSTED=$root
# Issue checkouts: under the lab state by default, or in an operator's shared
# directory, where Sortie keeps them in .sortie/busybee/ and each is shown as
# busybee-<issue> (prepare-workspace.sh).
if [ -n "${BUSYBEE_SORTIE_WORKSPACES:-}" ]; then
  case "$BUSYBEE_SORTIE_WORKSPACES" in
    /*) ;;
    *) echo 'BUSYBEE_SORTIE_WORKSPACES must be an absolute path.' >&2; exit 2 ;;
  esac
  [ -d "$BUSYBEE_SORTIE_WORKSPACES" ] || {
    echo "BUSYBEE_SORTIE_WORKSPACES names $BUSYBEE_SORTIE_WORKSPACES, which is not a directory." >&2; exit 2; }
  export BUSYBEE_SORTIE_WORKSPACE_ROOT="$BUSYBEE_SORTIE_WORKSPACES/.sortie/busybee"
else
  export BUSYBEE_SORTIE_WORKSPACE_ROOT="$BUSYBEE_SORTIE_STATE/workspaces"
fi

cd "$root"
if [ "$mode" = validate ]; then
  GITHUB_TOKEN=validation-only sortie validate "$root/sortie/$workflow"
  exit
fi

# Turns run the agent inside the worker, from its development shell.
if [ -z "${GITHUB_TOKEN:-}" ]; then
  GITHUB_TOKEN=$(gh auth token)
  export GITHUB_TOKEN
fi
if [ "$mode" = dry-run ]; then
  exec sortie --dry-run "$root/sortie/$workflow"
fi
if [ "$runner" = cursor ]; then
  echo 'Cursor has not been qualified inside lab workers; use claude or codex.' >&2
  exit 1
fi

# Snapshot committed, reviewed policy and controller outside every issue
# checkout. Never execute a candidate branch's launcher or replace policy
# during a session.
ref=${BUSYBEE_SORTIE_TRUSTED_REF:-origin/main}
trusted=$(bash "$root/sortie/snapshot.sh" "$ref" "$BUSYBEE_SORTIE_STATE")
export BUSYBEE_SORTIE_TRUSTED=$trusted
# Sortie splits the command on whitespace, and the guest mirrors the
# workspace path; refuse paths either would break.
case "$root$trusted$BUSYBEE_SORTIE_WORKSPACE_ROOT" in
  *[[:space:]]*) echo 'The lab needs checkout and workspace paths without whitespace.' >&2; exit 1 ;;
esac
export BUSYBEE_LAB_ROOT=$root
export SORTIE_AGENT_COMMAND="python3 $trusted/scripts/vm/vmctl.py --root $root session agent -- $agent_command"
# Hooks see only SORTIE_* copies of these (sortie/README.md, "What hooks see").
export SORTIE_BUSYBEE_TRUSTED=$trusted SORTIE_BUSYBEE_LAB_ROOT=$root
export SORTIE_BUSYBEE_CLONE_URL=$BUSYBEE_SORTIE_CLONE_URL SORTIE_BUSYBEE_AGENT_KIND=$kind
export SORTIE_BUSYBEE_WORKSPACES=${BUSYBEE_SORTIE_WORKSPACES:-} SORTIE_BUSYBEE_GITHUB_TOKEN=$GITHUB_TOKEN
sortie validate "$trusted/sortie/$workflow"

# A mkdir lock prevents a second process from dispatching the same issues.
# A stale lock is an explicit startup failure; inspect before removing it.
mkdir -p "$BUSYBEE_SORTIE_STATE"
lock="$BUSYBEE_SORTIE_STATE/launcher.lock"
mkdir "$lock" || { echo 'Launcher lock exists; inspect the previous process before retrying.' >&2; exit 1; }
printf '%s\n' "$$" > "$lock/pid"
sidecar=""; orchestrator=""
cleanup() {
  for pid in "$sidecar" "$orchestrator"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then kill "$pid"; fi
  done
  rm -f "$lock/pid"
  rmdir "$lock"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# Links to checkouts Sortie has since removed are dropped; nothing else is.
if [ -n "${BUSYBEE_SORTIE_WORKSPACES:-}" ]; then
  for link in "$BUSYBEE_SORTIE_WORKSPACES"/busybee-*; do
    if [ -L "$link" ] && [ ! -e "$link" ]; then
      case $(readlink "$link") in "$BUSYBEE_SORTIE_WORKSPACE_ROOT"/*) rm "$link" ;; esac
    fi
  done
fi
(
  while :; do
    python3 "$trusted/sortie/lab.py" release || echo 'Lab dependency release failed' >&2
    sleep 60
  done
) &
sidecar=$!
sortie --port "${BUSYBEE_SORTIE_PORT:-7679}" "$trusted/sortie/$workflow" &
orchestrator=$!
wait "$orchestrator"
