# Maprisma WLAN Monitor for COSMIC

See who is on your Wi-Fi from the COSMIC panel, in plain words. A native libcosmic panel applet, one of the
[Maprisma](https://github.com/maprisma) apps: the COSMIC port of the Omarchy widget, which stays as it is.
English only.

- **Panel icon:** a radar. Dim while disconnected, the theme accent when connected, with a thin
  signal-quality line underneath coloured by level, and a red dot when a new device joined. It pulses while the
  subnet is being scanned. **Right click:** an immediate rescan.
- **Panel (left click):**
  - **Connection:** this PC's and the router's address, signal in words (Excellent / Good / Fair / Weak) with dBm,
    channel and link speed, live download / upload, a VPN line. The summary line ("Your network · via FRITZ!Box")
    opens the explanations and the optional FRITZ!Box login.
  - **Devices:** every device on the local subnet with a type icon, name, maker and status. Click one for its
    details in plain words and **Rename**, **Pin to top**, **Hide**, **Open web page**, **It's mine**; copy its IP
    address. **Show N hidden**, **Copy list** (Markdown table), search with more than 12 devices.
  - **Groups:** No groups, By platform, By type, and with FRITZ!Box mesh details By band and By access point;
    groups fold with a click, **+** / **−** open or fold all.
  - **Network map:** Internet › router › groups › devices, repeaters as their own branches, link speed and
    signal bars per device; a click on a device opens it on the Devices page.
  - **New-device alerts** on trusted networks (a desktop notification from the collector and a NEW badge),
    **All known**, the **New-device alerts** switch.
  - **Rescan**, **Router** (its web page), **Trust / Untrust**, and COSMIC Settings' Wi-Fi page.
  - When a VPN blocks the home network: which one, and how to allow it (with a copyable command).

The collector `scripts/wlan-monitor.py` is the unchanged one from the Omarchy widget, so device names, pins,
hidden devices, trusted networks and the FRITZ!Box login are shared with it in
`~/.config/ubruckhaus-wlan-monitor/`. What it sends and stores is described below.

## Trusted networks and scanning

Discovery is **passive** on every network by default: the kernel neighbor table, mDNS announcements and single reverse-DNS lookups for devices that are already visible. Nothing sweeps the subnet automatically, so the applet behaves well on hotel, café and office Wi-Fi. The only connections it makes there on its own are single TCP connect checks of the router's web interface (ports 80 and 443) and of web interfaces that devices advertise over mDNS (before a link to them is offered), the router check while a VPN is up (see above), the unauthenticated FRITZ!Box description, and an empty UDP datagram to the router's discard port (at most every 10 s, only through the monitored interface) when the kernel's entry for the router has aged out, so the trusted-network check keeps working when nothing else talks to the router (e.g. all traffic goes through a VPN). **Rescan** runs one full sweep on any network when you ask for it.

Click **Trust** in the popup to allow **active** scanning on the current network (identified by SSID and router MAC address, both seen at that moment and read again, together with the shared trust list, right before every automatic sweep or probe starts: while the router is not in the neighbour table, or after a disconnect, nothing is inherited from before):

- a full sweep every 2 minutes (and when you open the panel, if the last one is older than 30 s): paced ICMP echo to the local `/24` plus a reverse-DNS (PTR) query per address to the router, which also finds sleeping DHCP leases;
- every 15 s a unicast probe of the devices already known, so the online state stays accurate without broadcast ARP traffic.

Every probe, PTR query (asked of the router only, never of the system resolver) and web-interface check leaves only through the monitored interface (`SO_BINDTODEVICE`; nothing is sent if that binding fails, and only IPv4 addresses are contacted). The one exception is the VPN check, a TCP connect to the router on port 80, 53 or 443 that deliberately follows normal routing to see whether an active VPN blocks the home network.

**Rescan** (or right click) runs one active sweep on demand, on any network. Trusted networks are stored in `~/.config/ubruckhaus-wlan-monitor/trusted-networks.json` (directory `0700`, file `0600`), written only when you click Trust or Untrust.

### FRITZ!Box login

The login is optional and only offered when the router is a FRITZ!Box: its maker in the MAC address database says so, or, when the database does not know the router's maker, its public TR-064 description (asked once per router, on trusted networks only) names a FRITZ!Box model. Where to find the login:

- There is no fixed default user name. FRITZ!OS before 7.24 asks only for a password; leave the user name empty.
- FRITZ!OS 7.24 and newer: if you never created a user, an update created one named `fritz` plus four random digits (e.g. `fritz1234`). Log in at http://fritz.box and look under System › FRITZ!Box Users.
- Unless you changed it, the password is printed on the label on the bottom or back of the FRITZ!Box (4 to 6 lowercase letters followed by 4 digits).

You can also create a separate FRITZ!Box user with the right "FRITZ!Box settings" (System › FRITZ!Box Users) and keep "Allow access for applications" on (Home Network › Network › Network Settings).

- The user name and password go from the panel to the collector over its stdin pipe (never as command-line arguments) and are saved in `~/.config/ubruckhaus-wlan-monitor/fritzbox.json` (`0600`) only after a successful login. The password is never sent back to the panel; the saved user name is, so the form can offer it again when you reconnect.
- They are bound to that FRITZ!Box's MAC address and used only with the current gateway when its MAC matches. All FRITZ!Box connections leave only through the monitored network interface (`SO_BINDTODEVICE`), and the router's MAC is read from that interface's neighbour table, so a device with the same address on another network (e.g. Ethernet and Wi-Fi at once) never gets them. They go over TR-064 HTTPS (port 49443) with HTTP Digest authentication, so the password itself is never transmitted.
- Only one collector per user talks to the FRITZ!Box: the first to take `fritzbox-leader.lock` (this applet or an Omarchy widget on the same account) holds it while it runs, reads and saves the login and polls the mesh list, and publishes what it reads, never the password, in `fritzbox-state.json`. The others show that only while it is fresh and for the router they see themselves, and pass a login or Disconnect to it over a private Unix socket in `$XDG_RUNTIME_DIR/ubruckhaus-wlan-monitor/` (`0600`, peers checked with `SO_PEERCRED`), with a random id and a 5 s expiry; it is answered on its own thread, a request reached after its expiry is dropped and never carried out late, and only then is it reported as not passed on. That collector reads the saved login again before every refresh and each request, so a login deleted from outside is dropped at once. Disconnect drops the login at once and deletes `fritzbox.json` (or empties it if it cannot be deleted). When that collector ends, another one takes over within a few seconds.
- The saved login is shared with any other collector using the same folder (e.g. the Omarchy widget): before every refresh the collector reads it again, so a **Disconnect** elsewhere stops it too, and a login saved elsewhere is used from then on.
- The FRITZ!Box certificate is self-signed; its SHA-256 fingerprint is pinned at login. If it changes, nothing is sent and the panel asks you to connect again.
- A login is bound to the router it was typed for. The collector gives the network it sees a fresh id whenever the gateway (address, MAC or interface) changes; the form remembers the id it was opened with and sends it with the login, and the collector refuses the login unless that id is still current when the login arrives, so a login typed on one router is never queued for another, even if the panel has not caught up with a change yet. When the network changes while the form is open, the form closes and the typed password and user name are cleared; Cancel and Escape clear only the password, and a user name left from another network is cleared when the form is opened again. If the network changes before it is sent, or while it is being sent, it is dropped and never sent to the new router: right before the password-derived response goes out, the gateway is checked again (no change reported since the login was typed, and the kernel still has that router's MAC at the address). Nothing from such a login is saved; the panel asks you to connect again.
- Reads: `X_AVM-DE_GetMeshListPath` and the mesh list it points to, every 30 s while the panel is open and every 5 minutes otherwise. Replies are size-limited (64 KiB SOAP, 2 MiB mesh list), time-limited (4 s) and every field is validated and sanitized. Nothing is ever changed on the FRITZ!Box.

Device names, pins and hidden flags are stored per network (router MAC + SSID) in `~/.config/ubruckhaus-wlan-monitor/devices.json` (`0600`). On trusted networks the MAC addresses seen there are remembered too, so new devices can be recognised; on other networks only devices you rename, pin or hide are written. The file is bounded (32 networks, 512 devices each) and read with the same checks as the trust list (no symlinks or other special files, owned by you, at most 8 MiB, which the largest valid store fits in, every field validated, times only within a sane range). Each change re-reads the file under a shared lock, applies only that change and writes it atomically, and the collector reads it again when it changed, so it never undoes a change made by the Omarchy widget or another collector on the same account (without the lock within 2 s, a change is shown but not written).

## F12: a screenshot with a note

While the popup (or the KDE window) is open, **F12** (without Ctrl, Shift, Alt or Super) saves a picture of it,
for telling what is wrong with it:

- **Only on F12.** Nothing is captured at any other time, and only this app's own popup or window: libcosmic draws
  it once more into memory (iced's `window::screenshot`) and the app encodes that as PNG. No screen capture tool
  or portal runs, nothing else on the screen is in it, and nothing is uploaded or sent anywhere. The picture shows
  what the popup shows, so device names and addresses on screen are in it.
- **Where:** `~/Pictures/Maprisma WLAN Monitor/maprisma-wlan-monitor-<date>-<time>.png` (the pictures folder from
  `~/.config/user-dirs.dirs` when it is set there; that file is only read when it is a small regular file). The
  popup then names the file and asks **What is wrong? (optional)**; **Enter** or **Done** saves the text next to
  the picture as a `.txt` with the same name (at most 4000 characters; no text, no file), **Esc** skips it, and
  closing the popup or window keeps what was typed.
- **Private files:** the folder is made `0700` when missing and only used when it is a real folder (not a link) of
  yours that no one else can write to; pictures and notes are new `0600` files created inside that folder,
  never written through a link or over an existing file or note (`-2`, `-3` … are added instead), and a file that
  cannot be written completely is removed again.
- **Off:** switch **F12 saves a screenshot** off at the bottom of the popup (stored as `"screenshot_key": false`
  in `~/.config/ubruckhaus-wlan-monitor/maprisma-settings.json`, beside the language).

The KDE tray icon's menu is drawn by Plasma and has no F12.

## Corners

**Corners** at the bottom of the popup or window: *Like the desktop (… px)* (the default: the COSMIC theme's corner
radius, 16, 8 or 2 px by COSMIC Settings › Appearance; on KDE the theme libcosmic loads there, normally 16 px),
*Slightly rounded (6 px)*, *Rounded (12 px)* or *Square*. Cards and the popup's frame take the full radius,
buttons, search and rename fields, the notice boxes and the open device's row at most 8 px, small parts (meter
tracks, fills, signal bars) at most 4 px; the "N online" pill stays round, or square in *Square*. The panel icon
and the KDE window's own frame keep the desktop's corners. Stored as `"corners"` in `maprisma-settings.json`;
*Like the desktop* removes it again, and files without these keys stay valid.

## Install

Needs `python3`, Rust (cargo) and COSMIC. `avahi-browse` and `iw` are used when installed.

```sh
./install.sh
```

Then add **Maprisma WLAN Monitor** in COSMIC Settings › Desktop › Panel › Applets.

## Tests

```sh
env -u DISPLAY -u WAYLAND_DISPLAY python3 -m unittest discover -s tests
```

The collector's tests (120); they use fixtures and never scan a real network.

## KDE Plasma

On KDE Plasma 6.8 (Wayland), run `./install.sh` and launch **Maprisma WLAN Monitor**
from the application launcher, or run `~/.local/bin/maprisma-wlan-monitor --window`.
The resizable libcosmic window contains the same UI as the COSMIC popup, with the
same collector, settings, commands and refresh subscriptions. Its settings button
opens the desktop's network settings (COSMIC's, KDE's; on Hyprland the first installed of
Omarchy's Wi-Fi launcher, `nm-connection-editor`, KDE's, COSMIC's). Only one window runs
per session.

The installer also installs `~/.local/bin/maprisma-wlan-monitor-tray` and a
KDE-only autostart entry in `${XDG_CONFIG_HOME:-~/.config}/autostart/` (active on
next login). Run the tray binary manually to start it immediately. Left click
opens or closes the window; the menu offers **Open Maprisma WLAN Monitor** (brings an
open window to the front, also a minimized one) and **Quit tray**. Quitting the tray
leaves an open window running. The tray reconnects when Plasma's StatusNotifierWatcher
restarts, and only one tray runs per session. While no window is open the tray runs the
collector itself, at its "panel closed" pace, so discovery and new-device alerts go on
after the window closes (as with COSMIC's applet); it ends with the tray. Its tooltip
refreshes connection, signal and VPN status every ten seconds using the collector's
read helpers. The radar shows the connection, as the COSMIC applet does: the full radar in the
theme accent (`…WlanMonitor-active-symbolic`) while a link is up, only its rings (`…WlanMonitor-off-symbolic`) while not connected.

Running `maprisma-wlan-monitor` without arguments still starts the COSMIC applet.
The shared `scripts/` files are unchanged.

Rust validation:

```sh
cargo test
cargo build --release --offline
```
