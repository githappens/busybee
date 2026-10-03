#!/usr/bin/env bash
# Prints the guest state a baseline must not carry, one `kind: value` per line.
# Runs as the lab account. Not -e: an empty search is the expected answer.
set -u
pgrep -lx 'pueued|bzbd|busybee' | sed 's/^[0-9]* /process: /'
find /private/tmp /private/var/tmp "$HOME/.local" -type s \( -name '*bzbd*' -o -name '*pueue*' \) 2>/dev/null \
  | sed 's/^/socket: /'
for path in "$HOME/busybee" "$HOME/.local/state/busybee" "$HOME/Library/Application Support/pueue" \
  "$HOME/.config/pueue"; do
  [ -e "$path" ] && echo "path: $path"
done
find "$HOME/.local" -name leases.json -path '*busybee*' 2>/dev/null | sed 's/^/path: /'
sed '/^$/d; s/^/key: /' "$HOME/.ssh/authorized_keys" 2>/dev/null
