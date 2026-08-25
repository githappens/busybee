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
# shellcheck disable=SC2016
expect_deny 'bzbd without PUEUE_CONFIG_PATH' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'BUSYBEE_STATE_DIR=$CLAUDE_PROJECT_DIR/build/test/state ./build/debug/bzbd'
# shellcheck disable=SC2016
expect_deny 'quoted unisolated bzbd' 'workspace-local BUSYBEE_STATE_DIR' Bash \
  '"$CLAUDE_PROJECT_DIR/build/debug/bzbd" --foreground'
expect_deny 'exec unisolated pueued' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'exec pueued --daemonize'
expect_deny 'command -p unisolated pueued' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'command -p pueued --daemonize'
expect_deny 'cargo run -p bzbd unisolated' 'workspace-local BUSYBEE_STATE_DIR' Bash \
  'cargo run -p bzbd -- --foreground'
expect_deny 'cargo run -p pueued unisolated' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'cargo run -p pueued'
expect_deny 'cargo r -p bzbd unisolated' 'workspace-local BUSYBEE_STATE_DIR' Bash \
  'cargo r -p bzbd -- --foreground'
expect_deny 'cargo --locked run -p bzbd unisolated' 'workspace-local BUSYBEE_STATE_DIR' Bash \
  'cargo --locked run -p bzbd -- --foreground'
expect_deny 'cargo --locked r -p pueued unisolated' 'workspace-local PUEUE_CONFIG_PATH' Bash \
  'cargo --locked r -p pueued -- --daemonize'
expect_deny 'git clean -fdx' 'do not modify the machine-safety hook' Bash \
  'git clean -fdx'
expect_deny 'git clean -xfd' 'do not modify the machine-safety hook' Bash \
  'git clean -xfd'
expect_deny 'git clean separated -x' 'do not modify the machine-safety hook' Bash \
  'git clean -f -d -x'
expect_deny 'rm -rf .claude' 'do not modify the machine-safety hook' Bash \
  'rm -rf .claude'
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
# shellcheck disable=SC2016
expect_deny 'last duplicate assignment is the effective path' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=build/test PUEUE_CONFIG_PATH="$HOME/.config/pueue" pueued --daemonize'
# shellcheck disable=SC2016
expect_deny 'env --chdir then relative config path' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  'env --chdir="$HOME" PUEUE_CONFIG_PATH=.config/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_deny 'env -C then relative config path' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  'env -C "$HOME" PUEUE_CONFIG_PATH=.config/pueue pueued --daemonize'
# Nested env -C is not composed: a second cwd wrapper makes relative paths unsafe.
# shellcheck disable=SC2016
expect_deny 'nested env -C then relative config path' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  'env -C "$HOME" env -C . PUEUE_CONFIG_PATH=.config/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_deny 'nested env -C then relative bzbd paths' \
  'workspace-local BUSYBEE_STATE_DIR' Bash \
  'env -C "$HOME" env -C . BUSYBEE_STATE_DIR=build/state PUEUE_CONFIG_PATH=build/pueue bzbd'
expect_deny 'absolute env path unisolated pueued' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  '/usr/bin/env pueued --daemonize'
# shellcheck disable=SC2016
expect_deny 'unresolved XDG_CONFIG_HOME in state path' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  'PUEUE_CONFIG_PATH=$XDG_CONFIG_HOME/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_deny 'cd then relative config path' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  'cd "$HOME" && PUEUE_CONFIG_PATH=.config/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_deny 'cd then PWD-prefixed config path' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  'cd "$HOME" && PUEUE_CONFIG_PATH=$PWD/build/test/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_deny 'grouped cd then relative config path' \
  'workspace-local PUEUE_CONFIG_PATH' Bash \
  '(cd "$HOME" && PUEUE_CONFIG_PATH=.config/pueue pueued --daemonize)'
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
# shellcheck disable=SC2016
expect_allow 'cargo run -p bzbd isolated' Bash \
  'BUSYBEE_STATE_DIR=$PWD/build/test/state PUEUE_CONFIG_PATH=$PWD/build/test/pueue cargo run -p bzbd -- --foreground'
# shellcheck disable=SC2016
expect_allow 'cargo r -p bzbd isolated' Bash \
  'BUSYBEE_STATE_DIR=$PWD/build/test/state PUEUE_CONFIG_PATH=$PWD/build/test/pueue cargo r -p bzbd -- --foreground'
# shellcheck disable=SC2016
expect_allow 'cargo --locked run -p bzbd isolated' Bash \
  'BUSYBEE_STATE_DIR=$PWD/build/test/state PUEUE_CONFIG_PATH=$PWD/build/test/pueue cargo --locked run -p bzbd -- --foreground'
expect_allow 'cargo --locked r -p pueued isolated' Bash \
  'PUEUE_CONFIG_PATH=build/test/pueue cargo --locked r -p pueued -- --daemonize'
expect_allow 'git clean -fd' Bash 'git clean -fd'
expect_allow 'isolated pueued' Bash \
  'PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_allow 'isolated bzbd' Bash \
  'BUSYBEE_STATE_DIR=$PWD/build/test/state PUEUE_CONFIG_PATH=$PWD/build/test/pueue ./build/debug/bzbd'
# shellcheck disable=SC2016
expect_allow 'quoted isolated bzbd' Bash \
  'BUSYBEE_STATE_DIR=$CLAUDE_PROJECT_DIR/build/test/state PUEUE_CONFIG_PATH=$CLAUDE_PROJECT_DIR/build/test/pueue "$CLAUDE_PROJECT_DIR/build/debug/bzbd" --foreground'
expect_allow 'isolated pueued under nix develop -c' Bash \
  'nix develop -c env PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'env -- with isolated pueued' Bash \
  'env -- PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'exec env isolated pueued' Bash \
  'exec env PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'command -p env isolated pueued' Bash \
  'command -p env PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'env -i with isolated pueued' Bash \
  'env -i PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_allow 'env -u then reassign isolated pueued' Bash \
  'PUEUE_CONFIG_PATH=build/test env -u PUEUE_CONFIG_PATH PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_allow 'last duplicate assignment local' Bash \
  'PUEUE_CONFIG_PATH="$HOME/.config/pueue" PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_allow 'env --chdir with PWD-prefixed config' Bash \
  'env --chdir="$HOME" PUEUE_CONFIG_PATH=$PWD/build/test/pueue pueued --daemonize'
expect_allow 'absolute env with isolated pueued' Bash \
  '/usr/bin/env PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_allow 'cd then CLAUDE_PROJECT_DIR config path' Bash \
  'cd "$HOME" && PUEUE_CONFIG_PATH=$CLAUDE_PROJECT_DIR/build/test/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_allow 'grouped cd then CLAUDE_PROJECT_DIR config path' Bash \
  '(cd "$HOME" && PUEUE_CONFIG_PATH=$CLAUDE_PROJECT_DIR/build/test/pueue pueued --daemonize)'
# shellcheck disable=SC2016
expect_allow 'nested env -C then CLAUDE_PROJECT_DIR config path' Bash \
  'env -C "$HOME" env -C . PUEUE_CONFIG_PATH=$CLAUDE_PROJECT_DIR/build/pueue pueued --daemonize'
# shellcheck disable=SC2016
expect_allow 'nested env -C then CLAUDE_PROJECT_DIR bzbd paths' Bash \
  'env -C "$HOME" env -C . BUSYBEE_STATE_DIR=$CLAUDE_PROJECT_DIR/build/state PUEUE_CONFIG_PATH=$CLAUDE_PROJECT_DIR/build/pueue bzbd'
expect_allow 'read cargo config' Bash 'cat .cargo/config.toml'
expect_allow 'read runtime hook' Bash 'cat .claude/hooks/machine-safety-hook.sh'
expect_allow 'edit ordinary source' Write "$root/crates/bzb/src/main.rs"

# Exercise WORKFLOW.md's before_run install: payload from origin/main, even
# when the checked-out issue branch does not contain those files.
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
git -C "$scratch" init -q
git -C "$scratch" checkout -q -b main
mkdir -p "$scratch/sortie"
install -m 0755 "$hook" "$scratch/sortie/machine-safety-hook.sh"
install -m 0644 "$settings" "$scratch/sortie/claude-settings.json"
git -C "$scratch" add sortie
git -C "$scratch" -c user.email=test@example.com -c user.name=test \
  commit -qm 'payload on main'
git -C "$scratch" checkout -q -b old-issue
git -C "$scratch" rm -rq sortie
git -C "$scratch" -c user.email=test@example.com -c user.name=test \
  commit -qm 'issue branch without payload'
git -C "$scratch" update-ref refs/remotes/origin/main main
grep -qxF '/.claude/' "$scratch/.git/info/exclude" \
  || printf '/.claude/\n' >> "$scratch/.git/info/exclude"
mkdir -p "$scratch/.claude/hooks"
hook_ref=origin/main
if ! git -C "$scratch" cat-file -e "$hook_ref:sortie/machine-safety-hook.sh" 2>/dev/null; then
  hook_ref=HEAD
fi
git -C "$scratch" show "$hook_ref:sortie/machine-safety-hook.sh" \
  > "$scratch/.claude/hooks/machine-safety-hook.sh"
chmod 0755 "$scratch/.claude/hooks/machine-safety-hook.sh"
git -C "$scratch" show "$hook_ref:sortie/claude-settings.json" \
  > "$scratch/.claude/settings.json"
chmod 0644 "$scratch/.claude/settings.json"
test -x "$scratch/.claude/hooks/machine-safety-hook.sh"
test "$hook_ref" = origin/main
jq -e '.hooks.PreToolUse[] | select(.matcher | contains("Bash"))' \
  "$scratch/.claude/settings.json" >/dev/null
test -z "$(git -C "$scratch" status --porcelain)"
test ! -e "$scratch/sortie/machine-safety-hook.sh"
printf 'ok - before_run hook payload from origin/main\n'

if [ "$failures" -ne 0 ]; then
  printf '%s machine-safety test(s) failed\n' "$failures" >&2
  exit 1
fi
