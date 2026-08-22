#!/usr/bin/env bash
# Agent launcher used by sortie (agent.command). Runs with the workspace as
# the working directory. Picks the Claude model per issue: the before_run
# hook writes `.sortie/model` when the issue carries a `model:<name>` label;
# without it the default below applies.
set -euo pipefail
DEFAULT_MODEL=opus
model="$DEFAULT_MODEL"
if [ -s .sortie/model ]; then
  model="$(tr -d '[:space:]' < .sortie/model)"
fi
exec claude --model "$model" "$@"
