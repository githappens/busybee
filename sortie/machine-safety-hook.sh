#!/usr/bin/env bash
# PreToolUse guard for disposable sortie workspaces.
#
# Not a same-UID sandbox and not a Bash interpreter. Cooperative agents
# must launch busybee/bzb/bzbd/pueued/pueue through .claude/isolated.sh.
# Anything else that looks like those tools is denied.
set -euo pipefail
shopt -s extglob

input=$(cat)
project_dir=${CLAUDE_PROJECT_DIR:-$PWD}

# Claude Code treats hook exits other than 0 / 2 as non-blocking. If jq is
# missing, emitting this JSON with exit 0 still denies the tool call.
if ! command -v jq >/dev/null 2>&1; then
  printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"machine-safety rule: jq is required to evaluate tool calls"}}'
  exit 0
fi

emit_deny() {
  jq -n --arg reason "$1" '{
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: $reason
    }
  }'
}

deny() {
  emit_deny "machine-safety rule: $1"
  exit 0
}

if ! tool=$(jq -er '.tool_name' <<<"$input"); then
  deny "invalid hook input; refusing the tool call"
fi

STATEFUL_MSG='run Busy Bee/Pueue through .claude/isolated.sh (sets workspace-local BUSYBEE_STATE_DIR and PUEUE_CONFIG_PATH). Example: .claude/isolated.sh bzb -- xcodebuild ...'

# Longer names first so "pueued" is not classified as "pueue" plus a trailing d.
mentions_stateful() {
  [[ $1 =~ (^|[^[:alnum:]_.-])(busybee|bzbd|pueued|pueue|bzb)([^[:alnum:]_.-]|$) ]]
}

# Compound-command markers. Enough to keep `cat …; pueued` from counting as
# a reader or an isolated launch. Not a Bash parser.
has_shell_operator() {
  local c=$1
  [[ $c == *';'* || $c == *'&&'* || $c == *'||'* || $c == *'|'* ]] && return 0
  [[ $c == *'&'* ]] && return 0
  return 1
}

# The whole command must be a reader: no list/pipe operators and no redirects.
# A prefix match would let `cat /dev/null; pueued` through.
is_reader() {
  local c=$1
  has_shell_operator "$c" && return 1
  [[ $c == *'>'* || $c == *'<'* ]] && return 1
  [[ $c =~ ^[[:space:]]*(cat|head|tail|less|more|ls|grep|egrep|fgrep|rg|wc|file|stat)[[:space:]] ]]
}

is_workspace_isolated_script() {
  local p=$1
  p=${p#\"}
  p=${p%\"}
  p=${p#\'}
  p=${p%\'}
  case "$p" in
    .claude/isolated.sh|./.claude/isolated.sh)
      return 0
      ;;
    \$CLAUDE_PROJECT_DIR/.claude/isolated.sh|\$\{CLAUDE_PROJECT_DIR\}/.claude/isolated.sh)
      return 0
      ;;
    "$project_dir"/.claude/isolated.sh)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

# The entire command must be: optional `nix develop -c`, then the installed
# launcher, then one of the five stateful tools.
is_isolated_launch() {
  local c=$1 launcher rest
  c=${c##+([[:space:]])}
  c=${c%%+([[:space:]])}
  has_shell_operator "$c" && return 1
  if [[ $c =~ ^nix[[:space:]]+develop[[:space:]]+-c[[:space:]]+(.*)$ ]]; then
    c=${BASH_REMATCH[1]}
    c=${c##+([[:space:]])}
  fi
  local q=${c:0:1}
  if [ "$q" = '"' ] || [ "$q" = "'" ]; then
    local after=${c:1}
    case "$after" in
      *"$q"*) ;;
      *) return 1 ;;
    esac
    launcher=${after%%"$q"*}
    rest=${after#*"$q"}
    rest=${rest##+([[:space:]])}
  elif [[ $c =~ ^([^[:space:]]+)[[:space:]]+(.*)$ ]]; then
    launcher=${BASH_REMATCH[1]}
    rest=${BASH_REMATCH[2]}
  else
    return 1
  fi
  is_workspace_isolated_script "$launcher" || return 1
  case "${rest%%[[:space:]]*}" in
    busybee|bzb|bzbd|pueued|pueue) return 0 ;;
    *) return 1 ;;
  esac
}

# cargo test/clippy/fmt/check/build/doc, optionally after `cd DIR &&` and/or
# `nix develop -c`. cargo run is not in this list.
is_safe_cargo() {
  local c=$1 cd_re nix_re cargo_re
  [[ $c == *';'* || $c == *'||'* || $c == *'|'* ]] && return 1
  cd_re='^[[:space:]]*cd[[:space:]]+[^[:space:];&|]+[[:space:]]+&&[[:space:]]+(.*)$'
  if [[ $c =~ $cd_re ]]; then
    c=${BASH_REMATCH[1]}
  fi
  [[ $c == *'&&'* || $c == *'&'* ]] && return 1
  nix_re='^[[:space:]]*nix[[:space:]]+develop[[:space:]]+-c[[:space:]]+(.*)$'
  if [[ $c =~ $nix_re ]]; then
    c=${BASH_REMATCH[1]}
  fi
  cargo_re='^[[:space:]]*cargo([[:space:]]+\+[^[:space:]]+)?[[:space:]]+(test|clippy|fmt|check|build|doc)([[:space:]]|$)'
  [[ $c =~ $cargo_re ]]
}

runtime_path() {
  case "$1" in
    .claude|.claude/*|*/.claude|*/.claude/*)
      return 0
      ;;
  esac
  return 1
}

case "$tool" in
  Edit|MultiEdit|Write)
    if ! path=$(jq -er '.tool_input.file_path' <<<"$input"); then
      deny "missing file path; refusing a write whose target cannot be checked"
    fi
    case "$path" in
      .cargo/config.toml|*/.cargo/config.toml)
        deny "do not edit .cargo/config.toml"
        ;;
    esac
    if runtime_path "$path"; then
      deny "do not modify the machine-safety hook or its settings"
    fi
    ;;

  Bash)
    if ! command=$(jq -er '.tool_input.command' <<<"$input"); then
      deny "missing Bash command; refusing a command that cannot be checked"
    fi
    command=${command//$'\n'/ ; }

    if [[ $command =~ (^|[[:space:]\&;\|])nix[[:space:]]+profile[[:space:]]+(install|add)([[:space:]]|$) ]]; then
      deny "do not run nix profile install from an agent workspace"
    fi
    if [[ $command =~ (^|[[:space:]\&;\|])cargo([[:space:]]+\+[^[:space:]]+)?[[:space:]]+install([[:space:]]|$) ]]; then
      deny "do not run cargo install from an agent workspace"
    fi
    if [[ $command =~ (^|[[:space:]\&;\|])(\./)?scripts/buildanddeploy\.sh([[:space:]]|$) ]]; then
      deny "do not install or replace the global busybee/bzb/bzbd/pueued binaries"
    fi
    if [[ $command =~ (cp|mv|install)[[:space:]].*(/usr/local/bin|/opt/homebrew/bin|/\.cargo/bin|/\.nix-profile/bin|/\.local/bin)/(busybee|bzb|bzbd|pueued)([^[:alnum:]_.-]|$) ]]; then
      deny "do not write to a global busybee/bzb/bzbd/pueued binary"
    fi

    if [[ $command =~ git[[:space:]]+clean[[:space:]].*-[a-zA-Z]*[xX] ]]; then
      deny "do not modify the machine-safety hook or its settings"
    fi

    if [[ $command == *'.cargo/config.toml'* ]] && ! is_reader "$command"; then
      deny "do not edit .cargo/config.toml"
    fi

    if mentions_stateful "$command"; then
      if is_isolated_launch "$command"; then
        :
      elif is_safe_cargo "$command"; then
        :
      elif is_reader "$command"; then
        :
      else
        deny "$STATEFUL_MSG"
      fi
    fi

    if [[ $command == *'.claude'* ]] && ! is_reader "$command" && ! is_isolated_launch "$command"; then
      deny "do not modify the machine-safety hook or its settings"
    fi
    ;;

  *)
    deny "unexpected tool '$tool'; refusing a call the hook cannot check"
    ;;
esac
