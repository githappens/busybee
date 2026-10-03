#!/usr/bin/env bash
# Workspace-local Busy Bee / Pueue launcher.
#
# Usage: .claude/isolated.sh <busybee|bzb|bzbd|pueued|pueue> [args...]
#
# Sets BUSYBEE_STATE_DIR under this workspace and points PUEUE_CONFIG_PATH at a
# generated workspace-local Pueue YAML file whose data, runtime, alias and
# socket paths all sit in that workspace, then execs the tool. This is the only supported way for a sortie agent to launch
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

case "$root" in
  *[[:cntrl:]]*) echo "isolated.sh: workspace path contains a control character" >&2; exit 2 ;;
esac
ensure_workspace_dir "$state_root"
ensure_workspace_dir "$busybee_state"
ensure_workspace_dir "$pueue_state"

# sun_path holds the socket path and its terminating NUL. A longer path fails
# at bind time with an unclear error, so refuse it here instead.
case "$(uname -s)" in
  Darwin|*BSD) socket_limit=104 ;;
  *) socket_limit=108 ;;
esac
check_socket() {
  local LC_ALL=C
  if [ $((${#1} + 1)) -gt "$socket_limit" ]; then
    echo "isolated.sh: socket path $1 is ${#1} bytes; $(uname -s) allows $((socket_limit - 1)). Use a workspace with a shorter path." >&2
    exit 2
  fi
}
pueue_socket=$pueue_state/pueue.sock
check_socket "$busybee_state/bzbd.sock"
check_socket "$pueue_socket"

# Paths become YAML double-quoted scalars. The workspace path was checked for
# control characters above, so escaping backslash and quote is sufficient.
yaml_string() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  printf '"%s"' "$s"
}

# Regenerated on every launch, so it always matches this workspace. Refuse a
# symlink or other non-file there rather than writing through it.
pueue_config=$state_root/pueue.yml
if [ -L "$pueue_config" ] || { [ -e "$pueue_config" ] && [ ! -f "$pueue_config" ]; }; then
  echo "isolated.sh: $pueue_config exists and is not a regular file" >&2
  exit 2
fi
pueue_config_tmp=$(mktemp "$state_root/.pueue.yml.XXXXXX")
{
  printf 'shared:\n'
  printf '  pueue_directory: %s\n' "$(yaml_string "$pueue_state")"
  printf '  runtime_directory: %s\n' "$(yaml_string "$pueue_state")"
  printf '  alias_file: %s\n' "$(yaml_string "$pueue_state/aliases.yml")"
  printf '  use_unix_socket: true\n'
  printf '  unix_socket_path: %s\n' "$(yaml_string "$pueue_socket")"
} >"$pueue_config_tmp"
mv -f "$pueue_config_tmp" "$pueue_config"

export BUSYBEE_STATE_DIR=$busybee_state
export PUEUE_CONFIG_PATH=$pueue_config
exec "$tool" "$@"
