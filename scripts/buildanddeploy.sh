#!/usr/bin/env bash
# Build busybee in release mode inside `nix develop`, then install/refresh the
# binary in the user's nix profile via `nix profile`.
#
# Usage: scripts/buildanddeploy.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> building release binary"
nix develop --command cargo build --release

if [ ! -f build/release/busybee ]; then
    echo "error: build/release/busybee not produced" >&2
    exit 1
fi
if [ ! -f build/release/bzb ]; then
    echo "error: build/release/bzb not produced" >&2
    exit 1
fi
if [ ! -f build/release/bzbd ]; then
    echo "error: build/release/bzbd not produced" >&2
    exit 1
fi

echo "==> deploying to nix profile"
# BUSYBEE_REPO tells the flake where to find build/release/busybee.
# --impure is required because build/ is gitignored and builtins.getEnv
# is a side-effectful (impure) operation.
export BUSYBEE_REPO="$REPO_ROOT"

# Remove any previous local install; ignore failure (no existing install).
nix profile remove busybee 2>/dev/null || true
nix profile add --impure .

echo "==> installed:"
which busybee || { echo "busybee not on PATH — check ~/.nix-profile/bin"; exit 1; }
which bzb || { echo "bzb not on PATH — check ~/.nix-profile/bin"; exit 1; }
which bzbd || { echo "bzbd not on PATH — check ~/.nix-profile/bin"; exit 1; }
busybee --help | head -3
