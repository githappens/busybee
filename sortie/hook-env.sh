# Sourced (POSIX sh) first by every LAB_WORKFLOW.md hook and the review triage
# script, from the trusted snapshot.
#
# Sortie runs hooks as `sh -c` with only a small system allowlist (PATH, HOME,
# USER, TMPDIR, SSH_AUTH_SOCK, ...) and the SORTIE_* variables of its own
# environment; everything else the launcher exported is stripped. launch.sh
# therefore also exports its settings as SORTIE_BUSYBEE_*. This restores the
# names the lab scripts read, and stops the hook with a clear message when the
# launcher did not provide one, rather than letting it run with an empty value.
# The agent command is not filtered: it inherits the launcher's environment.

for busybee_name in SORTIE_BUSYBEE_TRUSTED SORTIE_BUSYBEE_LAB_ROOT SORTIE_BUSYBEE_CLONE_URL \
  SORTIE_BUSYBEE_AGENT_KIND SORTIE_BUSYBEE_GITHUB_TOKEN; do
  eval "busybee_value=\${$busybee_name:-}"
  if [ -z "$busybee_value" ]; then
    echo "sortie hook: $busybee_name is not set. Start the lab with sortie/run-lab.sh, which exports it for hooks." >&2
    exit 2
  fi
done
unset busybee_name busybee_value

export BUSYBEE_SORTIE_TRUSTED="$SORTIE_BUSYBEE_TRUSTED"
export BUSYBEE_LAB_ROOT="$SORTIE_BUSYBEE_LAB_ROOT"
export BUSYBEE_SORTIE_CLONE_URL="$SORTIE_BUSYBEE_CLONE_URL"
export BUSYBEE_SORTIE_AGENT_KIND="$SORTIE_BUSYBEE_AGENT_KIND"
# The tracker identity, so gh in hooks acts as the account Sortie polls with.
export GITHUB_TOKEN="$SORTIE_BUSYBEE_GITHUB_TOKEN"
# Optional: an operator's shared workspace directory.
if [ -n "${SORTIE_BUSYBEE_WORKSPACES:-}" ]; then
  export BUSYBEE_SORTIE_WORKSPACES="$SORTIE_BUSYBEE_WORKSPACES"
else
  unset BUSYBEE_SORTIE_WORKSPACES
fi
