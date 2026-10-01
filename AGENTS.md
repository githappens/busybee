# busybee — agent notes

Build, test and layout conventions live in `CLAUDE.md`. The design specification
for the broker work is `docs/design/bzbd.md`; task issues carry scope and
acceptance criteria, the specification carries semantics.

## Agent review workflow

Repository skills live in `skills/contract-review/SKILL.md` and
`skills/ponytail-review/SKILL.md`. Use these exact files for PR review, regardless
of the agent runner or identically named personal skills. For ready PRs from
this repository, CI runs
both skills in separate Claude Opus 5.5 high sessions and publishes a formal
review. The implementing agent fixes valid scoped findings, tests, pushes, and
hands the same PR back to Sortie. Local skill reviews are optional feedback.
Reviewers remain read-only; author comments do not grant approval.
The CI evidence and Sortie handoff are defined in
[`docs/development/agent-review.md`](docs/development/agent-review.md).

For VM infrastructure, also read `docs/design/agent-lab.md`. Infrastructure
issues may change the controller and runner files explicitly in their scope;
an ordinary product task must not change dispatch or merge policy. The running
controller and gate use a trusted revision outside the candidate branch.

## Code Review Rules

### Conform to the specification
Compare the change against `docs/design/bzbd.md` and the linked issue's
acceptance criteria. Flag code that contradicts a rule, table, placeholder, or
contract in the spec, and flag behaviour changes that do not update the spec in
the same pull request. A deviation that is argued in the PR body is still a
finding if the spec was not updated.

### No silent fallbacks
Flag any path that swallows an error, substitutes a default, or degrades to a
weaker method without surfacing it (a returned `Err`, a logged warning at warn
level or above, or a `notices` entry). "Unknown → exclusive" is the documented
safe default for classification; anything else that quietly does less than asked
is a defect.

### Resource accounting must stay honest
Every token taken from the jobserver fifo must be returned on every exit path
(normal exit, kill, client disconnect, daemon restart). Flag leases that can start
without an `Admit`, tokens that can be held by a lease the scheduler no longer
tracks, and any place where the pool could be over-subscribed (jobserver-class
tasks running alongside a static grant that was never drained).

### Tests are the contract
Flag tests that were weakened, skipped, or removed to make a change pass, and new
behaviour without a test named in the issue. Integration tests must spawn their
own `pueued`/`bzbd` in a temporary state directory, never the user's instance.

### Stay within the issue's scope
A finding must concern the change under review: the issue's acceptance criteria,
the spec sections it touches, and the rules above. Do not flag hardening,
robustness, or style improvements to code the pull request did not need to
touch, and do not open a new line of findings on code that exists only to
satisfy an earlier finding unless that code is actually wrong. Mention
out-of-scope improvements in the review summary as follow-up suggestions, not as
findings; the author files them as issues. A pull request that keeps growing to
absorb adjacent improvements never converges.

### Public repository hygiene
Flag machine-, user-, or workspace-specific details in code, comments, fixtures,
docs, or commit messages: home-directory paths, host names, other project names.
Examples must be generic.
