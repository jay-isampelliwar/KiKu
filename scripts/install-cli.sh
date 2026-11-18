#!/bin/bash
# Install the `preeti` command in ~/.local/bin. It opens the meeting notes menu from any folder.
# Does not touch the background daemon, so no new permission prompts.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$HOME/.local/bin/preeti"

cargo build --release --manifest-path "$REPO/Cargo.toml"

mkdir -p "$(dirname "$TARGET")"
cat > "$TARGET" <<WRAPPER
#!/bin/bash
# Meeting notes menu. Installed by $REPO/scripts/install-cli.sh
exec "$REPO/target/release/meetingd" --config "$REPO/config/config.toml" "\$@"
WRAPPER
chmod +x "$TARGET"

echo "Installed $TARGET"
echo "Run: preeti            (menu)"
echo "     preeti last | todos | list | show <id> | pause | resume"
