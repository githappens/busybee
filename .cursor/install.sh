#!/usr/bin/env bash
# Repository bootstrap for the busybee Cloud Agent environment. Runs after the
# source is checked out; idempotent so it is safe to re-run.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Warm the dependency cache and the build. .cargo/config.toml redirects the
# target directory to build/, so artifacts land there rather than in target/.
cargo fetch --locked
cargo build --workspace --all-targets --locked

# Pre-create pueue's config and data directory tree.
#
# bzbd (busybee's broker) spawns pueued under a restrictive umask (0o177, see
# crates/bzbd/src/lib.rs) so its own state directory is owner-only. That umask
# is inherited by the pueued it starts and strips the execute bit from every
# directory pueued would otherwise create, so on a cold machine pueued cannot
# populate a fresh ~/.config/pueue or ~/.local/share/pueue and busybee's
# auto-start fails with "pueued did not become reachable". Creating the tree
# ahead of time (directories only — pueued still generates its own config,
# certificates and shared secret at runtime, so no secrets are baked into the
# image) lets the umask-restricted pueued write its files into dirs that are
# already traversable.
umask 022
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p \
    "$config_home/pueue" \
    "$data_home/pueue/certs" \
    "$data_home/pueue/log" \
    "$data_home/pueue/task_logs"
