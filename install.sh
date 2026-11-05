#!/usr/bin/env bash
# Build and install WLAN Monitor for the current user (no root needed):
#   ~/.local/bin/cosmic-wlan-monitor                  the applet
#   ~/.local/share/cosmic-wlan-monitor/scripts/       the collector (wlan-monitor.py)
#   ~/.local/share/applications/…desktop              makes it an applet for the panel
#   ~/.local/share/icons/hicolor/scalable/apps/…svg   the radar icon
# Then add it in COSMIC Settings › Desktop › Panel › Applets.
set -euo pipefail
cd "$(dirname "$0")"
id=io.github.ubruckhaus.CosmicWlanMonitor
data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin"

cargo build --release
install -Dm755 target/release/cosmic-wlan-monitor "$bin/cosmic-wlan-monitor"
install -Dm644 scripts/wlan-monitor.py "$data/cosmic-wlan-monitor/scripts/wlan-monitor.py"
install -Dm644 "data/icons/$id-symbolic.svg" "$data/icons/hicolor/scalable/apps/$id-symbolic.svg"
# The panel starts applets without ~/.local/bin in PATH: the full path.
install -d "$data/applications"
sed "s|^Exec=.*|Exec=$bin/cosmic-wlan-monitor|" "data/$id.desktop" > "$data/applications/$id.desktop"
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
echo "Installed. Add “WLAN Monitor” in COSMIC Settings › Desktop › Panel › Applets."
