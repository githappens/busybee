#!/usr/bin/env bash
# Documented loop limits come from config, ground rules point at their source,
# and the dispatch profile uses the shared CI review/handoff contract.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
workflow="$root/sortie/LAB_WORKFLOW.md"
readme="$root/sortie/README.md"
failures=0

ok() { printf 'ok - %s\n' "$1"; }
not_ok() {
  printf 'not ok - %s\n' "$1" >&2
  failures=$((failures + 1))
}

if grep -E 'max [0-9]+ per issue' "$readme" >/dev/null; then
  not_ok 'README must not hardcode a bot-review continuation count'
else
  ok 'README does not hardcode a bot-review continuation count'
fi

if grep -q 'bot_review.max_continuation_turns' "$readme"; then
  ok 'README names reactions.bot_review.max_continuation_turns'
else
  not_ok 'README must name reactions.bot_review.max_continuation_turns as the source of truth'
fi

rules=$(awk '/^## Ground rules$/ {on=1; print; next} on && /^## / {exit} on' "$workflow")
while IFS= read -r heading; do
  line=$(printf '%s\n' "$rules" | grep -E "^[0-9]+\. \*\*${heading}\.\*\*" || true)
  if [ -z "$line" ]; then
    not_ok "ground rule '${heading}' missing"
    continue
  fi
  if ! printf '%s\n' "$line" | grep -Eq 'CLAUDE\.md|AGENTS\.md'; then
    not_ok "ground rule '${heading}' has no pointer to CLAUDE.md or AGENTS.md"
    continue
  fi
  # A duplicated rule that wraps is a drift hazard; keep it on one line.
  n=$(printf '%s\n' "$rules" | awk -v h="$heading" '
    $0 ~ "^[0-9]+\\. \\*\\*" h "\\.\\*\\*" {n=1; next}
    n && /^[0-9]+\. / {exit}
    n && /^## / {exit}
    n && NF {print; exit}
  ' | wc -l | tr -d ' ')
  if [ "$n" -eq 0 ]; then
    ok "ground rule '${heading}' is one line and points at its home"
  else
    not_ok "ground rule '${heading}' wraps onto a continuation line"
  fi
done <<'HEADINGS'
Read first
TDD
No silent fallbacks
Isolation
Public repo hygiene
HEADINGS

if printf '%s\n' "$rules" | grep -E '^[0-9]+\. \*\*Scope\.\*\*' | grep -q 'AGENTS.md'; then
  ok 'scope rule points at AGENTS.md'
else
  not_ok 'scope rule must point at AGENTS.md'
fi

if grep -q 'docs/development/agent-review.md' "$workflow" &&
   grep -q '\$BUSYBEE_SORTIE_TRUSTED/sortie/reviews.py handoff' "$workflow" &&
   grep -q '\$BUSYBEE_SORTIE_TRUSTED/sortie/review-triage.py' "$workflow"; then
  ok 'LAB_WORKFLOW.md uses the shared trusted review and handoff contract'
else
  not_ok 'LAB_WORKFLOW.md must use the shared trusted review and handoff contract'
fi

# One dispatch profile: the retired host-only product profile stays retired.
for gone in run.sh WORKFLOW.md unblock.sh peek.sh; do
  if [ -e "$root/sortie/$gone" ]; then
    not_ok "sortie/$gone belongs to the retired product profile"
  else
    ok "sortie/$gone is retired"
  fi
done
stale=$(grep -rlE --exclude=test-harness-docs.sh \
  'sortie/(run|unblock|peek)\.sh|sortie/WORKFLOW\.md|--profile (product|lab)|product profile' \
  "$root/sortie" "$root/docs" "$root/AGENTS.md" "$root/CLAUDE.md" "$root/README.md" || true)
if [ -n "$stale" ]; then
  not_ok "the retired product profile is still referenced: $(printf '%s ' $stale)"
else
  ok 'nothing references the retired product profile'
fi

if [ "$failures" -ne 0 ]; then
  printf '%s harness-docs test(s) failed\n' "$failures" >&2
  exit 1
fi
