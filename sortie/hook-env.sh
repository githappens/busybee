# Sourced (POSIX sh) first by every LAB_WORKFLOW.md hook and the review triage
# script, from the trusted snapshot. Sortie strips every unprefixed variable
# from hook environments (sortie/README.md, "What hooks see"), so launch.sh
# also exports its settings as SORTIE_BUSYBEE_*. This restores the names the
# lab scripts read; a missing setting stops the hook with a message naming it.

# The caller already required SORTIE_BUSYBEE_TRUSTED to find this file.
export BUSYBEE_SORTIE_TRUSTED="$SORTIE_BUSYBEE_TRUSTED"
export BUSYBEE_LAB_ROOT="${SORTIE_BUSYBEE_LAB_ROOT:?is not set. Start the lab with sortie/run-lab.sh, which exports it for hooks.}"
export BUSYBEE_SORTIE_CLONE_URL="${SORTIE_BUSYBEE_CLONE_URL:?is not set. Start the lab with sortie/run-lab.sh, which exports it for hooks.}"
export BUSYBEE_SORTIE_AGENT_KIND="${SORTIE_BUSYBEE_AGENT_KIND:?is not set. Start the lab with sortie/run-lab.sh, which exports it for hooks.}"
# The tracker identity, so gh in hooks acts as the account Sortie polls with.
export GITHUB_TOKEN="${SORTIE_BUSYBEE_GITHUB_TOKEN:?is not set. Start the lab with sortie/run-lab.sh, which exports it for hooks.}"
# Optional: an operator's shared workspace directory; empty means none.
export BUSYBEE_SORTIE_WORKSPACES="${SORTIE_BUSYBEE_WORKSPACES:-}"
