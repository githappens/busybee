#!/usr/bin/env bash
# Materialize the machine-safety payload into a sortie workspace.
# Invoked by WORKFLOW.md's before_run; MACHINE_SAFETY_REF names the git
# object (origin/main, or HEAD until that blob exists on main).
set -euo pipefail

ref=${MACHINE_SAFETY_REF:?MACHINE_SAFETY_REF is required}

grep -qxF '/.claude/' .git/info/exclude || printf '/.claude/\n' >> .git/info/exclude
mkdir -p .claude/hooks

git show "$ref:sortie/machine-safety-hook.sh" > .claude/hooks/machine-safety-hook.sh
chmod 0755 .claude/hooks/machine-safety-hook.sh
git show "$ref:sortie/claude-settings.json" > .claude/settings.json
chmod 0644 .claude/settings.json
git show "$ref:sortie/isolated.sh" > .claude/isolated.sh
chmod 0755 .claude/isolated.sh
