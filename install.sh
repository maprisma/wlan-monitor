#!/usr/bin/env bash
# Build (source tree) or copy (release tarball) and install Maprisma WLAN Monitor for the current user (no root needed):
#   ~/.local/bin/maprisma-wlan-monitor                  the applet
#   ~/.local/bin/maprisma-wlan-monitor-tray             the KDE/Hyprland tray
#   ~/.local/bin/maprisma-wlan-mcp                      the local MCP server
#   ~/.local/share/maprisma-wlan-monitor/scripts/       the collector, network monitor, Wi-Fi setup & MCP scripts
#   ~/.local/share/applications/…desktop                makes it an applet for the panel
#   ~/.local/share/icons/hicolor/scalable/apps/…svg     the radar icon
# Then add it in COSMIC Settings › Desktop › Panel › Applets.
set -euo pipefail
cd "$(dirname "$0")"
id=io.github.ubruckhaus.CosmicMaprismaWlanMonitor
data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin"

# From the source tree: build. From a release tarball: use its prebuilt bin/ binaries.
if [[ -f Cargo.toml ]]; then
    cargo build --release
    built=target/release
elif [[ -x bin/maprisma-wlan-monitor && -x bin/maprisma-wlan-monitor-tray ]]; then
    built=bin
else
    echo "No Cargo.toml and no prebuilt bin/ found next to install.sh." >&2
    exit 1
fi
install -Dm755 "$built/maprisma-wlan-monitor" "$bin/maprisma-wlan-monitor"
install -Dm755 "$built/maprisma-wlan-monitor-tray" "$bin/maprisma-wlan-monitor-tray"
install -Dm644 scripts/wlan-monitor.py "$data/maprisma-wlan-monitor/scripts/wlan-monitor.py"
install -Dm644 scripts/net_monitor.py "$data/maprisma-wlan-monitor/scripts/net_monitor.py"
install -Dm644 scripts/wifi_setup.py "$data/maprisma-wlan-monitor/scripts/wifi_setup.py"
install -Dm755 scripts/wlan-mcp.py "$data/maprisma-wlan-monitor/scripts/wlan-mcp.py"
ln -sf "$data/maprisma-wlan-monitor/scripts/wlan-mcp.py" "$bin/maprisma-wlan-mcp"
install -Dm644 "data/icons/$id-symbolic.svg" "$data/icons/hicolor/scalable/apps/$id-symbolic.svg"
# The tray's radar: in the accent colour while connected, the plain rings while not.
install -Dm644 "data/icons/$id-active-symbolic.svg" "$data/icons/hicolor/scalable/apps/$id-active-symbolic.svg"
install -Dm644 "data/icons/$id-off-symbolic.svg" "$data/icons/hicolor/scalable/apps/$id-off-symbolic.svg"
# The panel starts applets without ~/.local/bin in PATH: the full path.
install -d "$data/applications"
# Quote one Exec argument, then escape the desktop-entry value layer.
desktop_arg() {
    local v=$1
    v=${v//\\/\\\\}
    v=${v//\"/\\\"}
    v=${v//\`/\\\`}
    v=${v//\$/\\\$}
    v="\"$v\""
    v=${v//\\/\\\\}
    v=${v//\%/%%}
    printf '%s' "$v"
}
app_exec=$(desktop_arg "$bin/maprisma-wlan-monitor")
tray_exec=$(desktop_arg "$bin/maprisma-wlan-monitor-tray")
# Keep the existing COSMIC metadata and default invocation.
while IFS= read -r line; do
    if [[ $line == Exec=* ]]; then printf 'Exec=%s\n' "$app_exec"; else printf '%s\n' "$line"; fi
done < "data/$id.desktop" > "$data/applications/$id.desktop"
cat > "$data/applications/$id.Window.desktop" <<EOF
[Desktop Entry]
Type=Application
$(sed -n '/^Name/p; /^Comment/p' "data/$id.desktop")
Exec=$app_exec --window
Icon=$id-symbolic
Terminal=false
NoDisplay=false
Categories=Network;Monitor;
EOF
autostart="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
install -d "$autostart"
cat > "$autostart/$id.Tray.desktop" <<EOF
[Desktop Entry]
Type=Application
$(sed -n '/^Name/p; /^Comment/p' "data/$id.desktop")
Exec=$tray_exec
Icon=$id-symbolic
Terminal=false
OnlyShowIn=KDE;Hyprland;
EOF
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
echo "Installed. Add “Maprisma WLAN Monitor” in COSMIC Settings › Desktop › Panel › Applets."
