#!/bin/bash
# Stop meetingd and remove it from login. Meetings, notes and the database are kept.
set -euo pipefail

LABEL="com.preeti.meetingd"

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null && echo "stopped" || echo "was not running"
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
rm -f "$HOME/Library/Application Support/Preeti/bin/meetingd"

echo "removed from login. Data is still in ~/Library/Application Support/Preeti"
