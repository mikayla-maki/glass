#!/usr/bin/env bash
# Remove Glass's macOS LaunchAgent. Glass will stop and won't autostart at
# the next login. Does NOT touch your vault, system data, or build outputs.

set -euo pipefail

PLIST_LABEL="dev.glass.glass"
PLIST_PATH="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"

if [[ ! -f "$PLIST_PATH" ]]; then
    echo "No LaunchAgent at $PLIST_PATH. Nothing to do."
    exit 0
fi

launchctl unload "$PLIST_PATH" 2>/dev/null || true
rm "$PLIST_PATH"
echo "Uninstalled. Glass will not autostart at login."
echo "(Vault and system data are untouched; re-run install-autostart.sh to bring it back.)"
