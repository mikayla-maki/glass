#!/usr/bin/env bash
# Install Glass as a macOS LaunchAgent so it starts at login and restarts on
# crash. Idempotent — re-run after `git pull` to rebuild and reload.
#
# Usage:   ./scripts/install-autostart.sh
# Logs:    $GLASS_SYSTEM_DATA/launchd.{out,err}.log
# Remove:  ./scripts/uninstall-autostart.sh

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLIST_LABEL="dev.glass.glass"
PLIST_PATH="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"
LOG_DIR="${GLASS_SYSTEM_DATA:-$HOME/Library/Application Support/Glass}"

cd "$REPO"

if [[ ! -f ".env" ]]; then
    echo "error: .env not found in $REPO" >&2
    echo "Copy .env.example to .env and fill in secrets before installing autostart." >&2
    exit 1
fi

echo "─── Building release binary (cargo build --release) ───"
cargo build --release --quiet

echo "─── Installing workspace + building providers ───"
npm install --silent
npm run build --silent

mkdir -p "$LOG_DIR"

# Resolve node's dir dynamically so PATH works on both Apple Silicon
# (/opt/homebrew/bin) and Intel (/usr/local/bin) brew layouts. Falls back
# to the Apple Silicon path if `which node` fails for some reason.
NODE_DIR="$(dirname "$(command -v node 2>/dev/null || echo /opt/homebrew/bin/node)")"

echo "─── Writing $PLIST_PATH ───"
cat > "$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${PLIST_LABEL}</string>

  <key>ProgramArguments</key>
  <array>
    <string>${REPO}/target/release/glass</string>
  </array>

  <key>WorkingDirectory</key>
  <string>${REPO}</string>

  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>${REPO}/node_modules/.bin:${NODE_DIR}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>

  <key>RunAtLoad</key>
  <true/>

  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>

  <key>StandardOutPath</key>
  <string>${LOG_DIR}/launchd.out.log</string>

  <key>StandardErrorPath</key>
  <string>${LOG_DIR}/launchd.err.log</string>

  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
EOF

# Reload: unload any prior instance (ignore errors if not loaded), then load
# fresh from the new plist.
launchctl unload "$PLIST_PATH" 2>/dev/null || true
launchctl load "$PLIST_PATH"

echo ""
echo "─── Installed. Glass is running and will autostart at login. ───"
echo ""
echo "  Status:    launchctl list | grep ${PLIST_LABEL}"
echo "  Stop:      launchctl unload ${PLIST_PATH}"
echo "  Start:     launchctl load ${PLIST_PATH}"
echo "  Logs:      tail -f ${LOG_DIR}/launchd.{out,err}.log"
echo "  Uninstall: ${REPO}/scripts/uninstall-autostart.sh"
echo ""
echo "After 'git pull', re-run this script to rebuild and reload."
