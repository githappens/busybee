#!/usr/bin/env bash
# Invariants for #55: documented loop numbers match the config, duplicated
# ground rules stay one line with a pointer, and the reply-contract copies
# name each other.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
workflow="$root/sortie/WORKFLOW.md"
readme="$root/sortie/README.md"
gate="$root/.github/workflows/codex-gate.yml"
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

if awk '
  /^[0-9]+\. \*\*Use these skills/ {p=1; next}
  p && /^[0-9]+\. / {exit}
  p && /whole scope/ {found=1}
  END {exit found ? 0 : 1}
' "$workflow"; then
  not_ok 'skills rule must not restate that the acceptance criteria are the whole scope'
else
  ok 'skills rule does not restate scope'
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

twin='Reply-contract twin:'
if grep -q "$twin" "$workflow" && grep -q 'codex-gate.yml' "$workflow"; then
  ok 'WORKFLOW.md reply contract names its twin'
else
  not_ok 'WORKFLOW.md reply contract must name .github/workflows/codex-gate.yml'
fi
if grep -q "$twin" "$gate" && grep -q 'WORKFLOW.md' "$gate"; then
  ok 'codex-gate.yml reply contract names its twin'
else
  not_ok 'codex-gate.yml reply contract must name sortie/WORKFLOW.md'
fi

if [ "$failures" -ne 0 ]; then
  printf '%s harness-docs test(s) failed\n' "$failures" >&2
  exit 1
fi
