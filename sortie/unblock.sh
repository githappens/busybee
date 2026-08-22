#!/usr/bin/env bash
# Release dependents: add the `sortie` marker label to every open issue in the
# milestone that is `sortie:ready`, lacks the marker, and whose blocked-by
# issues are all closed. Idempotent; safe to run from a loop.
#
# Why: sortie's GitHub adapter only fills blocked-by for single-issue fetches,
# not for the candidate list, so its own dependency gate never fires. Until
# that is fixed upstream, the marker label *is* the readiness gate: blocked
# issues carry `sortie:ready` for the board but no marker, and this script
# hands out the marker when the dependencies are merged.
set -euo pipefail
REPO="${REPO:-githappens/busybee}"
MILESTONE="${MILESTONE:-bzbd: shared CPU token pool}"

released=0
while read -r n; do
  [ -n "$n" ] || continue
  open_blockers=$(gh api "repos/$REPO/issues/$n/dependencies/blocked_by" --jq '[.[] | select(.state=="open") | .number] | join(" ")')
  if [ -z "$open_blockers" ]; then
    gh issue edit "$n" --repo "$REPO" --add-label sortie >/dev/null
    echo "released #$n"; released=$((released+1))
  else
    [ "${VERBOSE:-}" = 1 ] && echo "#$n waits on: $open_blockers"
  fi
done < <(gh issue list --repo "$REPO" --milestone "$MILESTONE" --label sortie:ready --state open --limit 100 --json number,labels \
          --jq '.[] | select([.labels[].name] | index("sortie") | not) | .number')
[ "$released" -gt 0 ] || [ "${VERBOSE:-}" != 1 ] || echo "nothing to release"
