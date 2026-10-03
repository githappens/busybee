#!/usr/bin/env bash
# Shared launcher for the product and lab profiles. No host install.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [ "${BUSYBEE_SORTIE_IN_SHELL:-}" != 1 ]; then
  export BUSYBEE_SORTIE_IN_SHELL=1
  exec nix develop "$root#agent" -c bash "$root/sortie/launch.sh" "$@"
fi

runner=codex
profile=product
mode=run
while [ "$#" -gt 0 ]; do
  case "$1" in
    --agent) runner=${2:?--agent requires claude, codex, or cursor}; shift 2 ;;
    --profile) profile=${2:?--profile requires product or lab}; shift 2 ;;
    --validate) mode=validate; shift ;;
    --dry-run) mode=dry-run; shift ;;
    *) echo "Usage: sortie/launch.sh [--profile product|lab] [--agent claude|codex|cursor] [--validate|--dry-run]" >&2; exit 2 ;;
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
case "$profile" in
  product) workflow=WORKFLOW.md; port=7678; export BUSYBEE_SORTIE_STATE="$root/build" ;;
  lab) workflow=LAB_WORKFLOW.md; port=7679; export BUSYBEE_SORTIE_STATE="$root/build/sortie-lab" ;;
  *) echo "Unsupported profile: $profile" >&2; exit 2 ;;
esac
export BUSYBEE_SORTIE_PROFILE=$profile
export BUSYBEE_SORTIE_CLONE_URL=${BUSYBEE_SORTIE_CLONE_URL:-https://github.com/githappens/busybee.git}
export BUSYBEE_SORTIE_TRUSTED=$root

cd "$root"
if [ "$mode" = validate ]; then
  GITHUB_TOKEN=validation-only sortie validate "$root/sortie/$workflow"
  exit
fi

# Lab turns run the agent inside the worker, from its development shell.
if [ "$profile" = product ]; then
  command -v "${agent_command%% *}" >/dev/null || { echo "Agent executable unavailable: ${agent_command%% *}" >&2; exit 1; }
fi
if [ -z "${GITHUB_TOKEN:-}" ]; then
  GITHUB_TOKEN=$(gh auth token)
  export GITHUB_TOKEN
fi
if [ "$mode" = dry-run ]; then
  exec sortie --dry-run "$root/sortie/$workflow"
fi
# Lab agents run each turn inside their own VM worker (scripts/vm/session.py);
# product agents run on this host, where Claude's required bypassPermissions
# mode is refused.
if [ "$profile" = product ] && [ "$runner" = claude ] && [ "${BUSYBEE_SORTIE_WORKER:-}" != 1 ]; then
  echo 'Sortie 1.24 requires Claude bypassPermissions; on this host only the lab profile, whose turns run in an allocated worker, may use it. Use Codex for host bootstrap.' >&2
  exit 1
fi
if [ "$profile" = lab ] && [ "$runner" = cursor ]; then
  echo 'Cursor has not been qualified inside lab workers; use claude or codex with the lab profile.' >&2
  exit 1
fi

# Snapshot committed, reviewed policy and controller outside every issue
# checkout. Never execute a candidate branch's launcher or replace policy
# during a session.
ref=${BUSYBEE_SORTIE_TRUSTED_REF:-origin/main}
trusted=$(bash "$root/sortie/snapshot.sh" "$ref" "$BUSYBEE_SORTIE_STATE")
export BUSYBEE_SORTIE_TRUSTED=$trusted
if [ "$profile" = lab ]; then
  # Sortie splits the command on whitespace; refuse paths it would break.
  case "$root$trusted" in
    *[[:space:]]*) echo 'The lab profile needs a checkout path without whitespace.' >&2; exit 1 ;;
  esac
  export BUSYBEE_LAB_ROOT=$root
  export SORTIE_AGENT_COMMAND="python3 $trusted/scripts/vm/vmctl.py --root $root session agent -- $agent_command"
fi
sortie validate "$trusted/sortie/$workflow"

# A mkdir lock prevents a second process from dispatching the same issues.
# A stale lock is an explicit startup failure; inspect before removing it.
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
(
  while :; do
    if [ "$profile" = lab ]; then
      python3 "$trusted/sortie/lab.py" release || echo 'Lab dependency release failed' >&2
    else
      bash "$trusted/sortie/unblock.sh" || echo 'Product dependency release failed' >&2
    fi
    sleep 60
  done
) &
sidecar=$!
sortie --port "${BUSYBEE_SORTIE_PORT:-$port}" "$trusted/sortie/$workflow" &
orchestrator=$!
wait "$orchestrator"
