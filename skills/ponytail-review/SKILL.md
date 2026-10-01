---
name: ponytail-review
description: >
  Review a PR diff for unnecessary complexity: dead code, reinvented standard
  library, redundant dependencies, speculative abstractions, and unused
  flexibility. Propose bounded deletions while preserving the issue contract
  and tests. Read-only.
---

# Ponytail review

Find what can be deleted from the PR while preserving its contract. Read the
issue, `AGENTS.md`, relevant specification, and the PR's actual base-to-head
diff. Record the PR number and base/head SHAs. Use the same review packet as
contract-review; do not depend on another runner's slash-command syntax.

Return findings to the implementing agent. Do not apply fixes, push, post
GitHub comments or reviews, change issue state, or merge. A requested local
report is the only write this review needs.

## Findings

Use one line per finding:

`path/to/file:L12: tag: what to cut. What replaces it and why the contract holds.`

Tags:

- `delete`: dead code, unused flexibility, speculative feature. Replacement:
  nothing.
- `stdlib`: handwritten behavior the standard library already provides. Name
  the replacement and check that its semantics fit the required inputs.
- `native`: a dependency or layer duplicates an available platform feature.
- `yagni`: an abstraction or configuration surface has no required use.
- `shrink`: the same behavior can be expressed more directly. Show the form.

Anchor findings to the changed code. Do not infer that an adapter is redundant
merely because it currently has one implementation: a named failure-injection
test or an explicit controller boundary may require it. Do not suggest dropping
tests, error reporting, isolation, timeouts, durable evidence, or resource
accounting that the issue or specification requires. A smoke test is useful
verification, not clutter.

Examples:

```text
parse.py:L42: stdlib: hand-written JSON decoder. json.loads preserves the required JSON input contract.
worker.py:L80: delete: unused retry-policy registry. Nothing; this issue requires one fixed bounded retry policy.
```

Correctness/security/performance bugs belong in contract-review when they meet
its criteria; this skill does not relabel them as complexity. Exclude adjacent
pre-existing code and preferences that do not yield a concrete scoped
simplification. Inspect a proposed simplification against the required tests
before recommending it.

## Convergence and report

Report `BLOCKED` when concrete in-scope cuts remain, `READY` when the change is
already lean enough, or `UNSURE` when required packet evidence is missing.
End with `net: -N lines possible.` for an estimate of justified savings, or
`Lean already. Ship.` when nothing needs cutting. Line count is descriptive;
fewer lines never justify weakening the contract.

The parent agent applies valid scoped fixes and records the reason for any
declined finding. In a follow-up, settle those findings and inspect the new
delta; do not repeat unchanged suggestions or start another broad pass.

When invoked with a structured output schema, return `head`, `verdict`,
`findings`, and `report` as defined in `docs/development/agent-review.md`.
The CI controller supplies skill digests and actual session IDs; do not invent
them. A local invocation can return the same report for early feedback. A
clean contract review does not substitute for this review, or vice versa.
