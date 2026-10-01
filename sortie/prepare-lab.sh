#!/usr/bin/env bash
set -euo pipefail

trusted=${BUSYBEE_SORTIE_TRUSTED:?trusted controller directory required}
issue=${SORTIE_ISSUE_IDENTIFIER:?issue number required}
[[ "$issue" =~ ^[1-9][0-9]*$ ]] || { echo 'Invalid issue number' >&2; exit 2; }
python3 "$trusted/sortie/lab.py" check --issue "$issue"

# Fetch and push use the dispatcher's GitHub identity. Clear inherited helpers
# in this checkout only; an unrelated cached account must not take precedence.
git config --local --replace-all credential.helper ''
git config --local --add credential.helper '!gh auth git-credential'
git fetch -q origin main
branch="sortie-lab/$issue"
if git rev-parse -q --verify "$branch" >/dev/null; then
  git checkout -q "$branch"
elif git rev-parse -q --verify "origin/$branch" >/dev/null; then
  git checkout -q -b "$branch" --track "origin/$branch"
else
  git checkout -q -b "$branch" origin/main
fi

mkdir -p .sortie
gitdir=$(git rev-parse --git-path info/exclude)
grep -qxF '/.sortie/' "$gitdir" || printf '/.sortie/\n' >> "$gitdir"
# Persist policy identity for continuations without copying or executing the
# issue branch's launcher. Claude's existing guard is a cooperative safeguard;
# the VM controller, once implemented, supplies the worker boundary.
printf '%s\n' "$trusted" > .sortie/trusted-controller
if [ "$BUSYBEE_SORTIE_AGENT_KIND" = claude-code ]; then
  for path in /.claude/hooks/ /.claude/settings.json /.claude/isolated.sh; do
    grep -qxF "$path" "$gitdir" || printf '%s\n' "$path" >> "$gitdir"
  done
  mkdir -p .claude/hooks
  cp "$trusted/sortie/machine-safety-hook.sh" .claude/hooks/machine-safety-hook.sh
  cp "$trusted/sortie/claude-settings.json" .claude/settings.json
  cp "$trusted/sortie/isolated.sh" .claude/isolated.sh
  chmod 0755 .claude/hooks/machine-safety-hook.sh .claude/isolated.sh
fi
