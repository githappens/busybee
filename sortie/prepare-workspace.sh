#!/usr/bin/env bash
set -euo pipefail

trusted=${BUSYBEE_SORTIE_TRUSTED:?trusted controller directory required}
issue=${SORTIE_ISSUE_IDENTIFIER:?issue number required}
[[ "$issue" =~ ^[1-9][0-9]*$ ]] || { echo 'Invalid issue number' >&2; exit 2; }
python3 "$trusted/sortie/lab.py" check --issue "$issue"
branch="sortie-lab/$issue"

# The transport is the clone URL's. Over HTTPS, fetch and push use the
# dispatcher's `gh` identity: inherited helpers are cleared in this checkout
# only, so an unrelated cached account cannot take precedence. Over SSH, the
# operator's host alias selects a dedicated deploy key; no HTTPS helper is
# installed, and a key that would prompt (passphrase, hardware touch) fails
# instead of stalling an unattended run.
case $(git config remote.origin.url) in
  http://* | https://* | file://* | /*) ssh=0 ;;
  ssh://* | git+ssh://* | *@*:*) ssh=1 ;;
  *) ssh=0 ;;
esac
if [ "$ssh" = 1 ]; then
  git config --local core.sshCommand 'ssh -o BatchMode=yes'
else
  git config --local --replace-all credential.helper ''
  git config --local --add credential.helper '!gh auth git-credential'
fi
git fetch -q origin main
if git rev-parse -q --verify "$branch" >/dev/null; then
  git checkout -q "$branch"
elif git rev-parse -q --verify "origin/$branch" >/dev/null; then
  git checkout -q -b "$branch" --track "origin/$branch"
else
  git checkout -q -b "$branch" origin/main
fi

# A shared workspace root (sortie/README.md §Workspaces) shows each checkout as
# busybee-<issue> beside other projects' checkouts. Sortie refuses a linked
# workspace, so the link points at Sortie's directory, never the reverse. A
# name the operator already uses for something else is left alone.
if [ -n "${BUSYBEE_SORTIE_WORKSPACES:-}" ]; then
  link="$BUSYBEE_SORTIE_WORKSPACES/busybee-$issue"
  if [ -L "$link" ] || [ ! -e "$link" ]; then
    ln -sfn "$(pwd -P)" "$link"
  else
    echo "prepare-workspace: $link exists and is not a link; leaving it as it is (this checkout is $(pwd -P))" >&2
  fi
fi

mkdir -p .sortie
gitdir=$(git rev-parse --git-path info/exclude)
grep -qxF '/.sortie/' "$gitdir" || printf '/.sortie/\n' >> "$gitdir"
# Persist policy identity for continuations without copying or executing the
# issue branch's launcher. Claude's existing guard is a cooperative safeguard;
# the VM worker is the boundary.
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
