use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
pub const APP_NAME: &str = "Maprisma WLAN Monitor";
pub const ICON: &str = "io.github.ubruckhaus.CosmicMaprismaWlanMonitor-symbolic";
/// The radar without sweep and dot while not connected (installed under this name).
pub const ICON_OFF: &str = "io.github.ubruckhaus.CosmicMaprismaWlanMonitor-off-symbolic";

/// The full radar in the accent colour (ColorScheme-Highlight) while connected.
pub const ICON_ACTIVE: &str = "io.github.ubruckhaus.CosmicMaprismaWlanMonitor-active-symbolic";

/// The tray's radar: coloured while connected, the plain rings while not.
pub fn icon_for(connected: bool) -> &'static str {
    if connected { ICON_ACTIVE } else { ICON_OFF }
}
pub const WINDOW_BUS: &str = "io.github.ubruckhaus.CosmicMaprismaWlanMonitor.Window";
pub static CLOSE_WINDOW: AtomicBool = AtomicBool::new(false);
pub struct WindowControl;
#[zbus::interface(name = "io.github.ubruckhaus.CosmicMaprismaWlanMonitor.Window")]
impl WindowControl {
    fn close(&self) {
        CLOSE_WINDOW.store(true, Ordering::Relaxed);
    }
}

// Installed beside the binary or in the data home; WLAN_MONITOR_SCRIPTS wins.
pub fn script_path(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("WLAN_MONITOR_SCRIPTS") {
        dirs.push(dir.into());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(bin) = exe.parent()
    {
        dirs.push(bin.join("../share/maprisma-wlan-monitor/scripts"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(data) = data_home {
        dirs.push(data.join("maprisma-wlan-monitor/scripts"));
    }
    dirs.push("/usr/share/maprisma-wlan-monitor/scripts".into());
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

pub mod i18n;
pub mod shot;
pub mod corners;
pub mod look;

#[cfg(test)]
mod icon_tests {
    use super::*;
    #[test]
    fn tray_radar_follows_the_connection() {
        assert_eq!(icon_for(true), ICON_ACTIVE);
        assert_eq!(icon_for(false), ICON_OFF);
    }
}
