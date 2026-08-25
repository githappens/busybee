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

# Refuse a state directory that physically resolves outside the workspace
# (for example a symlink to the user's config). mkdir first so the path
# exists, then resolve it.
ensure_workspace_dir() {
  local dir=$1 resolved root_resolved
  mkdir -p "$dir"
  resolved=$(cd "$dir" && pwd -P) || {
    echo "isolated.sh: cannot resolve $dir" >&2
    exit 2
  }
  root_resolved=$(cd "$root" && pwd -P) || {
    echo "isolated.sh: cannot resolve workspace root" >&2
    exit 2
  }
  case "$resolved" in
    "$root_resolved"|"$root_resolved"/*) ;;
    *)
      echo "isolated.sh: $dir resolves outside the workspace" >&2
      exit 2
      ;;
  esac
}

ensure_workspace_dir "$busybee_state"
ensure_workspace_dir "$pueue_state"

export BUSYBEE_STATE_DIR=$busybee_state
export PUEUE_CONFIG_PATH=$pueue_state
exec "$tool" "$@"
