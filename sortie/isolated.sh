#!/usr/bin/env bash
# Workspace-local Busy Bee / Pueue launcher.
#
# Usage: .claude/isolated.sh <busybee|bzb|bzbd|pueued|pueue> [args...]
#
# Sets BUSYBEE_STATE_DIR and PUEUE_CONFIG_PATH under this workspace, then
# execs the tool. This is the only supported way for a sortie agent to launch
# those programs; the PreToolUse hook denies every other spelling.
set -euo pipefail

root=${CLAUDE_PROJECT_DIR:-$PWD}
if [ ! -f "$root/flake.nix" ]; then
  echo "isolated.sh: CLAUDE_PROJECT_DIR/PWD is not a busybee workspace" >&2
  exit 2
fi

if [ "$#" -lt 1 ]; then
  echo "isolated.sh: usage: isolated.sh <busybee|bzb|bzbd|pueued|pueue> [args...]" >&2
  exit 2
fi

tool=$1
shift
case "$tool" in
  busybee|bzb|bzbd|pueued|pueue) ;;
  *)
    echo "isolated.sh: first argument must be busybee, bzb, bzbd, pueued, or pueue" >&2
    exit 2
    ;;
esac

state_root=$root/build/sortie-agent-state
busybee_state=$state_root/busybee
pueue_state=$state_root/pueue

# Refuse a state directory that resolves outside the workspace (for example a
# symlink to the user's config). A failed cd aborts via set -e.
root_resolved=$(cd "$root" && pwd -P)
ensure_workspace_dir() {
  local resolved
  mkdir -p "$1"
  resolved=$(cd "$1" && pwd -P)
  case "$resolved" in
    "$root_resolved"|"$root_resolved"/*) ;;
    *) echo "isolated.sh: $1 resolves outside the workspace" >&2; exit 2 ;;
  esac
}

ensure_workspace_dir "$busybee_state"
ensure_workspace_dir "$pueue_state"

export BUSYBEE_STATE_DIR=$busybee_state
export PUEUE_CONFIG_PATH=$pueue_state
exec "$tool" "$@"
