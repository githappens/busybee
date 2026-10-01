#!/usr/bin/env bash
# A separate process/state directory for the VM-lab milestone. No host install.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [ "${BUSYBEE_SORTIE_IN_SHELL:-}" != 1 ]; then
  export BUSYBEE_SORTIE_IN_SHELL=1
  exec nix develop "$root#agent" -c bash "$root/sortie/run-lab.sh" "$@"
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
export BUSYBEE_SORTIE_STATE="$root/build/sortie-lab"
export BUSYBEE_SORTIE_CLONE_URL=${BUSYBEE_SORTIE_CLONE_URL:-https://github.com/githappens/busybee.git}
export BUSYBEE_SORTIE_TRUSTED=$root

cd "$root"
if [ "$mode" = validate ]; then
  GITHUB_TOKEN=validation-only sortie validate "$root/sortie/LAB_WORKFLOW.md"
  exit
fi

command -v "${agent_command%% *}" >/dev/null || { echo "Agent executable unavailable: ${agent_command%% *}" >&2; exit 1; }
if [ -z "${GITHUB_TOKEN:-}" ]; then
  GITHUB_TOKEN=$(gh auth token)
  export GITHUB_TOKEN
fi
if [ "$mode" = dry-run ]; then
  exec sortie --dry-run "$root/sortie/LAB_WORKFLOW.md"
fi
if [ "$runner" = claude ] && [ "${BUSYBEE_SORTIE_WORKER:-}" != 1 ]; then
  echo 'Sortie 1.24 requires Claude bypassPermissions; this profile requires an allocated worker. Use Codex for host bootstrap.' >&2
  exit 1
fi

# Snapshot committed, reviewed policy outside every issue checkout. Never
# execute a candidate branch's launcher or replace policy during a session.
ref=${BUSYBEE_SORTIE_TRUSTED_REF:-origin/main}
sha=$(git rev-parse --verify "$ref^{commit}")
git cat-file -e "$sha:sortie/LAB_WORKFLOW.md" || {
  echo 'The selected trusted revision has no lab workflow; land the bootstrap first.' >&2
  exit 1
}
mkdir -p "$BUSYBEE_SORTIE_STATE/trusted"
trusted="$BUSYBEE_SORTIE_STATE/trusted/$sha"
if [ ! -d "$trusted" ]; then
  candidate=$(mktemp -d "$BUSYBEE_SORTIE_STATE/trusted/.candidate.XXXXXX")
  git archive "$sha" sortie skills docs/development/agent-review.md AGENTS.md CLAUDE.md | tar -x -C "$candidate"
  mv "$candidate" "$trusted"
fi
export BUSYBEE_SORTIE_TRUSTED=$trusted
sortie validate "$trusted/sortie/LAB_WORKFLOW.md"

# A mkdir lock prevents a second lab process from dispatching the same issues.
# A stale lock is an explicit startup failure; inspect before removing it.
lock="$BUSYBEE_SORTIE_STATE/launcher.lock"
mkdir "$lock" || { echo 'Lab launcher lock exists; inspect the previous process before retrying.' >&2; exit 1; }
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
    python3 "$trusted/sortie/lab.py" release || echo 'Lab dependency release failed' >&2
    sleep 60
  done
) &
sidecar=$!
sortie --port "${BUSYBEE_SORTIE_PORT:-7679}" "$trusted/sortie/LAB_WORKFLOW.md" &
orchestrator=$!
wait "$orchestrator"
