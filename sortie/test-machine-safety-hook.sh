#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
hook="$root/sortie/machine-safety-hook.sh"
launcher="$root/sortie/isolated.sh"
settings="$root/sortie/claude-settings.json"
install_script="$root/sortie/install-machine-safety.sh"
failures=0

chmod +x "$hook" "$launcher" "$install_script"

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

# --- supported isolated launches ---
expect_allow 'isolated bzb xcodebuild' Bash \
  '.claude/isolated.sh bzb -- xcodebuild -project App.xcodeproj'
expect_allow 'isolated bzb cmake' Bash \
  '.claude/isolated.sh bzb -- cmake --build build'
expect_allow 'isolated busybee cargo test' Bash \
  '.claude/isolated.sh busybee -- cargo test --workspace'
expect_allow 'isolated pueued' Bash \
  '.claude/isolated.sh pueued --daemonize'
expect_allow 'isolated bzbd' Bash \
  '.claude/isolated.sh bzbd --foreground'
expect_allow 'isolated pueue client' Bash \
  '.claude/isolated.sh pueue clean'
expect_allow 'quoted project-dir isolated bzb' Bash \
  '"$CLAUDE_PROJECT_DIR/.claude/isolated.sh" bzb -- xcodebuild -project App.xcodeproj'
expect_allow 'nix develop -c isolated busybee' Bash \
  'nix develop -c .claude/isolated.sh busybee -- cargo test --workspace'
expect_allow 'sortie-tree isolated.sh' Bash \
  'sortie/isolated.sh bzb -- cmake --build build'

# --- unisolated stateful launches denied ---
expect_deny 'bare bzb xcodebuild' 'isolated.sh' Bash \
  'bzb -- xcodebuild -project App.xcodeproj'
expect_deny 'bare busybee cargo test' 'isolated.sh' Bash \
  'busybee -- cargo test --workspace'
expect_deny 'bare pueued' 'isolated.sh' Bash \
  'pueued --daemonize'
expect_deny 'bare bzbd' 'isolated.sh' Bash \
  './build/debug/bzbd'
expect_deny 'bare pueue client' 'isolated.sh' Bash \
  'pueue clean'
expect_deny 'env-prefixed pueued is not the launcher' 'isolated.sh' Bash \
  'PUEUE_CONFIG_PATH=build/test/pueue pueued --daemonize'
expect_deny 'env-prefixed bzb is not the launcher' 'isolated.sh' Bash \
  'BUSYBEE_STATE_DIR=build/state PUEUE_CONFIG_PATH=build/pueue bzb -- cmake --build build'

# --- normal build / test / lint / format ---
expect_allow 'cargo test' Bash 'cargo test --workspace'
expect_allow 'cargo clippy' Bash 'cargo clippy --workspace --all-targets -- -D warnings'
expect_allow 'cargo fmt' Bash 'cargo fmt --all'
expect_allow 'cargo check' Bash 'cargo check --workspace'
expect_allow 'cargo build' Bash 'cargo build --release'
expect_allow 'cd crate then cargo test' Bash 'cd crates/bzbd && cargo test'
expect_allow 'cargo test -p bzb' Bash 'cargo test -p bzb'
expect_allow 'read-only mention of pueued' Bash 'rg pueued crates/'

# --- global install / deploy ---
expect_deny 'nix profile install' 'nix profile install' Bash \
  'nix profile install .'
expect_deny 'nix profile add' 'nix profile install' Bash \
  'nix profile add --impure .'
expect_deny 'cargo install' 'cargo install' Bash \
  'cargo +stable install bzb'
expect_deny 'deploy script' 'global busybee/bzb/bzbd/pueued' Bash \
  './scripts/buildanddeploy.sh'
expect_deny 'copy to ~/.local/bin' 'global busybee/bzb/bzbd/pueued' Bash \
  "cp build/release/bzbd '$HOME/.local/bin/bzbd'"

# --- .cargo/config.toml ---
expect_deny 'Edit of cargo config' 'do not edit .cargo/config.toml' Edit \
  "$root/.cargo/config.toml"
expect_deny 'Bash write to cargo config' 'do not edit .cargo/config.toml' Bash \
  'printf "x" > .cargo/config.toml'
expect_deny 'python write to cargo config' 'do not edit .cargo/config.toml' Bash \
  'python -c '"'"'open(".cargo/config.toml", "w").write("x")'"'"''
expect_allow 'read cargo config' Bash 'cat .cargo/config.toml'
expect_allow 'edit ordinary source' Write "$root/crates/bzb/src/main.rs"

# --- runtime hook / settings / launcher ---
expect_deny 'rm -rf .claude' 'do not modify the machine-safety hook' Bash \
  'rm -rf .claude'
expect_deny 'rm project-prefixed .claude' 'do not modify the machine-safety hook' Bash \
  'rm -rf "$CLAUDE_PROJECT_DIR"/.claude'
expect_deny 'Write of runtime hook' 'do not modify the machine-safety hook' Write \
  "$root/.claude/hooks/machine-safety-hook.sh"
expect_deny 'Edit of runtime settings' 'do not modify the machine-safety hook' Edit \
  "$root/.claude/settings.json"
expect_deny 'Write of isolated launcher' 'do not modify the machine-safety hook' Write \
  "$root/.claude/isolated.sh"
expect_deny 'Bash overwrite of runtime hook' 'do not modify the machine-safety hook' Bash \
  'printf "exit 0\n" > .claude/hooks/machine-safety-hook.sh'
expect_deny 'runtime hook via ..' 'do not modify the machine-safety hook' Bash \
  "printf 'exit 0\n' > .claude/hooks/../hooks/machine-safety-hook.sh"
expect_deny 'git clean -fdx' 'do not modify the machine-safety hook' Bash \
  'git clean -fdx'
expect_deny 'git stash --all' 'do not modify the machine-safety hook' Bash \
  'git stash push --all'
expect_deny 'find -delete' 'do not modify the machine-safety hook' Bash \
  'find . -depth -delete'
expect_allow 'git clean -fd' Bash 'git clean -fd'
expect_allow 'read runtime hook' Bash 'cat .claude/hooks/machine-safety-hook.sh'

# --- cargo run of stateful binaries is not a supported form ---
expect_deny 'cargo run -p bzbd' 'isolated.sh' Bash \
  'cargo run -p bzbd -- --foreground'
expect_deny 'cargo run --bin busybee' 'isolated.sh' Bash \
  'cargo run --bin busybee -- -- cmake --build build'

# --- representative fail-closed ambiguity ---
expect_deny 'env wrapper around pueued' 'isolated.sh' Bash \
  'env PUEUE_CONFIG_PATH=build/test pueued --daemonize'
expect_deny 'cd then bare pueued' 'isolated.sh' Bash \
  'cd "$HOME" && pueued --daemonize'
expect_deny 'isolated then extra pueued' 'isolated.sh' Bash \
  '.claude/isolated.sh bzb -- true; pueued --daemonize'
expect_deny 'command substitution of pueued' 'isolated.sh' Bash \
  'echo $(pueued --daemonize)'
expect_deny 'cargo test then bare pueued' 'isolated.sh' Bash \
  'cargo test && pueued --daemonize'
expect_deny 'unknown isolated.sh first arg' 'isolated.sh' Bash \
  '.claude/isolated.sh bash -c pueued'

# --- missing jq fails closed ---
no_jq_bin=$(mktemp -d)
ln -s "$(command -v cat)" "$no_jq_bin/cat"
bash_bin=$(command -v bash)
no_jq_input=$(jq -nc '{tool_name:"Bash",tool_input:{command:"pueued --daemonize"}}')
no_jq_output=$(printf '%s\n' "$no_jq_input" \
  | PATH="$no_jq_bin" CLAUDE_PROJECT_DIR="$root" "$bash_bin" "$hook")
if jq -e '
  .hookSpecificOutput.permissionDecision == "deny"
  and (.hookSpecificOutput.permissionDecisionReason | contains("jq is required"))
' >/dev/null <<<"$no_jq_output"; then
  printf 'ok - hook denies when jq is missing\n'
else
  printf 'not ok - hook denies when jq is missing\n%s\n' "$no_jq_output" >&2
  failures=$((failures + 1))
fi
if PATH="$no_jq_bin" "$bash_bin" "$root/sortie/agent.sh" >/dev/null 2>"$no_jq_bin/err"; then
  printf 'not ok - agent.sh requires jq\n' >&2
  failures=$((failures + 1))
elif grep -q 'jq is required' "$no_jq_bin/err"; then
  printf 'ok - agent.sh requires jq\n'
else
  printf 'not ok - agent.sh requires jq\n%s\n' "$(cat "$no_jq_bin/err")" >&2
  failures=$((failures + 1))
fi
rm -rf "$no_jq_bin"
if grep -q 'pkgs.jq' "$root/flake.nix"; then
  printf 'ok - flake.nix provides jq\n'
else
  printf 'not ok - flake.nix provides jq\n' >&2
  failures=$((failures + 1))
fi
if grep -q 'command -v jq' "$root/sortie/run.sh"; then
  printf 'ok - run.sh checks jq in nix develop\n'
else
  printf 'not ok - run.sh checks jq in nix develop\n' >&2
  failures=$((failures + 1))
fi

# --- launcher sets workspace-local state and refuses a symlink escape ---
fake_bin=$(mktemp -d)
cat >"$fake_bin/bzb" <<'EOF'
#!/bin/sh
printf 'BUSYBEE_STATE_DIR=%s\n' "$BUSYBEE_STATE_DIR"
printf 'PUEUE_CONFIG_PATH=%s\n' "$PUEUE_CONFIG_PATH"
EOF
chmod +x "$fake_bin/bzb"
launch_out=$(PATH="$fake_bin:$PATH" CLAUDE_PROJECT_DIR="$root" "$launcher" bzb)
if [[ $launch_out == *"BUSYBEE_STATE_DIR=$root/build/sortie-agent-state/busybee"* ]] &&
   [[ $launch_out == *"PUEUE_CONFIG_PATH=$root/build/sortie-agent-state/pueue"* ]]; then
  printf 'ok - isolated.sh exports workspace-local state\n'
else
  printf 'not ok - isolated.sh exports workspace-local state\n%s\n' "$launch_out" >&2
  failures=$((failures + 1))
fi
if PATH="$fake_bin:$PATH" CLAUDE_PROJECT_DIR="$root" "$launcher" bash -c true \
  >/dev/null 2>"$fake_bin/err"; then
  printf 'not ok - isolated.sh rejects a non-stateful first arg\n' >&2
  failures=$((failures + 1))
elif grep -q 'first argument must be' "$fake_bin/err"; then
  printf 'ok - isolated.sh rejects a non-stateful first arg\n'
else
  printf 'not ok - isolated.sh rejects a non-stateful first arg\n%s\n' \
    "$(cat "$fake_bin/err")" >&2
  failures=$((failures + 1))
fi

escape=$(mktemp -d)
mkdir -p "$root/build/sortie-agent-state"
# The launcher refuses a state dir that already points outside the workspace.
if [ -d "$root/build/sortie-agent-state/busybee" ] &&
   [ ! -L "$root/build/sortie-agent-state/busybee" ]; then
  # A leftover real directory from the previous launch is fine; replace it
  # with an escaping symlink for this check.
  rm -rf "$root/build/sortie-agent-state/busybee"
fi
ln -s "$escape" "$root/build/sortie-agent-state/busybee"
if PATH="$fake_bin:$PATH" CLAUDE_PROJECT_DIR="$root" "$launcher" bzb \
  >/dev/null 2>"$fake_bin/escape.err"; then
  printf 'not ok - isolated.sh refuses a state dir outside the workspace\n' >&2
  failures=$((failures + 1))
elif grep -q 'resolves outside the workspace' "$fake_bin/escape.err"; then
  printf 'ok - isolated.sh refuses a state dir outside the workspace\n'
else
  printf 'not ok - isolated.sh refuses a state dir outside the workspace\n%s\n' \
    "$(cat "$fake_bin/escape.err")" >&2
  failures=$((failures + 1))
fi
rm -rf "$root/build/sortie-agent-state/busybee" "$escape" "$fake_bin"

# --- persisted old issue branch still receives the payload ---
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
git -C "$scratch" init -q
git -C "$scratch" checkout -q -b main
mkdir -p "$scratch/sortie"
install -m 0755 "$hook" "$scratch/sortie/machine-safety-hook.sh"
install -m 0755 "$launcher" "$scratch/sortie/isolated.sh"
install -m 0755 "$install_script" "$scratch/sortie/install-machine-safety.sh"
install -m 0644 "$settings" "$scratch/sortie/claude-settings.json"
git -C "$scratch" add sortie
git -C "$scratch" -c user.email=test@example.com -c user.name=test \
  commit -qm 'payload on main'
git -C "$scratch" checkout -q -b old-issue
git -C "$scratch" rm -rq sortie
git -C "$scratch" -c user.email=test@example.com -c user.name=test \
  commit -qm 'issue branch without payload'
git -C "$scratch" update-ref refs/remotes/origin/main main

# Mirror WORKFLOW.md's before_run: load the installer from origin/main.
install_ref=origin/main
if ! git -C "$scratch" cat-file -e "$install_ref:sortie/install-machine-safety.sh" 2>/dev/null; then
  install_ref=HEAD
fi
(
  cd "$scratch"
  git show "$install_ref:sortie/install-machine-safety.sh" \
    | MACHINE_SAFETY_REF="$install_ref" bash
)
test -x "$scratch/.claude/hooks/machine-safety-hook.sh"
test -x "$scratch/.claude/isolated.sh"
test "$install_ref" = origin/main
jq -e '.hooks.PreToolUse[] | select(.matcher | contains("Bash"))' \
  "$scratch/.claude/settings.json" >/dev/null
test -z "$(git -C "$scratch" status --porcelain --untracked-files=no)"
test ! -e "$scratch/sortie/machine-safety-hook.sh"
printf 'ok - before_run payload from origin/main\n'

if ! grep -q 'install-machine-safety.sh' "$root/sortie/WORKFLOW.md"; then
  printf 'not ok - WORKFLOW.md invokes install-machine-safety.sh\n' >&2
  failures=$((failures + 1))
else
  printf 'ok - WORKFLOW.md invokes install-machine-safety.sh\n'
fi

if [ "$failures" -ne 0 ]; then
  printf '%s machine-safety test(s) failed\n' "$failures" >&2
  exit 1
fi
