#!/usr/bin/env bash
# Usage: sortie/snapshot.sh REF STATE_DIR
#
# Snapshots a reviewed revision's dispatch scripts, VM controller and guest
# definitions, scenario runner, guard policy and review skills under STATE_DIR/trusted/<sha> and
# prints that directory. Reads only the committed revision, never a working
# tree or an issue branch, so a candidate edit cannot replace the policy or
# controller supervising its own session.
set -euo pipefail
ref=${1:?usage: snapshot.sh REF STATE_DIR}
state=${2:?usage: snapshot.sh REF STATE_DIR}
sha=$(git rev-parse --verify "$ref^{commit}")
git cat-file -e "$sha:sortie/prepare-workspace.sh" || {
  echo 'The selected trusted revision has no current controller; land the bootstrap first.' >&2
  exit 1
}
mkdir -p "$state/trusted"
trusted="$state/trusted/$sha"
# A snapshot is complete once it records its revision; an older or
# interrupted one is rebuilt rather than reused without the controller.
if [ -d "$trusted" ] && [ ! -f "$trusted/.revision" ]; then
  rm -rf "$trusted"
fi
if [ ! -d "$trusted" ]; then
  candidate=$(mktemp -d "$state/trusted/.candidate.XXXXXX")
  git archive "$sha" sortie skills scripts/vm infra/vm tests/scenarios docs/development/agent-review.md AGENTS.md \
    CLAUDE.md .github/workflows/agent-review-gate.yml | tar -x -C "$candidate"
  # Not a checkout: the controller reports this as its revision.
  printf '%s\n' "$sha" > "$candidate/.revision"
  mv "$candidate" "$trusted"
fi
printf '%s\n' "$trusted"
