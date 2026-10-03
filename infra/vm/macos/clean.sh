#!/usr/bin/env bash
# Runs as the lab account before the baseline snapshot: no task, daemon or
# validation state may survive into it. Dependency caches (the Nix store,
# Cargo's registry) stay; that is what warming was for.
set -euo pipefail
for daemon in pueued bzbd busybee; do
  # pkill exits 1 when nothing matched, which is the expected case.
  pkill -x "$daemon" || [ $? -eq 1 ]
done
rm -rf "$HOME/busybee" "$HOME/.local/state/busybee" "$HOME/Library/Application Support/pueue" \
  "$HOME/.config/pueue" "$HOME/.cache/pueue" "$HOME/.zsh_history" "$HOME/.zsh_sessions" "$HOME/.bash_history"
# What the lab account and the scenarios left in the temporary directories;
# the system's own entries there are root's.
sudo -n find /private/tmp /private/var/tmp -mindepth 1 -maxdepth 1 ! -user root -exec rm -rf {} +
