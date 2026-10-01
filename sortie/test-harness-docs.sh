#!/usr/bin/env bash
# Documented loop limits come from config, ground rules point at their source,
# and both dispatch profiles use the shared CI review/handoff contract.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
workflow="$root/sortie/WORKFLOW.md"
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

rules=$(awk '/^## Ground rules$/,/^## Prohibitions$/' "$workflow")
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

for profile in WORKFLOW.md LAB_WORKFLOW.md; do
  if grep -q 'docs/development/agent-review.md' "$root/sortie/$profile" &&
     grep -q '\$BUSYBEE_SORTIE_TRUSTED/sortie/reviews.py handoff' "$root/sortie/$profile" &&
     grep -q '\$BUSYBEE_SORTIE_TRUSTED/sortie/review-triage.py' "$root/sortie/$profile"; then
    ok "$profile uses the shared trusted review and handoff contract"
  else
    not_ok "$profile must use the shared trusted review and handoff contract"
  fi
done

if [ "$failures" -ne 0 ]; then
  printf '%s harness-docs test(s) failed\n' "$failures" >&2
  exit 1
fi
