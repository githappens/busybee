---
# Launch with run-lab.sh. It selects a native adapter, pins the executing
# scripts outside issue workspaces, and uses the repo-owned agent dev shell.
tracker:
  kind: github
  project: githappens/busybee
  api_key: $GITHUB_TOKEN
  query_filter: 'label:sortie milestone:"agent lab: autonomous VM development"'
  active_states: [sortie:ready, sortie:working]
  in_progress_state: sortie:working
  handoff_state: sortie:review
  terminal_states: [sortie:done]
  handoff_evidence: strict

workspace:
  root: $BUSYBEE_SORTIE_STATE/workspaces
db_path: $BUSYBEE_SORTIE_STATE/sortie.db

polling:
  interval_ms: 60000

hooks:
  after_create: |
    git -c credential.helper= -c credential.helper='!gh auth git-credential' clone "$BUSYBEE_SORTIE_CLONE_URL" .
  # Each attempt runs in a fresh Linux worker from the trusted controller:
  # before_run closes an unclosed attempt and opens one; after_run checkpoints,
  # collects, destroys and accounts for the worker. Agent turns run there via
  # the launcher's `session agent` command.
  before_run: |
    set -e
    bash "$BUSYBEE_SORTIE_TRUSTED/sortie/prepare-workspace.sh"
    profile=$(python3 "$BUSYBEE_SORTIE_TRUSTED/sortie/lab.py" profile --issue "$SORTIE_ISSUE_IDENTIFIER")
    python3 "$BUSYBEE_SORTIE_TRUSTED/scripts/vm/vmctl.py" --root "$BUSYBEE_LAB_ROOT" session start \
      --issue "$SORTIE_ISSUE_IDENTIFIER" --workspace "$SORTIE_WORKSPACE" --profile "$profile"
  after_run: |
    python3 "$BUSYBEE_SORTIE_TRUSTED/scripts/vm/vmctl.py" --root "$BUSYBEE_LAB_ROOT" session end \
      --workspace "$SORTIE_WORKSPACE"
  timeout_ms: 900000

agent:
  # launch.sh selects the adapter through Sortie's supported SORTIE_AGENT_*
  # overrides. These fields do not expand arbitrary environment variables.
  kind: codex
  command: codex app-server
  max_concurrent_agents: 1
  max_turns: 20
  max_sessions: 20
  turn_timeout_ms: 7200000
  read_timeout_ms: 30000
  # A turn that asks for a handoff ends with the evidence gate verifying its
  # head and base on Linux and macOS, silently, before the turn returns.
  stall_timeout_ms: 3600000
  max_retry_backoff_ms: 300000

claude-code:
  # Sortie 1.24 requires this mode; every lab turn runs in an allocated worker.
  permission_mode: bypassPermissions
  allowed_tools: "Bash Edit MultiEdit Write Read Glob Grep Agent TodoWrite"
  disallowed_tools: "mcp__sortie-tools__tracker_api"
  session_persistence: true

codex:
  approval_policy: never
  thread_sandbox: workspaceWrite
  turn_sandbox_policy:
    type: workspaceWrite
    networkAccess: true

reactions:
  bot_review:
    provider: github
    bot_usernames: ["github-actions", "github-actions[bot]"]
    poll_interval_ms: 60000
    debounce_ms: 60000
    max_continuation_turns: 6
    watch_window_ms: 21600000
    escalation: label
    escalation_label: needs-human
    triage:
      script: |
        python3 "$BUSYBEE_SORTIE_TRUSTED/sortie/review-triage.py"
      timeout_ms: 180000
  review_comments:
    provider: github
    max_retries: 2
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 120000
    debounce_ms: 60000
    max_continuation_turns: 6
    watch_window_ms: 21600000
  ci_failure:
    provider: github
    max_retries: 3
    max_log_lines: 80
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 60000
    watch_window_ms: 21600000
  merge_conflicts:
    provider: github
    max_retries: 2
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 60000
  auto_merge:
    provider: github
    strategy: squash
    require_ci: true
    delete_branch: true
    poll_interval_ms: 60000
    watch_window_ms: 21600000
    max_retries: 2
    escalation: label
    escalation_label: needs-human
  merge_completion:
    provider: github
    target_state: sortie:done
    poll_interval_ms: 60000
    max_retries: 2
    escalation: label
    escalation_label: needs-human
---

Implement one busybee issue and deliver its PR through the repository's review
and verification loop. You may be running in any supported agent runtime.

Issue #{{ .issue.identifier }}: {{ .issue.title }}
{{ .issue.description }}
Issue URL: {{ .issue.url }}

Read `AGENTS.md`, `CLAUDE.md`, the issue's specification sections, and the
trusted `docs/development/agent-review.md` under `$BUSYBEE_SORTIE_TRUSTED`.
The executing policy is outside this workspace. Edits to a candidate policy
are reviewed code; they do not replace the policy supervising this task.

You work inside your own disposable Linux VM worker, as its administrator:
install tools, restart or break daemons, signal processes, and reset it as
your investigation needs, without asking. Your checkout is on the existing
`sortie-lab/{{ .issue.identifier }}` branch; it is checkpointed to the
dispatcher after every turn, so work survives a reset or a replaced worker.
Do not reset the branch to main or create a duplicate PR. `$BUSYBEE_LAB_PR`
names the PR when one exists. Read the merged prerequisite code. Respect the
issue's scope and named tests. Infrastructure issues may change only the
controller/runner files their scope requires; product fixes may not change
orchestration or approval policy, and the session guard refuses to adopt or
push such changes.

`lab` (on your PATH) is the controller for your worker only: `lab terminal
open|send|resize|capture` for a real terminal with decoded screens, `lab
scenario`, `lab exec`, `lab inspect`, `lab console`, `lab collect`, `lab
checkpoint`, and `lab reset`, which applies when your turn ends (end the turn
after asking for it). The worker holds no GitHub credentials: `lab fetch`
updates origin/main, `lab push [--force-with-lease]` pushes your branch, and
`lab pr status|create|ready|comment|view`, `lab handoff` and `lab verification`
act on its PR.

Host configuration and global installations are outside scope. All project Nix,
provisioning, and tests belong in the repo. Product tests use private
Pueue/bzbd state even in your worker, and the startup path under test must not
be prepared by hand: do not work around a cold startup bug by creating its
runtime directories.

When implementation and required checks are ready:

1. Commit, `lab push`, and create/reuse a draft PR with `Closes #{{ .issue.identifier }}`
   through `lab pr create --title TITLE --body-file FILE`.
2. Mark the implementation ready with `lab pr ready`. CI runs both trusted
   review skills in separate Claude Opus 5.5 high sessions after Linux/macOS
   checks pass. Local skill reviews are optional early feedback; do not publish
   author review receipts or claim they can satisfy the gate.
3. `lab handoff` asks for review of the pushed head; end the turn right after
   it. Your report of success is not evidence: when the turn ends, the
   controller releases your worker and verifies the head and its merge base
   with main in fresh Linux and macOS workers (required checks, the issue's
   regression scenarios red on the base and green on the head, terminal
   evidence, cleanup). A product fix must show its regression: a scenario
   naming the issue, or the regression test files you added, named with
   `lab handoff --overlay PATH...` so the base's red run includes them. Only
   an accepted head is handed to review, with the evidence posted on the PR;
   then the trusted `$BUSYBEE_SORTIE_TRUSTED/sortie/reviews.py handoff`
   writes `.sortie/scm.json` and `.sortie/status`. Otherwise your next turn starts in a fresh worker: read
   `lab verification`, fix what it names, push, and hand off again. Do not
   post evidence yourself; the controller refuses text carrying its marker.
   `needs-human-review` is Sortie's protocol name: CI supplies the formal
   review and Sortie handles the resulting continuation or merge.
4. On a findings continuation, fix valid scoped findings, rerun affected
   checks, and `lab push` to the same PR. Explain declined findings with
   `lab pr comment` and concrete evidence; CI re-reviews that new disposition
   even without a code change. Repeat the handoff. CI settles prior findings and reviews the
   new delta; both final reports and required CI must cover the current head.

Do not change issue labels/state or merge the PR yourself. Do not request the
external Codex review bot: these two skills provide this workflow's review.
If a prerequisite or tool genuinely blocks progress, retain source and reports,
write a specific reason to `.sortie/blocker.md`, set `.sortie/status` to
`blocked`, and stop. Do not retry unchanged evidence or ask someone to operate
the environment as a routine step.

{{ if .run.is_continuation }}
This is a continuation. Inspect branch/PR state and prior reports first. Reuse
completed work. Rerun checks when code changed or a reported failure requires
them; an unchanged settled review does not require a new test run or push.
{{ end }}
{{ if .review_comments }}
Review feedback:
{{ range .review_comments }}- {{ .reviewer }}: {{ .body }}
{{ end }}
Address the scoped feedback, then repeat affected checks and the current-head
review/handoff process. Explain declined findings with concrete reasons.
{{ end }}
{{ if .bot_review_comments }}
CI skill review findings:
{{ range .bot_review_comments }}- {{ .reviewer }}: {{ .body }}
{{ end }}
Read the latest formal CI review on this PR (`lab pr view`). Fix its scoped findings or explain
declined findings with concrete evidence in a PR comment. Retain settled
decisions. Push changes and hand off again; CI owns the follow-up reviews.
Do not run an author receipt loop, reply to a clean approval, or make an empty
commit to trigger review. Authentication/tool failures escalate through triage.
{{ end }}
{{ if .ci_failure }}
CI failed on {{ .ci_failure.ref }}:
{{ .ci_failure.log_excerpt }}
Reproduce and fix the failure; never weaken a test to pass. Push to the same PR
and hand off its new head for CI review.
{{ end }}
{{ if .merge_conflict }}
PR #{{ .merge_conflict.pr_number }} conflicts with {{ .merge_conflict.base }}.
`lab fetch` and rebase only to resolve the actual conflict, preserve scope, run
the required checks, `lab push --force-with-lease`, and review the new head.
{{ end }}
