#!/usr/bin/env bash
# Install Pi (https://pi.dev/) globally via npm.
# Pi is the agent runtime Glass invokes as a subprocess.
set -euo pipefail

if ! command -v npm >/dev/null 2>&1; then
  echo "npm not found. Install Node.js first (e.g. via nvm, brew, or your package manager)." >&2
  exit 1
fi

npm install -g @mariozechner/pi-coding-agent

echo
echo "Done. Verify with:"
echo "  pi --version"
echo
echo "Then make sure ANTHROPIC_API_KEY is set in your environment (or in glass/.env)."
