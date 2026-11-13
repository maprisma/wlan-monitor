//! Brings the open window to the front for the tray's "Open" (KDE, Hyprland). Shared copy: the
//! same file in every Maprisma Rust tray; change it in all of them.
//!
//! A Wayland window can't raise or un-minimize itself (winit's `focus_window` does nothing there),
//! so the tray asks the compositor for the window of the process that owns the window's D-Bus
//! name: Hyprland through `hyprctl dispatch`, KWin through a one-off KWin script (un-minimize,
//! go to its virtual desktop, activate). Elsewhere (COSMIC uses the panel applet) nothing happens.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    Hyprland,
    Kde,
    Other,
}

/// From the session's variables: Hyprland's instance signature, else KDE in
/// `XDG_CURRENT_DESKTOP` or Plasma's `KDE_FULL_SESSION`.
pub fn compositor_from(hyprland_signature: &str, current_desktop: &str, kde_full_session: &str) -> Compositor {
    if !hyprland_signature.trim().is_empty() {
        Compositor::Hyprland
    } else if current_desktop.split(':').any(|d| d.trim().eq_ignore_ascii_case("KDE"))
        || kde_full_session.trim().eq_ignore_ascii_case("true")
    {
        Compositor::Kde
    } else {
        Compositor::Other
    }
}

fn compositor() -> Compositor {
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    compositor_from(&var("HYPRLAND_INSTANCE_SIGNATURE"), &var("XDG_CURRENT_DESKTOP"), &var("KDE_FULL_SESSION"))
}

/// Hyprland 0.55+ (Lua dispatchers) first, then the classic dispatcher.
pub fn hyprland_dispatches(pid: u32) -> [Vec<String>; 2] {
    [
        vec!["dispatch".into(), format!("hl.dsp.focus({{ window = \"pid:{pid}\" }})")],
        vec!["dispatch".into(), "focuswindow".into(), format!("pid:{pid}")],
    ]
}

/// The KWin script that shows the window of `pid` (a number: nothing else goes into the script).
pub fn kwin_script(pid: u32) -> String {
    format!(
        "(function () {{\n\
         for (const w of workspace.windowList()) {{\n\
         if (w.pid !== {pid} || !w.normalWindow) continue;\n\
         if (w.minimized) w.minimized = false;\n\
         if (!w.onAllDesktops && w.desktops.length > 0 && w.desktops.indexOf(workspace.currentDesktop) < 0) workspace.currentDesktop = w.desktops[0];\n\
         workspace.activeWindow = w;\n\
         break;\n\
         }}\n\
         }})();\n"
    )
}

/// Runs `hyprctl` (or a stand-in in tests) with each dispatch until one succeeds.
pub async fn present_hyprland(hyprctl: &std::path::Path, pid: u32) -> bool {
    for args in hyprland_dispatches(pid) {
        let run = tokio::process::Command::new(hyprctl)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(3), run).await {
            if status.success() {
                return true;
            }
        }
    }
    false
}

async fn present_kde(connection: &zbus::Connection, pid: u32) -> zbus::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|d| d.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let name = format!("maprisma-present-{}-{pid}-{nanos}", std::process::id());
    let path = dir.join(format!("{name}.js"));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(kwin_script(pid).as_bytes()))
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    let result = async {
        let scripting = zbus::Proxy::new(connection, "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting").await?;
        let id: i32 = scripting.call("loadScript", &(path.to_string_lossy().as_ref(), name.as_str())).await?;
        if id >= 0 {
            scripting.call::<_, _, ()>("start", &()).await?;
            // KWin runs it from its event loop; then it is unloaded again.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let _ = scripting.call::<_, _, bool>("unloadScript", &(name.as_str(),)).await;
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&path);
    result
}

/// Shows the window whose D-Bus connection is `owner` (un-minimized, in front).
pub async fn present(connection: &zbus::Connection, owner: &str) {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(connection).await else { return };
    let Ok(name) = zbus::names::BusName::try_from(owner) else { return };
    let Ok(pid) = dbus.get_connection_unix_process_id(name).await else { return };
    match compositor() {
        Compositor::Hyprland => {
            present_hyprland(std::path::Path::new("hyprctl"), pid).await;
        }
        Compositor::Kde => {
            if let Err(error) = present_kde(connection, pid).await {
                eprintln!("KWin: {error}");
            }
        }
        Compositor::Other => {}
    }
}

#[cfg(test)]
mod present_tests {
    use super::*;

    #[test]
    fn which_compositor() {
        assert_eq!(compositor_from("abc_123", "KDE", ""), Compositor::Hyprland);
        assert_eq!(compositor_from("", "KDE", ""), Compositor::Kde);
        assert_eq!(compositor_from("", "foo:kde", ""), Compositor::Kde);
        assert_eq!(compositor_from("", "", "true"), Compositor::Kde);
        assert_eq!(compositor_from("", "COSMIC", ""), Compositor::Other);
        assert_eq!(compositor_from(" ", "", "false"), Compositor::Other);
    }

    #[test]
    fn kwin_script_shows_only_that_pid() {
        let script = kwin_script(4242);
        assert!(script.contains("w.pid !== 4242"));
        assert!(script.contains("w.minimized = false"));
        assert!(script.contains("workspace.activeWindow = w"));
    }

    #[tokio::test]
    async fn hyprland_lua_dispatch_then_classic() {
        let dir = std::env::temp_dir().join(format!("maprisma-present-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let fake = dir.join("hyprctl");
        // Fails the Lua dispatcher (an older Hyprland), takes the classic one.
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$2\" in hl.dsp*) exit 1;; esac\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(present_hyprland(&fake, 77).await);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls, "dispatch hl.dsp.focus({ window = \"pid:77\" })\ndispatch focuswindow pid:77\n");
        assert!(!present_hyprland(&dir.join("missing"), 77).await);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
