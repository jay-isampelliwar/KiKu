#!/bin/bash
# Build meetingd, install it outside the repo, and start it now and at every login.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.preeti.meetingd"
APP_DIR="$HOME/Library/Application Support/Preeti"
BIN="$APP_DIR/bin/meetingd"
LOGS="$HOME/Library/Logs/Preeti"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

echo "==> Building release binary"
cargo build --release --manifest-path "$REPO/Cargo.toml"

echo "==> Stopping the old daemon, if running"
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true

echo "==> Installing $BIN"
mkdir -p "$APP_DIR/bin" "$LOGS" "$HOME/Library/LaunchAgents"
cp "$REPO/target/release/meetingd" "$BIN"
codesign --force --sign - --identifier "$LABEL" "$BIN"

echo "==> Writing $PLIST"
sed -e "s|__BIN__|$BIN|g" -e "s|__REPO__|$REPO|g" -e "s|__LOGS__|$LOGS|g" \
    "$REPO/launchd/$LABEL.plist" > "$PLIST"
plutil -lint "$PLIST" > /dev/null

echo "==> Installing the preeti command"
"$REPO/scripts/install-cli.sh" > /dev/null

echo "==> Starting"
launchctl bootstrap "$DOMAIN" "$PLIST"
sleep 3
launchctl print "$DOMAIN/$LABEL" | grep -E "^\s+(state|pid) =" || true

cat <<MSG

Done. meetingd now runs in the background and starts at every login.
  Logs:   $LOGS/meetingd.$(date +%Y-%m-%d).log
  Notes:  run \`preeti\` in any terminal
  Stop:   scripts/uninstall.sh

On the first meeting, macOS asks to allow microphone access for "meetingd". Click Allow.
After every reinstall, macOS may ask again.
MSG
