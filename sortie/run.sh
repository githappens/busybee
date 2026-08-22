#!/usr/bin/env bash
# Launch the sortie orchestrator for this repository.
#
#   sortie/run.sh              # run
#   sortie/run.sh --dry-run    # one poll cycle, no agents, no state written
#
# PORT is this instance's HTTP/metrics port. One sortie process serves one
# WORKFLOW.md, so every project gets its own port; the Prometheus scrape
# target for this project (label project=busybee) must use the same value.
set -euo pipefail

PORT=7678
WORKFLOW="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/WORKFLOW.md"
REPO_ROOT="$(dirname "$(dirname "$WORKFLOW")")"

die() { echo "sortie/run.sh: $*" >&2; exit 1; }

command -v sortie >/dev/null || die "sortie not on PATH"
command -v gh >/dev/null     || die "gh not on PATH"
[ -f "$REPO_ROOT/flake.nix" ] || die "flake.nix not found at $REPO_ROOT"

# Tracker token: sortie reads $GITHUB_TOKEN (see tracker.api_key); reuse gh's.
if [ -z "${GITHUB_TOKEN:-}" ]; then
  GITHUB_TOKEN="$(gh auth token 2>/dev/null)" || die "gh is not authenticated (gh auth login)"
  export GITHUB_TOKEN
fi

# Workspaces clone over the automation alias; fail here rather than in a hook.
# (GitHub ends the -T handshake with exit 1 even on success; inspect the text.)
ssh_out="$(ssh -o BatchMode=yes -T git@github.com-sortie 2>&1 || true)"
grep -q "successfully authenticated" <<<"$ssh_out" \
  || die "ssh alias github.com-sortie does not authenticate: $ssh_out"

# Refuse to double-start: a second instance on the same port would also
# double-dispatch the same issues.
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  die "port $PORT is already in use; is another sortie for this repo running?"
fi

cd "$REPO_ROOT"
mkdir -p build/sortie-workspaces

# Sidecar: release dependents whose blockers have merged (sortie 1.21 does not
# gate on blocked-by itself; see sortie/unblock.sh). Skipped for --dry-run.
# Sidecar 2: nudge the review gate. A clean Codex verdict is a reaction, which
# raises no workflow event, and the gate's cron trigger is throttled on quiet
# repositories; a lone clean PR would otherwise wait for the next unrelated
# event. A dispatch with no PR argument evaluates every open PR and exits
# quickly when nothing needs judging.
unblock_pid=""; gate_pid=""
case " $* " in *" --dry-run "*) ;; *)
  ( while sleep 60; do "$REPO_ROOT/sortie/unblock.sh" 2>/dev/null | sed 's/^/unblock: /'; done ) &
  unblock_pid=$!
  ( while sleep 600; do gh workflow run codex-gate.yml --repo githappens/busybee >/dev/null 2>&1 || echo "gate nudge: dispatch failed"; done ) &
  gate_pid=$!
  trap 'for p in "$unblock_pid" "$gate_pid"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done' EXIT
  ;;
esac

nix develop -c sortie --port "$PORT" "$@" "$WORKFLOW"
