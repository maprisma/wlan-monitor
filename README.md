# Maprisma WLAN Monitor for COSMIC

See who is on your Wi-Fi from the COSMIC panel, in plain words. A native libcosmic panel applet, ported from
[WLAN Monitor for GNOME](https://github.com/uBruckhaus/gnome-wlan-monitor) (itself the port of the Omarchy
widget). English only.

- **Panel icon:** a radar. Dim while disconnected, green when connected, blue while a VPN is active, with a thin
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

The collector `scripts/wlan-monitor.py` is the unchanged one from the Omarchy widget and the GNOME extension, so
device names, pins, hidden devices, trusted networks and the FRITZ!Box login are shared with them in
`~/.config/ubruckhaus-wlan-monitor/`. Discovery is passive on untrusted networks; see the GNOME extension's README
for what is sent and stored, and for the FRITZ!Box login (the password goes to the collector over its stdin pipe
and is never shown again; a login typed for one router is never sent to another).

## Install

Needs `python3`, Rust (cargo) and COSMIC. `avahi-browse` and `iw` are used when installed.

```sh
./install.sh
```

Then add **WLAN Monitor** in COSMIC Settings › Desktop › Panel › Applets.

## Tests

```sh
env -u DISPLAY -u WAYLAND_DISPLAY python3 -m unittest discover -s tests
```

The collector's tests (120), copied from the GNOME extension; they use fixtures and never scan a real network.
