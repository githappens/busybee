#!/usr/bin/env bash
# PreToolUse guard for the disposable sortie workspaces.
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

# True when $1 is a workspace-local path (relative, under $PWD / project_dir).
# Reject `..` first so a project/`$PWD` prefix cannot smuggle a traversal.
# Unresolved `$VAR` expansions are not local unless they are `$PWD` / `${PWD}`.
is_local_path() {
  local value=$1 rest
  value=${value#\"}; value=${value%\"}
  value=${value#\'}; value=${value%\'}
  case "$value" in
    *../*|*/..|..)
      return 1
      ;;
    \$PWD|\$\{PWD\}|\$CLAUDE_PROJECT_DIR|\$\{CLAUDE_PROJECT_DIR\})
      return 0
      ;;
    \$PWD/*)
      rest=${value#\$PWD/}
      [[ $rest == *\$* ]] && return 1
      return 0
      ;;
    \$\{PWD\}/*)
      rest=${value#\$\{PWD\}/}
      [[ $rest == *\$* ]] && return 1
      return 0
      ;;
    \$CLAUDE_PROJECT_DIR/*)
      rest=${value#\$CLAUDE_PROJECT_DIR/}
      [[ $rest == *\$* ]] && return 1
      return 0
      ;;
    \$\{CLAUDE_PROJECT_DIR\}/*)
      rest=${value#\$\{CLAUDE_PROJECT_DIR\}/}
      [[ $rest == *\$* ]] && return 1
      return 0
      ;;
    "$project_dir"/*)
      return 0
      ;;
    /*|~*|\$HOME*|\$\{HOME\}*)
      return 1
      ;;
    *\$*)
      return 1
      ;;
    *)
      return 0
      ;;
  esac
}

# Map a hook-visible local state path to a filesystem path, then walk up to
# an existing ancestor and physically resolve it. Reject workspace-relative
# strings whose existing components are symlinks out of the project.
path_resolves_inside_workspace() {
  local value=$1 rest fs path resolved root
  value=${value#\"}; value=${value%\"}
  value=${value#\'}; value=${value%\'}
  case "$value" in
    \$PWD|\$\{PWD\})
      fs=$PWD
      ;;
    \$PWD/*)
      rest=${value#\$PWD/}
      fs=$PWD/$rest
      ;;
    \$\{PWD\}/*)
      rest=${value#\$\{PWD\}/}
      fs=$PWD/$rest
      ;;
    \$CLAUDE_PROJECT_DIR|\$\{CLAUDE_PROJECT_DIR\})
      fs=$project_dir
      ;;
    \$CLAUDE_PROJECT_DIR/*)
      rest=${value#\$CLAUDE_PROJECT_DIR/}
      fs=$project_dir/$rest
      ;;
    \$\{CLAUDE_PROJECT_DIR\}/*)
      rest=${value#\$\{CLAUDE_PROJECT_DIR\}/}
      fs=$project_dir/$rest
      ;;
    /*)
      fs=$value
      ;;
    *)
      fs=$project_dir/$value
      ;;
  esac
  # An earlier rm/ln/mv in this tool call can replace a path after we
  # inspect the filesystem; do not trust pre-execution ancestors.
  if [ "${fs_uncertain:-0}" -eq 1 ]; then
    return 1
  fi
  path=$fs
  while [ ! -e "$path" ] && [ "$path" != / ]; do
    rest=$(dirname "$path")
    [ "$rest" = "$path" ] && break
    path=$rest
  done
  if [ -d "$path" ]; then
    resolved=$(cd "$path" && pwd -P) || return 1
  elif [ -e "$path" ]; then
    resolved=$(cd "$(dirname "$path")" && pwd -P)/$(basename "$path") || return 1
  else
    return 0
  fi
  root=$(cd "$project_dir" && pwd -P) || return 1
  case "$resolved" in
    "$root"|"$root"/*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

# Split a flattened Bash string into shell segments on ; && || | & while
# respecting single/double quotes. Each segment is checked on its own so an
# assignment on an earlier command cannot satisfy a later daemon launch.
split_segments() {
  local text=$1
  local -n _out=$2
  local i=0 len=${#text} ch quote= segment=
  _out=()
  while (( i < len )); do
    ch=${text:i:1}
    if [ -n "$quote" ]; then
      segment+=$ch
      if [ "$ch" = "$quote" ]; then
        quote=
      elif [ "$ch" = '\' ] && [ "$quote" = '"' ] && (( i + 1 < len )); then
        segment+=${text:i+1:1}
        (( i++ )) || true
      fi
      (( i++ )) || true
      continue
    fi
    case "$ch" in
      \'|\")
        quote=$ch
        segment+=$ch
        ;;
      ';')
        _out+=("$segment")
        segment=
        ;;
      '&'|'|')
        if [ "$ch" = '&' ] && [ "${text:i:2}" = '&&' ]; then
          _out+=("$segment")
          segment=
          (( i++ )) || true
        elif [ "$ch" = '|' ] && [ "${text:i:2}" = '||' ]; then
          _out+=("$segment")
          segment=
          (( i++ )) || true
        else
          _out+=("$segment")
          segment=
        fi
        ;;
      *)
        segment+=$ch
        ;;
    esac
    (( i++ )) || true
  done
  _out+=("$segment")
}

# Peel leading VAR=val assignments and unwrap supported wrappers from one
# segment. Sets: peeled_env (name=value lines), argv0, rest_args,
# peeled_chdir (a single env -C/--chdir operand), peel_uncertain (1 when
# an env option could not be parsed confidently). A second cwd wrapper
# sets cwd_uncertain rather than composing directories.
peel_segment() {
  local seg=$1
  local rest=${seg##+([[:space:]])}
  rest=${rest%%+([[:space:]])}
  peeled_env=
  argv0=
  rest_args=
  peel_uncertain=0
  peeled_chdir=

  # Keep unwrapping env / nix develop -c / command until the argv0 is the
  # real program. bzb/busybee are not unwrapped: they auto-spawn bzbd.
  local assign_re nix_re nix_c_re argv_re token_re
  # Keep the regex in a variable so quotes inside the pattern are literal.
  assign_re='^([A-Za-z_][A-Za-z0-9_]*)=("([^"]*)"|'\''([^'\'']*)'\''|[^[:space:]&;|]+)[[:space:]]+(.*)$'
  nix_re='^nix[[:space:]]+develop[[:space:]]+(.*)$'
  nix_c_re='^(.*[[:space:]])?-c[[:space:]]+(.*)$'
  argv_re='^([^[:space:]&;|]+)([[:space:]]+(.*))?$'
  token_re='^([^[:space:]]+)([[:space:]]+(.*))?$'

  peel_assignments() {
    local n v after
    while [[ $rest =~ $assign_re ]]; do
      n=${BASH_REMATCH[1]}
      v=${BASH_REMATCH[3]:-${BASH_REMATCH[4]:-${BASH_REMATCH[2]}}}
      after=${BASH_REMATCH[5]}
      strip_peeled_env "$n"
      peeled_env+="$n=$v"$'\n'
      rest=$after
    done
  }

  # Drop NAME from peeled_env so later env -u / --unset actually unsets it.
  strip_peeled_env() {
    local name=$1 out='' line
    [ -n "$peeled_env" ] || return 0
    while IFS= read -r line || [ -n "$line" ]; do
      case "$line" in
        ''|"$name"=*) continue ;;
      esac
      out+="$line"$'\n'
    done <<<"$peeled_env"
    peeled_env=$out
  }

  # Consume env(1) options (-i, -u NAME, --unset=NAME, --, …) until the
  # operand command. Unknown dash-options leave peel_uncertain set so the
  # caller can fail closed when the segment still mentions a daemon.
  peel_env_options() {
    local tok optarg name
    while :; do
      rest=${rest##+([[:space:]])}
      [ -n "$rest" ] || return 0
      if [[ $rest == -- ]]; then
        rest=
        return 0
      fi
      if [[ $rest =~ ^--[[:space:]]+(.*)$ ]]; then
        rest=${BASH_REMATCH[1]}
        return 0
      fi
      if [[ $rest =~ $assign_re ]]; then
        return 0
      fi
      if ! [[ $rest =~ $token_re ]]; then
        return 0
      fi
      tok=${BASH_REMATCH[1]}
      optarg=${BASH_REMATCH[3]-}
      case "$tok" in
        -i|--ignore-environment)
          peeled_env=
          rest=$optarg
          ;;
        -0|--null|-v|--version|-h|--help)
          rest=$optarg
          ;;
        -u|--unset)
          rest=${optarg##+([[:space:]])}
          if ! [[ $rest =~ $token_re ]]; then
            peel_uncertain=1
            return 0
          fi
          strip_peeled_env "${BASH_REMATCH[1]}"
          rest=${BASH_REMATCH[3]-}
          ;;
        --unset=*)
          name=${tok#--unset=}
          if [ -z "$name" ]; then
            peel_uncertain=1
            return 0
          fi
          strip_peeled_env "$name"
          rest=$optarg
          ;;
        -C|--chdir)
          rest=${optarg##+([[:space:]])}
          if ! [[ $rest =~ $token_re ]]; then
            peel_uncertain=1
            return 0
          fi
          # One env -C/--chdir is resolved; a second cwd wrapper is not composed.
          if [ -n "$peeled_chdir" ]; then
            cwd_uncertain=1
          fi
          peeled_chdir=${BASH_REMATCH[1]}
          rest=${BASH_REMATCH[3]-}
          ;;
        --chdir=*)
          if [ -z "${tok#--chdir=}" ]; then
            peel_uncertain=1
            return 0
          fi
          if [ -n "$peeled_chdir" ]; then
            cwd_uncertain=1
          fi
          peeled_chdir=${tok#--chdir=}
          rest=$optarg
          ;;
        -*)
          peel_uncertain=1
          return 0
          ;;
        *)
          return 0
          ;;
      esac
    done
  }

  while :; do
    rest=${rest##+([[:space:]])}
    peel_assignments
    rest=${rest##+([[:space:]])}
    # Unwrap env / exec / command by basename. command -v/-V is a lookup.
    if [[ $rest =~ $argv_re ]]; then
      local wrap_cmd=${BASH_REMATCH[1]} wrap_rest=${BASH_REMATCH[3]-}
      case "$(daemon_basename "$wrap_cmd")" in
        env)
          rest=$wrap_rest
          peel_env_options
          if [ "$peel_uncertain" -eq 1 ]; then
            break
          fi
          continue
          ;;
        exec)
          rest=$wrap_rest
          continue
          ;;
        command)
          wrap_rest=${wrap_rest##+([[:space:]])}
          case "$wrap_rest" in
            -v|-V|-v[[:space:]]*|-V[[:space:]]*)
              break
              ;;
            -p)
              rest=
              continue
              ;;
            -p[[:space:]]*)
              rest=${wrap_rest#-p}
              rest=${rest##+([[:space:]])}
              continue
              ;;
            *)
              rest=$wrap_rest
              continue
              ;;
          esac
          ;;
      esac
    fi
    # nix develop [flake-args…] -c <cmd>
    if [[ $rest =~ $nix_re ]]; then
      local nix_rest=${BASH_REMATCH[1]}
      if [[ $nix_rest =~ $nix_c_re ]]; then
        rest=${BASH_REMATCH[2]}
        continue
      fi
    fi
    break
  done

  rest=${rest##+([[:space:]])}
  peel_assignments
  rest=${rest##+([[:space:]])}
  if [[ $rest =~ $argv_re ]]; then
    argv0=$(unquote_token "${BASH_REMATCH[1]}")
    rest_args=${BASH_REMATCH[3]-}
  else
    argv0=
    rest_args=
  fi
}

env_value_for() {
  local name=$1 line value=
  local found=0
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      "$name"=*)
        value=${line#*=}
        found=1
        ;;
    esac
  done <<<"$peeled_env"
  [ "$found" -eq 1 ] || return 1
  printf '%s\n' "$value"
}

# Join a relative state path with env --chdir so is_local_path sees the
# directory the daemon will actually use. Absolute / $PWD / $HOME values
# are independent of cwd and are returned unchanged.
resolve_against_chdir() {
  local dir=$1 value=$2
  dir=${dir#\"}; dir=${dir%\"}
  dir=${dir#\'}; dir=${dir%\'}
  value=${value#\"}; value=${value%\"}
  value=${value#\'}; value=${value%\'}
  [ -n "$dir" ] || { printf '%s\n' "$value"; return 0; }
  case "$value" in
    "$project_dir"/*|\$PWD/*|\$\{PWD\}/*|/*|~*|\$HOME*|\$\{HOME\}*|\$*)
      printf '%s\n' "$value"
      ;;
    *)
      printf '%s/%s\n' "${dir%/}" "$value"
      ;;
  esac
}

# True when the named isolation variable is set to a workspace-local path
# in this segment, after env -u/-i and after resolving against env --chdir.
# After an earlier cd/pushd/popd, relative and $PWD paths are not local:
# $PWD follows the new directory. Require $CLAUDE_PROJECT_DIR or an
# absolute project path.
state_path_is_local() {
  local value
  if ! value=$(env_value_for "$1"); then
    return 1
  fi
  if [ -n "$peeled_chdir" ]; then
    value=$(resolve_against_chdir "$peeled_chdir" "$value")
  fi
  if [ "${cwd_uncertain:-0}" -eq 1 ]; then
    case "$value" in
      "$project_dir"|"$project_dir"/*|\$CLAUDE_PROJECT_DIR|\$CLAUDE_PROJECT_DIR/*|\$\{CLAUDE_PROJECT_DIR\}|\$\{CLAUDE_PROJECT_DIR\}/*)
        ;;
      *)
        return 1
        ;;
    esac
  fi
  is_local_path "$value" || return 1
  path_resolves_inside_workspace "$value"
}

unquote_token() {
  local s=$1
  case "$s" in
    \"*\") s=${s#\"}; s=${s%\"} ;;
    \'*\') s=${s#\'}; s=${s%\'} ;;
  esac
  printf '%s\n' "$s"
}

daemon_basename() {
  local name
  name=$(unquote_token "$1")
  name=${name##*/}
  name=${name#./}
  name=$(unquote_token "$name")
  printf '%s\n' "$name"
}

segment_mentions_daemon() {
  [[ $1 =~ (^|[^[:alnum:]_])(pueued|bzbd|busybee|pueue|bzb)([^[:alnum:]_]|$) ]]
}

# Command / process / backtick substitution can launch a nested daemon.
# Do not parse the nested shell; fail closed when a daemon is mentioned.
segment_has_substitution() {
  [[ $1 == *'$('* || $1 == *'<('* || $1 == *'>('* || $1 == *'`'* ]]
}

# argv0 is a command that can mention pueued/bzbd without launching it
# (test names, log grep). cargo is handled separately: cargo run can launch.
known_non_launch() {
  case "$1" in
    command|true|false|echo|printf|cat|less|more|head|tail|wc|stat|file|ls|grep|egrep|fgrep|rg|bat|nl|od|hexdump|sha256sum|md5sum|cksum|diff|jq)
      return 0
      ;;
  esac
  return 1
}

# Empty: not cargo run, or an explicit non-daemon package/bin. bzbd/bzb/pueued:
# classified client or daemon target. targetless: `run`/`r` with no
# -p/--package/--bin. unknown: a run we will not guess (unsupported globals).
cargo_run_daemon_target() {
  local args=${1##+([[:space:]])} tok
  if [[ $args =~ ^\+[^[:space:]]+[[:space:]]+(.*)$ ]]; then
    args=${BASH_REMATCH[1]}
  fi
  while :; do
    args=${args##+([[:space:]])}
    [ -n "$args" ] || { printf ''; return 0; }
    tok=${args%%[[:space:]]*}
    case "$tok" in
      --locked|--offline|--frozen|--quiet|-q|--verbose|-v|--version|-V|--list)
        args=${args#"$tok"}
        ;;
      --color|--config|-Z|--manifest-path|--target-dir|--target|-C|--explain)
        args=${args#"$tok"}
        args=${args##+([[:space:]])}
        [ -n "$args" ] || { printf 'unknown'; return 0; }
        tok=${args%%[[:space:]]*}
        args=${args#"$tok"}
        ;;
      --color=*|--config=*|--manifest-path=*|--target-dir=*|--target=*|-Z=*|--explain=*)
        args=${args#"$tok"}
        ;;
      -*)
        printf 'unknown'
        return 0
        ;;
      *)
        break
        ;;
    esac
  done
  args=${args##+([[:space:]])}
  case "${args%%[[:space:]]*}" in
    run|r) ;;
    *)
      printf ''
      return 0
      ;;
  esac
  if [[ $args =~ (^|[[:space:]])(-p|--package|--bin)[[:space:]]+bzbd([[:space:]]|$) ]] \
    || [[ $args =~ --(package|bin)=bzbd([[:space:]]|$) ]]; then
    printf 'bzbd'
    return 0
  fi
  if [[ $args =~ (^|[[:space:]])(-p|--package|--bin)[[:space:]]+bzb([[:space:]]|$) ]] \
    || [[ $args =~ --(package|bin)=bzb([[:space:]]|$) ]]; then
    printf 'bzb'
    return 0
  fi
  if [[ $args =~ (^|[[:space:]])(-p|--package|--bin)[[:space:]]+busybee([[:space:]]|$) ]] \
    || [[ $args =~ --(package|bin)=busybee([[:space:]]|$) ]]; then
    printf 'bzb'
    return 0
  fi
  if [[ $args =~ (^|[[:space:]])(-p|--package|--bin)[[:space:]]+pueued([[:space:]]|$) ]] \
    || [[ $args =~ --(package|bin)=pueued([[:space:]]|$) ]]; then
    printf 'pueued'
    return 0
  fi
  if [[ $args =~ (^|[[:space:]])(-p|--package|--bin)[[:space:]]+[^[:space:]] ]] \
    || [[ $args =~ --(package|bin)=[^[:space:]] ]]; then
    printf ''
    return 0
  fi
  printf 'targetless'
}

git_clean_removes_ignored() {
  local args=$1 tok
  [[ $args =~ (^|[[:space:]])clean([[:space:]]|$) ]] || return 1
  # shellcheck disable=SC2086
  for tok in $args; do
    case "$tok" in
      --*) continue ;;
      -*[xX]*) return 0 ;;
    esac
  done
  return 1
}

claude_runtime_dir_in() {
  [[ $1 =~ (^|[[:space:]\"\'/])(\./)?\.claude(/|[[:space:]\"\']|$) ]] && return 0
  # `.*` and `.[!.]*` expand to `.claude`; do not interpret the glob.
  [[ $1 =~ (^|[[:space:]\"\'/])(\./)?\.(\*|\[) ]]
}

require_pueued_isolation() {
  if ! state_path_is_local PUEUE_CONFIG_PATH; then
    deny "launch pueued only with a workspace-local PUEUE_CONFIG_PATH"
  fi
}

require_bzbd_isolation() {
  if ! state_path_is_local BUSYBEE_STATE_DIR; then
    deny "launch bzbd only with a workspace-local BUSYBEE_STATE_DIR"
  fi
  if ! state_path_is_local PUEUE_CONFIG_PATH; then
    deny "launch bzbd only with a workspace-local PUEUE_CONFIG_PATH"
  fi
}

is_runtime_hook_path() {
  case "$1" in
    .claude/settings.json|*/.claude/settings.json|.claude/hooks/machine-safety-hook.sh|*/.claude/hooks/machine-safety-hook.sh)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

# Deny writes to a named path unless every mentioning segment is a known reader.
deny_unless_readonly_mention() {
  local needle=$1 reason=$2
  local redirect_re segments seg base
  [[ $command == *"$needle"* ]] || return 0
  redirect_re=">+[[:space:]]*[^[:space:]&;|]*${needle//./\\.}"
  if [[ $command =~ $redirect_re ]]; then
    deny "$reason"
  fi
  segments=()
  split_segments "$command" segments
  for seg in "${segments[@]}"; do
    [[ $seg == *"$needle"* ]] || continue
    peel_segment "$seg"
    base=$(daemon_basename "$argv0")
    case "$base" in
      cat|less|more|head|tail|wc|stat|file|ls|grep|egrep|fgrep|rg|bat|nl|od|hexdump|sha256sum|md5sum|cksum|diff|jq)
        ;;
      *)
        deny "$reason"
        ;;
    esac
  done
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
    if is_runtime_hook_path "$path"; then
      deny "do not modify the machine-safety hook or its settings"
    fi
    ;;

  Bash)
    if ! command=$(jq -er '.tool_input.command' <<<"$input"); then
      deny "missing Bash command; refusing a command that cannot be checked"
    fi
    # Newlines separate shell commands too; flatten them so the checks below
    # cover a multi-line Bash tool call.
    command=${command//$'\n'/ ; }

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

    # Narrow read-only allowlists: enumerating every writer is open-ended.
    deny_unless_readonly_mention '.cargo/config.toml' 'do not edit .cargo/config.toml'
    deny_unless_readonly_mention '.claude/settings.json' \
      'do not modify the machine-safety hook or its settings'
    deny_unless_readonly_mention '.claude/hooks/machine-safety-hook.sh' \
      'do not modify the machine-safety hook or its settings'

    segments=()
    split_segments "$command" segments
    cwd_uncertain=0
    fs_uncertain=0
    for seg in "${segments[@]}"; do
      peel_segment "$seg"
      base=$(daemon_basename "$argv0")
      # Grouping `(cd` / `{cd` is not a full subshell parser: treat it as an
      # unclassified cwd change so later relative paths are not trusted.
      cwd_base=$base
      while [[ $cwd_base == '('* || $cwd_base == '{'* ]]; do
        cwd_uncertain=1
        cwd_base=${cwd_base#(}
        cwd_base=${cwd_base#\{}
      done
      case "$cwd_base" in
        cd|pushd|popd)
          cwd_uncertain=1
          ;;
        ln|rm|mv|rmdir|unlink)
          fs_uncertain=1
          ;;
        builtin)
          # Only cwd-changing builtins; do not unwrap builtin in general.
          case "$(daemon_basename "${rest_args%%[[:space:]]*}")" in
            cd|pushd|popd)
              cwd_uncertain=1
              ;;
          esac
          ;;
      esac
      if [ "$base" = git ] && git_clean_removes_ignored "$rest_args"; then
        deny "do not modify the machine-safety hook or its settings"
      fi
      case "$base" in
        rm|mv|rmdir|unlink|find)
          if claude_runtime_dir_in "$rest_args"; then
            deny "do not modify the machine-safety hook or its settings"
          fi
          ;;
      esac
      if segment_mentions_daemon "$seg" && segment_has_substitution "$seg"; then
        deny "could not classify a pueued/bzbd launch; refusing"
      fi
      if [ "$base" = cargo ]; then
        case "$(cargo_run_daemon_target "$rest_args")" in
          bzbd|bzb)
            require_bzbd_isolation
            ;;
          pueued)
            require_pueued_isolation
            ;;
          targetless)
            deny "could not classify a pueued/bzbd launch; refusing"
            ;;
          unknown)
            if segment_mentions_daemon "$seg"; then
              deny "could not classify a pueued/bzbd launch; refusing"
            fi
            ;;
        esac
        continue
      fi
      if segment_mentions_daemon "$seg"; then
        if [ "$peel_uncertain" -eq 1 ] || [ -z "$argv0" ]; then
          deny "could not classify a pueued/bzbd launch; refusing"
        fi
        case "$base" in
          pueued|pueue|bzbd|bzb|busybee|cd|pushd|popd)
            ;;
          *)
            if ! known_non_launch "$base"; then
              deny "could not classify a pueued/bzbd launch; refusing"
            fi
            ;;
        esac
      fi
      [ -n "$argv0" ] || continue
      case "$base" in
        cd|pushd|popd)
          cwd_uncertain=1
          ;;
        ln|rm|mv|rmdir|unlink)
          fs_uncertain=1
          ;;
        pueued|pueue)
          require_pueued_isolation
          ;;
        bzbd|bzb|busybee)
          require_bzbd_isolation
          ;;
      esac
    done
    ;;

  *)
    deny "unexpected tool '$tool'; refusing a call the hook cannot check"
    ;;
esac
