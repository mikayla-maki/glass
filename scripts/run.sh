#!/usr/bin/env bash
# Install workspace deps, build every provider, then run the orchestrator
# with the locally-vendored Loom binary on PATH.
# Usage: ./scripts/run.sh [extra cargo run args]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [[ ! -d node_modules ]]; then
    echo "─── npm install (workspaces) ───"
    npm install --silent
fi

echo "─── building providers ───"
npm run build --silent

# Vendored Loom lives at node_modules/.bin/loom. Prepending it to PATH
# means the orchestrator's default `loom` command resolves here, not to a
# global install.
export PATH="$REPO_ROOT/node_modules/.bin:$PATH"

echo "─── cargo run ───"
exec cargo run "$@"
