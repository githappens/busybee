#!/usr/bin/env bash
# PreToolUse guard for the disposable sortie workspaces.
set -euo pipefail

input=$(cat)
project_dir=${CLAUDE_PROJECT_DIR:-$PWD}

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
    ;;

  Bash)
    if ! command=$(jq -er '.tool_input.command' <<<"$input"); then
      deny "missing Bash command; refusing a command that cannot be checked"
    fi
    # Newlines separate shell commands too; flatten them so the checks below
    # cover a multi-line Bash tool call.
    command=${command//$'\n'/ }

    if [[ $command =~ (^|[[:space:]\&;\|])nix[[:space:]]+profile[[:space:]]+install([[:space:]]|$) ]]; then
      deny "do not run nix profile install from an agent workspace"
    fi
    if [[ $command =~ (^|[[:space:]\&;\|])cargo([[:space:]]+\+[^[:space:]]+)?[[:space:]]+install([[:space:]]|$) ]]; then
      deny "do not run cargo install from an agent workspace"
    fi

    # This repository's deploy script replaces the profile binaries. Also
    # catch direct writes to the usual global bin locations and command
    # substitutions that resolve an installed daemon's path.
    if [[ $command =~ (^|[[:space:]\&;\|])(\./)?scripts/buildanddeploy\.sh([[:space:]]|$) ]]; then
      deny "do not install or replace the global busybee/bzbd/pueued binaries"
    fi
    global_bin_write_re='(cp|mv|install|ln|rm|unlink|touch|truncate|tee|chmod|chown)[[:space:]].*(/usr/local/bin|/opt/homebrew/bin|/\.cargo/bin|/\.nix-profile/bin|/\.local/bin)/(busybee|bzbd|pueued)([^[:alnum:]_.-]|$)'
    global_bin_redirect_re='>+[[:space:]]*[^[:space:]&;|]*(/usr/local/bin|/opt/homebrew/bin|/\.cargo/bin|/\.nix-profile/bin|/\.local/bin)/(busybee|bzbd|pueued)([^[:alnum:]_.-]|$)'
    if [[ $command =~ $global_bin_write_re ]] || [[ $command =~ $global_bin_redirect_re ]]; then
      deny "do not write to a global busybee/bzbd/pueued binary"
    fi
    if [[ $command =~ (cp|mv|install|ln|rm|unlink|touch|truncate|tee|chmod|chown)[[:space:]].*\$\((which|command[[:space:]]+-v)[[:space:]]+(busybee|bzbd|pueued)\) ]]; then
      deny "do not write to a global busybee/bzbd/pueued binary"
    fi
    if [[ $command =~ nix[[:space:]]+profile[[:space:]]+(remove|upgrade)[[:space:]].*(busybee|bzbd|pueued) ]]; then
      deny "do not install or replace the global busybee/bzbd/pueued binaries"
    fi

    # Edit/Write calls are checked above. Cover the ordinary shell spellings
    # that can mutate the same file without blocking read-only inspection.
    config_redirect_re='>+[[:space:]]*[^[:space:]&;|]*\.cargo/config\.toml'
    if [[ $command == *".cargo/config.toml"* ]] && {
      [[ $command =~ (^|[[:space:]\&;\|])(rm|unlink|touch|truncate|tee|sed[[:space:]]+-i|perl[[:space:]]+-pi|git[[:space:]]+(checkout|restore))[[:space:]].*\.cargo/config\.toml ]] \
        || [[ $command =~ (^|[[:space:]\&;\|])(cp|mv|install)[[:space:]].*[[:space:]]\.cargo/config\.toml([[:space:]\&;\|]|$) ]] \
        || [[ $command =~ $config_redirect_re ]];
    }; then
      deny "do not edit .cargo/config.toml"
    fi

    # Recognise a daemon in command position: at the beginning of a shell
    # segment, after environment assignments and an optional `env` wrapper.
    assignment='[A-Za-z_][A-Za-z0-9_]*=[^[:space:]\&;\|]+[[:space:]]+'
    segment='(^|[\&;\|][\&;\|]?[[:space:]]*)'
    launches_pueued=false
    launches_bzbd=false
    if [[ $command =~ $segment($assignment)*(env[[:space:]]+($assignment)*)?([^[:space:]\&;\|]*/)?pueued([[:space:]\&;\|]|$) ]]; then
      launches_pueued=true
    fi
    if [[ $command =~ $segment($assignment)*(env[[:space:]]+($assignment)*)?([^[:space:]\&;\|]*/)?bzbd([[:space:]\&;\|]|$) ]]; then
      launches_bzbd=true
    fi

    local_path_assignment() {
      local name=$1 value assignment_re
      assignment_re="(^|[[:space:]&;|])${name}=([^[:space:]&;|]+)"
      if [[ $command =~ $assignment_re ]]; then
        value=${BASH_REMATCH[2]}
        value=${value#\"}; value=${value%\"}
        value=${value#\'}; value=${value%\'}
        case "$value" in
          "$project_dir"/*|\$PWD/*|\$\{PWD\}/*)
            return 0
            ;;
          /*|~*|\$HOME*|\$\{HOME\}*|*../*|..)
            return 1
            ;;
          *)
            return 0
            ;;
        esac
      fi
      return 1
    }

    if $launches_pueued && ! local_path_assignment PUEUE_CONFIG_PATH; then
      deny "launch pueued only with a workspace-local PUEUE_CONFIG_PATH"
    fi
    if $launches_bzbd && ! local_path_assignment BUSYBEE_STATE_DIR; then
      deny "launch bzbd only with a workspace-local BUSYBEE_STATE_DIR"
    fi
    ;;

  *)
    deny "unexpected tool '$tool'; refusing a call the hook cannot check"
    ;;
esac
