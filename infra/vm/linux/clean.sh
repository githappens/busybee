#!/usr/bin/env bash
# Runs as root before the baseline snapshot: no task, daemon or validation
# state may survive into it. Dependency caches (the Nix store, Cargo's
# registry) stay; that is what warming was for.
set -euo pipefail
for daemon in pueued bzbd busybee; do
  # pkill exits 1 when nothing matched, which is the expected case.
  pkill -x "$daemon" || [ $? -eq 1 ]
done
rm -rf /root/busybee /root/.local/state/busybee /root/.local/share/pueue /root/.config/pueue \
  /root/.cache/pueue /run/user/0/pueue* /tmp/* /var/tmp/* /root/.bash_history
