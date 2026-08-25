#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
hook="$root/sortie/machine-safety-hook.sh"
settings="$root/sortie/claude-settings.json"
failures=0

call_hook() {
  local tool=$1 value=$2
  jq -nc --arg tool "$tool" --arg value "$value" '
    {tool_name: $tool,
     tool_input: (if $tool == "Bash" then {command: $value} else {file_path: $value} end)}
  ' | CLAUDE_PROJECT_DIR="$root" "$hook"
}

expect_deny() {
  local name=$1 reason=$2 tool=$3 value=$4 output
  output=$(call_hook "$tool" "$value")
  if ! jq -e --arg reason "$reason" '
    .hookSpecificOutput.permissionDecision == "deny"
    and (.hookSpecificOutput.permissionDecisionReason | contains($reason))
  ' >/dev/null <<<"$output"; then
    printf 'not ok - %s\n%s\n' "$name" "$output" >&2
    failures=$((failures + 1))
  else
    printf 'ok - %s\n' "$name"
  fi
}

expect_allow() {
  local name=$1 tool=$2 value=$3 output
  output=$(call_hook "$tool" "$value")
  if [ -n "$output" ]; then
    printf 'not ok - %s\n%s\n' "$name" "$output" >&2
    failures=$((failures + 1))
  else
    printf 'ok - %s\n' "$name"
  fi
}

expect_deny 'nix profile install' 'nix profile install' Bash \
  'nix profile install .'
expect_deny 'cargo install with toolchain' 'cargo install' Bash \
  'cargo +stable install bzb'
expect_deny 'repository deploy script' 'global busybee/bzbd/pueued' Bash \
  './scripts/buildanddeploy.sh'
expect_deny 'direct global binary write' 'global busybee/bzbd/pueued' Bash \
  "cp build/release/bzbd '$HOME/.local/bin/bzbd'"
# These are literal proposed commands passed to the hook, not commands for this
# test shell to expand.
# shellcheck disable=SC2016
expect_deny 'resolved global binary write' 'global busybee/bzbd/pueued' Bash \
  'install build/release/pueued "$(command -v pueued)"'
expect_deny 'unisolated pueued' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'pueued --daemonize'
expect_deny 'external pueued config' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=/tmp/pueue pueued --daemonize'
expect_deny 'similarly named pueued variable' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'NOT_PUEUE_CONFIG_PATH=build/test pueued --daemonize'
expect_deny 'unisolated bzbd' 'workspace-local BUSYBEE_STATE_DIR' Bash \
  './build/debug/bzbd'
expect_deny 'Edit of cargo config' 'do not edit .cargo/config.toml' Edit \
  "$root/.cargo/config.toml"
expect_deny 'Bash write to cargo config' 'do not edit .cargo/config.toml' Bash \
  'printf "x" > .cargo/config.toml'

# Codex findings: wrappers, wrong-segment assignments, interpreter writes.
expect_deny 'nix develop -c unisolated pueued' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'nix develop -c pueued --daemonize'
expect_deny 'assignment on earlier segment does not isolate pueued' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=build/test true; pueued --daemonize'
expect_deny 'interpreter write to cargo config' 'do not edit .cargo/config.toml' Bash \
  'python -c '"'"'open(".cargo/config.toml", "w").write("x")'"'"''

# env options must be consumed so argv0 is the real command, not -i / -u.
expect_deny 'env -i unisolated pueued' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'env -i pueued --daemonize'
expect_deny 'env -u clears isolation' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'env -u PUEUE_CONFIG_PATH pueued --daemonize'
# Leading assignments must not survive env -u / -i / --unset / --ignore-environment.
expect_deny 'assignment then env -u PUEUE_CONFIG_PATH' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=build/test env -u PUEUE_CONFIG_PATH pueued --daemonize'
expect_deny 'assignment then env -i' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=build/test env -i pueued --daemonize'
expect_deny 'assignment then env --unset=' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=build/test env --unset=PUEUE_CONFIG_PATH pueued --daemonize'
expect_deny 'assignment then env --ignore-environment' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=build/test env --ignore-environment pueued --daemonize'
# shellcheck disable=SC2016
expect_deny 'PWD-prefixed path traversal' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=$PWD/../../user-config pueued --daemonize'
expect_deny 'project-prefixed path traversal' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  "PUEUE_CONFIG_PATH=$root/../user-config pueued --daemonize"
expect_deny 'Write of runtime hook' 'do not modify the machine-safety hook' Write \
  "$root/.claude/hooks/machine-safety-hook.sh"
expect_deny 'Edit of runtime settings' 'do not modify the machine-safety hook' Edit \
  "$root/.claude/settings.json"
expect_deny 'Bash overwrite of runtime hook' 'do not modify the machine-safety hook' Bash \
  'printf "exit 0\n" > .claude/hooks/machine-safety-hook.sh'

expect_allow 'workspace test command' Bash 'busybee -- cargo test --workspace'
expect_allow 'clippy command' Bash 'cargo clippy --workspace --all-targets -- -D warnings'
expect_allow 'format command' Bash 'cargo fmt --all'
expect_allow 'test-fixture name as an argument' Bash \
  'cargo test -p bzb-test-support pueued'
expect_allow 'isolated pueued' Bash \
  'PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_allow 'isolated bzbd' Bash \
  'BUSYBEE_STATE_DIR=$PWD/build/test/state ./build/debug/bzbd'
expect_allow 'isolated pueued under nix develop -c' Bash \
  'nix develop -c env PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'env -- with isolated pueued' Bash \
  'env -- PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'env -i with isolated pueued' Bash \
  'env -i PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'env -u then reassign isolated pueued' Bash \
  'PUEUE_CONFIG_PATH=build/test env -u PUEUE_CONFIG_PATH PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'read cargo config' Bash 'cat .cargo/config.toml'
expect_allow 'read runtime hook' Bash 'cat .claude/hooks/machine-safety-hook.sh'
expect_allow 'edit ordinary source' Write "$root/crates/bzb/src/main.rs"

# Exercise the same two copies made by WORKFLOW.md's before_run hook.
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
git -C "$scratch" init -q
grep -qxF '/.claude/' "$scratch/.git/info/exclude" \
  || printf '/.claude/\n' >> "$scratch/.git/info/exclude"
mkdir -p "$scratch/.claude/hooks"
install -m 0755 "$hook" "$scratch/.claude/hooks/machine-safety-hook.sh"
install -m 0644 "$settings" "$scratch/.claude/settings.json"
test -x "$scratch/.claude/hooks/machine-safety-hook.sh"
jq -e '.hooks.PreToolUse[] | select(.matcher | contains("Bash"))' \
  "$scratch/.claude/settings.json" >/dev/null
test -z "$(git -C "$scratch" status --porcelain)"
printf 'ok - before_run hook payload\n'

if [ "$failures" -ne 0 ]; then
  printf '%s machine-safety test(s) failed\n' "$failures" >&2
  exit 1
fi
