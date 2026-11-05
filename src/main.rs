// WLAN Monitor for COSMIC: a panel applet for the local WLAN. A dim radar while
// disconnected; green while connected, blue while a VPN is active, with a thin
// signal-quality line underneath. It pulses while the subnet is being scanned,
// and a dot marks devices that joined recently. Left click opens the panel,
// right click rescans at once.
//
// The data comes from scripts/wlan-monitor.py (the same collector as the GNOME
// extension and the Omarchy widget, unchanged), which streams a JSON snapshot
// every 3 s while the panel is open (6 s while closed). Commands (rescan, panel
// open/closed, trust/untrust, per-device rename / pin / hide as one-line JSON)
// go to it over stdin. Names, pins, trusted networks and the FRITZ!Box login are
// kept by the collector in ~/.config/ubruckhaus-wlan-monitor/, shared with the
// GNOME extension and the Omarchy widget.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use cosmic::app::{Core, Task};
use cosmic::applet::cosmic_panel_config::PanelAnchor;
use cosmic::iced::futures::SinkExt;
use cosmic::iced::futures::channel::mpsc;
use cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup;
use cosmic::iced::widget::{Row, container as iced_container, svg as iced_svg, text as iced_text};
use cosmic::iced::{Alignment, Background, Border, Color, Length, Rectangle, Subscription, stream, window};
use cosmic::widget::rectangle_tracker::{RectangleTracker, RectangleUpdate, rectangle_tracker_subscription};
use cosmic::widget::{
    Id, autosize, button, column, container, divider, icon, mouse_area, row, scrollable, secure_input, space, svg,
    text, text_input, toggler,
};
use cosmic::{Element, theme};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

const APP_ID: &str = "io.github.ubruckhaus.CosmicWlanMonitor";
static AUTOSIZE_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-autosize"));
static SEARCH_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-search"));
static RENAME_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-rename"));
static FRITZ_USER_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-fritz-user"));
static FRITZ_PASSWORD_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-fritz-password"));
const RADAR: &[u8] = include_bytes!("../data/icons/io.github.ubruckhaus.CosmicWlanMonitor-symbolic.svg");

const RESTART_DELAY: Duration = Duration::from_secs(5); // before a collector that exited starts again
const POPUP_WIDTH: f32 = 440.0;
const POPUP_MAX_HEIGHT: f32 = 780.0;
const LIST_MAX_HEIGHT: f32 = 420.0;
const CLIPBOARD_LIMIT: usize = 262144;
const FLASH: Duration = Duration::from_millis(1500);

fn main() -> cosmic::iced::Result {
    cosmic::applet::run::<Monitor>(())
}

// ------------------------------------------------------------------------- texts and values

// Signal level: green >= 70 %, yellow >= 50 %, orange >= 30 %, red below.
fn signal_color(fraction: f64) -> Color {
    if fraction >= 0.7 {
        Color::from_rgb8(0x33, 0xd1, 0x7a)
    } else if fraction >= 0.5 {
        Color::from_rgb8(0xf6, 0xd3, 0x2d)
    } else if fraction >= 0.3 {
        Color::from_rgb8(0xff, 0x78, 0x00)
    } else {
        Color::from_rgb8(0xe0, 0x1b, 0x24)
    }
}

const GREEN: Color = Color::from_rgb(0x33 as f32 / 255.0, 0xd1 as f32 / 255.0, 0x7a as f32 / 255.0);
const BLUE: Color = Color::from_rgb(0x35 as f32 / 255.0, 0x84 as f32 / 255.0, 0xe4 as f32 / 255.0);

fn signal_word(quality: f64) -> &'static str {
    if quality >= 70.0 {
        "Excellent"
    } else if quality >= 50.0 {
        "Good"
    } else if quality >= 30.0 {
        "Fair"
    } else {
        "Weak"
    }
}

fn num(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64).filter(|v| v.is_finite())
}

fn n(value: &Value, key: &str) -> f64 {
    num(value, key).unwrap_or(0.0)
}

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn b(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

fn arr<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

// A number or a string as text ("36", "866"), empty for null.
fn shown(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::Number(v)) => v.to_string(),
        Some(Value::String(v)) => v.clone(),
        _ => String::new(),
    }
}

fn is_ipv4(ip: &str) -> bool {
    let parts: Vec<&str> = ip.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|c| c.is_ascii_digit()))
}

// The collector only builds http(s) URLs for private IPv4 literals; check the
// shape again here (http(s)://a.b.c.d[:port]/path) before handing it on.
fn is_web_url(url: &str) -> bool {
    let rest = match url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
        Some(rest) => rest,
        None => return false,
    };
    let Some(slash) = rest.find('/') else { return false };
    let (host, path) = rest.split_at(slash);
    let (ip, port) = match host.split_once(':') {
        Some((ip, port)) => (ip, Some(port)),
        None => (host, None),
    };
    if !is_ipv4(ip) {
        return false;
    }
    if let Some(port) = port
        && (port.is_empty() || port.len() > 5 || !port.bytes().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    path.len() <= 101 && path.bytes().all(|c| c.is_ascii_alphanumeric() || b"._~/-".contains(&c))
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "router" => "network-wireless-symbolic",
        "computer" | "laptop" => "computer-symbolic",
        "phone" => "phone-symbolic",
        "tablet" => "tablet-symbolic",
        "watch" => "preferences-system-time-symbolic",
        "tv" => "tv-symbolic",
        "speaker" => "audio-speakers-symbolic",
        "printer" => "printer-symbolic",
        "console" => "input-gaming-symbolic",
        "camera" => "camera-web-symbolic",
        "light" => "night-light-symbolic",
        "nas" => "drive-harddisk-symbolic",
        "appliance" => "application-x-appliance-symbolic",
        "smarthome" => "user-home-symbolic",
        _ => "dialog-question-symbolic",
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "router" => "Router",
        "computer" => "Computer",
        "laptop" => "Laptop",
        "phone" => "Phone",
        "tablet" => "Tablet",
        "watch" => "Watch",
        "tv" => "TV",
        "speaker" => "Speaker",
        "printer" => "Printer",
        "console" => "Game console",
        "camera" => "Camera",
        "light" => "Lighting",
        "nas" => "Storage / server",
        "appliance" => "Appliance",
        "smarthome" => "Smart-home device",
        _ => "Device",
    }
}

fn device_kind_label(dev: &Value) -> &'static str {
    if s(dev, "kind") == "router" && !b(dev, "is_gateway") { "Repeater / access point" } else { kind_label(s(dev, "kind")) }
}

// Group id → (title, icon).
fn group_info(id: &str) -> (&'static str, &'static str) {
    match id {
        "favourites" => ("Favourites", "starred-symbolic"),
        "apple" => ("Apple", "phone-apple-iphone-symbolic"),
        "android" => ("Android", "phone-symbolic"),
        "mobile" => ("Phones & tablets", "tablet-symbolic"),
        "computers" => ("Computers", "computer-symbolic"),
        "media" => ("TV & audio", "tv-symbolic"),
        "smarthome" => ("Smart home", "user-home-symbolic"),
        "office" => ("Printers & storage", "printer-symbolic"),
        "network" => ("Network & mesh", "network-workgroup-symbolic"),
        "none" => ("Not reported by the FRITZ!Box", "dialog-question-symbolic"),
        "gsix" => ("6 GHz", "network-wireless-symbolic"),
        "gfive" => ("5 GHz", "network-wireless-symbolic"),
        "gtwo" => ("2.4 GHz", "network-wireless-symbolic"),
        "glan" => ("LAN (cable)", "network-wired-symbolic"),
        "gother" => ("Band not reported", "dialog-question-symbolic"),
        "gsleep" => ("Sleeping", "weather-clear-night-symbolic"),
        _ => ("Other devices", "dialog-question-symbolic"),
    }
}

const PLATFORM_ORDER: &[&str] =
    &["favourites", "apple", "android", "mobile", "computers", "media", "smarthome", "office", "network", "other"];
const TYPE_ORDER: &[&str] = &["favourites", "mobile", "computers", "media", "smarthome", "office", "network", "other"];
const BAND_ORDER: &[&str] = &["favourites", "gsix", "gfive", "gtwo", "glan", "gother", "gsleep"];

fn order_for(mode: &str) -> &'static [&'static str] {
    match mode {
        "band" => BAND_ORDER,
        "type" => TYPE_ORDER,
        _ => PLATFORM_ORDER,
    }
}

fn kind_group(kind: &str) -> &'static str {
    match kind {
        "router" => "network",
        "phone" | "tablet" | "watch" => "mobile",
        "computer" | "laptop" => "computers",
        "tv" | "speaker" | "console" => "media",
        "smarthome" | "light" | "appliance" | "camera" => "smarthome",
        "printer" | "nas" => "office",
        _ => "other",
    }
}

fn band_group(band: &str) -> &'static str {
    match band {
        "6 GHz" => "gsix",
        "5 GHz" => "gfive",
        "2.4 GHz" => "gtwo",
        "LAN" => "glan",
        _ => "gother",
    }
}

fn view_short(mode: &str) -> &'static str {
    match mode {
        "platform" => "By platform",
        "type" => "By type",
        "band" => "By band",
        "ap" => "By access point",
        _ => "No groups",
    }
}

fn signal_quality(dbm: Option<f64>) -> f64 {
    dbm.map_or(-1.0, |d| (2.0 * (d + 100.0)).clamp(0.0, 100.0))
}

fn format_bytes(bytes: f64) -> String {
    if bytes >= 1073741824.0 {
        format!("{:.1} GiB", bytes / 1073741824.0)
    } else if bytes >= 1048576.0 {
        format!("{:.1} MiB", bytes / 1048576.0)
    } else if bytes >= 1024.0 {
        format!("{:.0} KiB", bytes / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_rate(rate: Option<f64>) -> String {
    match rate {
        None => "—".into(),
        Some(r) if r >= 1048576.0 => format!("{:.1} MB/s", r / 1048576.0),
        Some(r) if r >= 1024.0 => format!("{:.1} KB/s", r / 1024.0),
        Some(r) => format!("{} B/s", r.round()),
    }
}

fn ago_text(dev: &Value) -> String {
    match num(dev, "seen_min") {
        None => String::new(),
        Some(m) if m < 1.0 => "just now".into(),
        Some(m) if m < 60.0 => format!("{} min ago", m.round()),
        Some(m) => format!("{} h ago", (m / 60.0).floor()),
    }
}

fn status_word(dev: &Value) -> String {
    if b(dev, "is_gateway") {
        return "router".into();
    }
    if b(dev, "is_self") {
        return "this PC".into();
    }
    if b(dev, "online") {
        return "online".into();
    }
    if s(dev, "state") == "idle" {
        return "idle".into();
    }
    let ago = ago_text(dev);
    if ago.is_empty() { "sleeping".into() } else { format!("sleeping · {ago}") }
}

fn status_explain(dev: &Value) -> String {
    if b(dev, "is_gateway") {
        return "Your router: it connects all devices here to each other and to the internet.".into();
    }
    if b(dev, "is_self") {
        return "This computer.".into();
    }
    if b(dev, "online") {
        return "Online: the device answered just now.".into();
    }
    if s(dev, "state") == "idle" {
        return "Idle: connected recently but quiet for a while. On a network you have not trusted, the monitor does \
                not ask devices directly, so it cannot tell whether they are asleep."
            .into();
    }
    let ago = ago_text(dev);
    format!(
        "Sleeping: not answering right now. Phones and tablets do this to save battery and still count as connected. {}",
        if ago.is_empty() { "Known from your router's address list.".to_string() } else { format!("Last seen {ago}.") }
    )
}

fn device_key(dev: &Value) -> String {
    let mac = s(dev, "mac");
    if mac.is_empty() { s(dev, "ip").to_string() } else { mac.to_string() }
}

fn fritz_message(code: &str, args: &[Value]) -> String {
    let arg = |i: usize| args.get(i).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)).unwrap_or_default();
    match code {
        "fzConnecting" => "Connecting to the FRITZ!Box…".into(),
        "fzPaused" => "Paused while the VPN blocks the home network.".into(),
        "fzMethod" => "The FRITZ!Box asked for an unsupported login method.".into(),
        "fzCert" => "The FRITZ!Box certificate changed. Connect again to trust the new one.".into(),
        "fzNetworkChanged" => "The network changed before the login was sent. Connect again.".into(),
        "fzAppAccess" => "The FRITZ!Box does not accept app access. Turn on “Allow access for applications” under \
                          Home Network › Network › Network Settings."
            .into(),
        "fzNoAnswer" => "The FRITZ!Box did not answer.".into(),
        "fzNotStored" => "The login could not be stored in the settings folder.".into(),
        "fzNotDeleted" => {
            "The saved login could not be deleted yet. WLAN Monitor keeps trying and does not use it.".into()
        }
        "fzNotSent" => "The request could not be passed to the collector that talks to the FRITZ!Box.".into(),
        "fzWrongLogin" => "Wrong user name or password.".into(),
        "fzNoRight" => {
            "This FRITZ!Box user may not read the home network. Give it the right “FRITZ!Box settings”.".into()
        }
        "fzRefused" => format!("The FRITZ!Box refused the request (error {}).", arg(0)),
        "fzNoMesh" => "The FRITZ!Box sent no mesh list.".into(),
        "fzMeshUnreadable" => "The FRITZ!Box mesh list could not be read.".into(),
        "fzMeshEmpty" => "The FRITZ!Box mesh list was empty.".into(),
        _ => String::new(),
    }
}

const FRITZ_EXPLAIN: &str = "Shows which access point, band and speed every device uses, and the names from your \
    FRITZ!Box. Use a FRITZ!Box user with the right “FRITZ!Box settings” (System › FRITZ!Box Users). The password is \
    stored only on this PC (readable by you alone) and used only with this FRITZ!Box.";
const FRITZ_HELP: &str = "• There is no fixed default user name.\n• FRITZ!OS before 7.24: the FRITZ!Box asks only for \
    a password. Leave the user name empty.\n• FRITZ!OS 7.24 and newer: if you never created a user, an update \
    created one named “fritz” plus four random digits (for example fritz1234). Log in at http://fritz.box and look \
    under System › FRITZ!Box Users.\n• Password: unless you changed it, it is printed on the label on the bottom or \
    back of the FRITZ!Box (4 to 6 lowercase letters followed by 4 digits).";

// ------------------------------------------------------------------------- widgets

fn dim_text(theme: &cosmic::Theme) -> iced_text::Style {
    let mut color = Color::from(theme.cosmic().on_bg_color());
    color.a = 0.6;
    iced_text::Style { color: Some(color), ..Default::default() }
}

fn accent_text(theme: &cosmic::Theme) -> iced_text::Style {
    iced_text::Style { color: Some(Color::from(theme.cosmic().accent_text_color())), ..Default::default() }
}

fn warn_text(_theme: &cosmic::Theme) -> iced_text::Style {
    iced_text::Style { color: Some(Color::from_rgb8(0xff, 0x78, 0x00)), ..Default::default() }
}

fn dim<'a>(content: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Message> {
    text::caption(content).class(theme::Text::Custom(dim_text)).into()
}

fn accent<'a>(content: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Message> {
    text::caption(content).class(theme::Text::Custom(accent_text)).into()
}

fn fill(color: Color, radius: f32) -> theme::Container<'static> {
    theme::Container::custom(move |_| iced_container::Style {
        background: Some(Background::Color(color)),
        border: Border { radius: radius.into(), ..Default::default() },
        ..Default::default()
    })
}

fn track(radius: f32) -> theme::Container<'static> {
    theme::Container::custom(move |theme: &cosmic::Theme| {
        let mut color = Color::from(theme.cosmic().on_bg_color());
        color.a = 0.12;
        iced_container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        }
    })
}

fn neutral_fill(radius: f32) -> theme::Container<'static> {
    theme::Container::custom(move |theme: &cosmic::Theme| {
        let mut color = Color::from(theme.cosmic().on_bg_color());
        color.a = 0.45;
        iced_container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        }
    })
}

// A notice box: tinted background and a rounded border.
fn notice(color: Color) -> theme::Container<'static> {
    theme::Container::custom(move |_| {
        let mut back = color;
        back.a = 0.12;
        let mut edge = color;
        edge.a = 0.5;
        iced_container::Style {
            background: Some(Background::Color(back)),
            border: Border { radius: 8.0.into(), width: 1.0, color: edge },
            ..Default::default()
        }
    })
}

// The row of a device that is open: a light background.
fn selected_row() -> theme::Container<'static> {
    theme::Container::custom(|theme: &cosmic::Theme| {
        let mut color = Color::from(theme.cosmic().on_bg_color());
        color.a = 0.06;
        iced_container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 8.0.into(), ..Default::default() },
            ..Default::default()
        }
    })
}

// A rounded track with one fill (fraction 0..1).
fn bar<'a>(fraction: f64, class: theme::Container<'static>, height: f32) -> Element<'a, Message> {
    let part = (fraction.clamp(0.0, 1.0) * 1000.0).round() as u16;
    let mut line = Row::new().width(Length::Fill).height(Length::Fixed(height));
    if part > 0 {
        line = line.push(container(space::horizontal()).width(Length::FillPortion(part)).height(Length::Fill).class(class));
    }
    if part < 1000 {
        line = line.push(space::horizontal().width(Length::FillPortion(1000 - part)));
    }
    container(line).width(Length::Fill).class(track(height / 2.0)).into()
}

// Label left, value right, a bar underneath, an explanation below.
fn meter<'a>(name: String, value: String, fraction: f64, class: theme::Container<'static>, tip: String) -> Element<'a, Message> {
    let mut block = column![
        row![dim(name), space::horizontal().width(Length::Fill), text::caption(value)].align_y(Alignment::Center),
        bar(fraction, class, 6.0),
    ]
    .spacing(4);
    if !tip.is_empty() {
        block = block.push(dim(tip));
    }
    block.into()
}

fn section<'a>(title: &str) -> Element<'a, Message> {
    container(text::caption_heading(title.to_uppercase()).class(theme::Text::Custom(dim_text))).padding([6, 0, 0, 0]).into()
}

fn small_icon<'a>(name: &'static str, size: u16) -> Element<'a, Message> {
    icon::from_name(name).size(size).symbolic(true).icon().into()
}

// A small text action (icon + words).
fn link<'a>(icon_name: &'static str, label: String, message: Option<Message>) -> Element<'a, Message> {
    button::text(label).leading_icon(icon::from_name(icon_name).size(16).handle()).on_press_maybe(message).into()
}

// A choice of a row (tabs, grouping): the chosen one stands out.
fn choice<'a>(label: &'a str, chosen: bool) -> button::TextButton<'a, Message> {
    if chosen { button::suggested(label) } else { button::text(label) }
}

fn icon_button<'a>(icon_name: &'static str, message: Option<Message>) -> Element<'a, Message> {
    button::icon(icon::from_name(icon_name).size(16).handle()).extra_small().on_press_maybe(message).into()
}

// Four signal bars coloured like the panel line.
fn signal_bars<'a>(quality: f64) -> Element<'a, Message> {
    let mut bars = Row::new().spacing(2).align_y(Alignment::End);
    for (index, limit) in [0.0, 30.0, 50.0, 70.0].into_iter().enumerate() {
        let lit = quality > limit || (index == 0 && quality >= 0.0);
        let class = if lit { fill(signal_color(quality / 100.0), 1.0) } else { track(1.0) };
        bars = bars.push(
            container(space::horizontal())
                .width(Length::Fixed(3.0))
                .height(Length::Fixed(4.0 + index as f32 * 3.0))
                .class(class),
        );
    }
    bars.into()
}

// The radar icon, coloured (None: the theme's text colour), at `alpha`.
fn radar<'a>(size: f32, color: Option<Color>, alpha: f32) -> Element<'a, Message> {
    svg(iced_svg::Handle::from_memory(RADAR))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .class(theme::Svg::custom(move |theme: &cosmic::Theme| {
            let mut c = color.unwrap_or_else(|| Color::from(theme.cosmic().on_bg_color()));
            c.a *= alpha;
            iced_svg::Style { color: Some(c) }
        }))
        .into()
}

// ------------------------------------------------------------------------- list and map

enum Entry {
    Header { id: String, title: String, icon: &'static str, detail: String, count: usize, online: usize, has_new: bool, collapsed: bool },
    Device { index: usize, in_group: bool },
}

enum MapKind {
    Internet,
    Ap { id: String, title: String },
    Group { id: String, title: String, icon: &'static str, count: usize, online: usize },
    Device,
}

struct MapRow {
    depth: usize,
    kind: MapKind,
    dev: Option<usize>,
    collapsed: bool,
}

// ------------------------------------------------------------------------- applet

#[derive(Default)]
struct Monitor {
    core: Core,
    popup: Option<window::Id>,
    rectangle: Rectangle,
    tracker: Option<RectangleTracker<u32>>,
    commands: Option<UnboundedSender<String>>,
    stats: Value,
    devices: Vec<Value>,
    devices_rev: String,
    scan_busy: bool,      // the collector reported that a sweep is running
    rescan_pending: bool, // the user asked for a rescan, waiting for the result
    pulse_dim: bool,
    local_view: Option<Value>,
    expanded: String,
    editing: String,
    edit_draft: String,
    show_hidden: bool,
    filter: String,
    chooser: bool,
    fritz_form_open: bool,
    fritz_form_net: String,
    fritz_form_net_id: String,
    fritz_user: String,
    fritz_user_net: String,
    fritz_password: String,
    fritz_password_visible: bool,
    fritz_help_open: bool,
    flash: HashMap<String, std::time::Instant>,
}

#[derive(Debug, Clone)]
enum Message {
    Ready(UnboundedSender<String>),
    Line(Value),
    Exited,
    TogglePopup,
    CloseRequested(window::Id),
    Rectangle(RectangleUpdate<u32>),
    Pulse,
    Tick,
    Rescan,
    SetTrusted(bool),
    Alerts(bool),
    KnownAll,
    FritzLogout,
    DeviceCommand(&'static str, String, Value),
    ToggleGroup(String),
    SetViewMode(String),
    FoldAll(bool, Vec<String>),
    ToggleChooser,
    ToggleDevice(String),
    StartRename(String),
    RenameDraft(String),
    FinishRename,
    CancelRename,
    Filter(String),
    ToggleHidden,
    CopyIp(String),
    CopyList,
    CopyVpnCommand,
    OpenWeb(String),
    OpenWifiSettings,
    OpenDevice(usize),
    StartFritzLogin,
    FritzUser(String),
    FritzPassword(String),
    FritzPasswordVisible,
    FritzFocusPassword,
    FritzConnect,
    FritzCancel,
    FritzHelp,
}

impl Monitor {
    fn active(&self) -> bool {
        b(&self.stats, "active")
    }

    fn link(&self) -> &Value {
        self.stats.get("link").unwrap_or(&Value::Null)
    }

    fn fritz(&self) -> &Value {
        self.stats.get("fritz").unwrap_or(&Value::Null)
    }

    fn vpn(&self) -> Option<&Value> {
        self.link().get("vpn").filter(|v| v.is_object())
    }

    fn lan_blocked(&self) -> bool {
        self.vpn().is_some_and(|v| b(v, "lan_blocked"))
    }

    fn trusted(&self) -> bool {
        b(self.link(), "trusted")
    }

    fn scanning(&self) -> bool {
        self.scan_busy || self.rescan_pending
    }

    fn view(&self) -> Value {
        self.local_view
            .clone()
            .or_else(|| self.stats.get("view").filter(|v| v.is_object()).cloned())
            .unwrap_or_else(|| json!({"mode": "list", "collapsed": []}))
    }

    fn collapsed(&self) -> Vec<String> {
        arr(&self.view(), "collapsed").iter().filter_map(Value::as_str).map(str::to_string).collect()
    }

    fn mesh_active(&self) -> bool {
        b(&self.stats, "mesh_active")
    }

    fn view_mode(&self) -> String {
        let view = self.view();
        let mode = s(&view, "mode");
        if (mode == "ap" || mode == "band") && !self.mesh_active() {
            "platform".into()
        } else if mode.is_empty() {
            "list".into()
        } else {
            mode.into()
        }
    }

    fn view_modes(&self) -> &'static [&'static str] {
        if self.mesh_active() { &["list", "platform", "type", "band", "ap"] } else { &["list", "platform", "type"] }
    }

    fn map_page(&self) -> bool {
        self.collapsed().iter().any(|c| c == "mappage")
    }

    fn info_open(&self) -> bool {
        self.collapsed().iter().any(|c| c == "showinfo")
    }

    fn aps(&self) -> &[Value] {
        arr(self.fritz(), "aps")
    }

    fn gateway(&self) -> Option<&Value> {
        self.devices.iter().find(|d| b(d, "is_gateway"))
    }

    // "Allow it with: <command>" hints carry a command the user can copy.
    fn vpn_command(&self) -> String {
        let hint = self.vpn().map(|v| s(v, "hint")).unwrap_or("");
        hint.strip_prefix("Allow it with: ").unwrap_or("").to_string()
    }

    fn fritz_needs_login(&self) -> bool {
        let f = self.fritz();
        b(f, "available")
            && s(f, "state") == "error"
            && !s(f, "user").is_empty()
            && matches!(s(f, "message_code"), "fzCert" | "fzWrongLogin")
    }

    // The router the login form was opened for: its network id (new on every
    // gateway change), SSID, router address and MAC. The collector refuses a
    // login whose id is no longer current.
    fn network_id(&self) -> String {
        let link = self.link();
        json!([shown(link, "net_id"), s(link, "ssid"), s(link, "gateway"), shown(link, "gateway_mac")]).to_string()
    }

    fn send(&self, line: &str) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(line.to_string());
        }
    }

    fn send_json(&self, payload: Value) {
        self.send(&payload.to_string());
    }

    fn save_view(&mut self, mode: String, collapsed: Vec<String>) {
        let collapsed: Vec<String> = collapsed.into_iter().take(32).collect();
        let view = json!({"mode": mode, "collapsed": collapsed});
        self.send_json(json!({"cmd": "view", "mode": view["mode"], "collapsed": view["collapsed"]}));
        self.local_view = Some(view);
    }

    fn toggle_group(&mut self, id: &str) {
        let mut collapsed = self.collapsed();
        match collapsed.iter().position(|c| c == id) {
            Some(at) => {
                collapsed.remove(at);
            }
            None => collapsed.push(id.to_string()),
        }
        let mode = self.view_mode();
        self.save_view(mode, collapsed);
    }

    fn band_of(&self, dev: &Value) -> String {
        if let Some(band) = dev.get("mesh").map(|m| s(m, "band")).filter(|b| !b.is_empty()) {
            return band.to_string();
        }
        if b(dev, "is_self") && !s(self.link(), "via_band").is_empty() {
            return s(self.link(), "via_band").to_string();
        }
        String::new()
    }

    // The one grouping rule both pages use (list and access-point views have no groups here).
    fn group_of(&self, dev: &Value) -> String {
        if b(dev, "pinned") {
            return "favourites".into();
        }
        match self.view_mode().as_str() {
            "platform" => {
                let g = s(dev, "group");
                if g.is_empty() { "other".into() } else { g.into() }
            }
            "type" => kind_group(s(dev, "kind")).into(),
            "band" => {
                let band = self.band_of(dev);
                if !band.is_empty() {
                    band_group(&band).into()
                } else if b(dev, "online") {
                    "gother".into()
                } else {
                    "gsleep".into()
                }
            }
            _ => String::new(),
        }
    }

    fn group_id_of(&self, dev: &Value) -> String {
        if b(dev, "is_gateway") || b(dev, "is_self") {
            return String::new();
        }
        if self.view_mode() == "ap" {
            let via = dev.get("mesh").map(|m| s(m, "via_mac")).unwrap_or("");
            return if via.is_empty() { "none".into() } else { via.into() };
        }
        self.group_of(dev)
    }

    fn list_fold_ids(&self) -> Vec<String> {
        match self.view_mode().as_str() {
            "ap" => {
                let mut ids: Vec<String> = self.aps().iter().map(|ap| s(ap, "mac").to_string()).collect();
                ids.push("none".into());
                ids
            }
            "list" => Vec::new(),
            mode => order_for(mode).iter().map(|s| s.to_string()).collect(),
        }
    }

    fn map_fold_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = order_for(&self.view_mode()).iter().map(|s| s.to_string()).collect();
        for ap in self.aps() {
            if s(ap, "role") != "master" {
                ids.push(s(ap, "mac").to_string());
            }
        }
        ids
    }

    fn mesh_text(&self, dev: &Value) -> String {
        let Some(m) = dev.get("mesh").filter(|m| !s(m, "via").is_empty()) else { return String::new() };
        // With a single access point "via …" says nothing new; the band is enough.
        if self.aps().len() <= 1 {
            return if s(m, "band") == "LAN" { "cable".into() } else { s(m, "band").into() };
        }
        let band = s(m, "band");
        format!("via {}{}", s(m, "via"), if band.is_empty() { String::new() } else { format!(" · {band}") })
    }

    fn mesh_detail(&self, dev: &Value) -> String {
        let Some(m) = dev.get("mesh").filter(|m| m.is_object()) else { return String::new() };
        let band = s(m, "band");
        let rate = n(m, "rate_mbps");
        if band.is_empty() && rate <= 0.0 {
            return String::new();
        }
        let mut parts = Vec::new();
        if !band.is_empty() {
            parts.push(if band == "LAN" { "cable (LAN)".to_string() } else { band.to_string() });
        }
        if rate > 0.0 {
            parts.push(format!("{rate} Mbit/s"));
        }
        if let Some(dbm) = num(m, "signal_dbm") {
            parts.push(format!("signal {} ({dbm} dBm)", signal_word(signal_quality(Some(dbm))).to_lowercase()));
        }
        parts.join(" · ")
    }

    fn right_label(&self, dev: &Value) -> String {
        let word = status_word(dev);
        if word == "online" || word == "router" { s(dev, "ip").into() } else { format!("{word} · {}", s(dev, "ip")) }
    }

    fn subtitle(&self, dev: &Value) -> String {
        let kind = device_kind_label(dev);
        let name = s(dev, "name");
        let mut parts: Vec<String> = Vec::new();
        if !name.starts_with(kind) {
            parts.push(kind.into());
        }
        let model = s(dev, "model");
        if b(dev, "is_gateway") && !model.is_empty() && !name.contains(model) {
            parts.push(format!("model {model}"));
        }
        // The band is already the group heading when grouping by band.
        let mesh = self.mesh_text(dev);
        if !mesh.is_empty() && !(self.view_mode() == "band" && !b(dev, "is_self")) {
            parts.push(mesh);
        }
        let vendor = s(dev, "vendor");
        if !vendor.is_empty() && vendor != "Private MAC" && !name.contains(vendor) {
            parts.push(vendor.into());
        }
        if !s(dev, "custom_name").is_empty() && !s(dev, "auto_name").is_empty() {
            parts.push(s(dev, "auto_name").into());
        }
        parts.join("  ·  ")
    }

    fn visible_devices(&self) -> Vec<usize> {
        let query = self.filter.trim().to_lowercase();
        (0..self.devices.len())
            .filter(|&i| {
                let dev = &self.devices[i];
                if b(dev, "hidden") && !self.show_hidden {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                let via = dev.get("mesh").map(|m| s(m, "via")).unwrap_or("");
                [s(dev, "name"), s(dev, "auto_name"), s(dev, "ip"), s(dev, "vendor"), s(dev, "model"), s(dev, "hostname"), device_kind_label(dev), via]
                    .join(" ")
                    .to_lowercase()
                    .contains(&query)
            })
            .collect()
    }

    fn list_entries(&self) -> Vec<Entry> {
        let devs = self.visible_devices();
        let mode = self.view_mode();
        if mode == "list" || !self.filter.trim().is_empty() {
            return devs.into_iter().map(|index| Entry::Device { index, in_group: false }).collect();
        }
        let collapsed = self.collapsed();
        let mut order: Vec<String> = Vec::new();
        let mut buckets: HashMap<String, (String, &'static str, String, Vec<usize>)> = HashMap::new();
        if mode == "ap" {
            for ap in self.aps() {
                let mac = s(ap, "mac").to_string();
                let detail = if s(ap, "role") == "master" {
                    "main router".to_string()
                } else if !s(ap, "via").is_empty() {
                    let band = s(ap, "band");
                    format!("via {}{}", s(ap, "via"), if band.is_empty() { String::new() } else { format!(" {band}") })
                } else {
                    "repeater".to_string()
                };
                let name = if s(ap, "name").is_empty() { "Access point".to_string() } else { s(ap, "name").to_string() };
                order.push(mac.clone());
                buckets.insert(mac, (name, "network-wireless-symbolic", detail, Vec::new()));
            }
            order.push("none".into());
        } else {
            order = order_for(&mode).iter().map(|s| s.to_string()).collect();
        }
        let mut top = Vec::new();
        for index in devs {
            let dev = &self.devices[index];
            if b(dev, "is_gateway") || (b(dev, "is_self") && mode != "ap") {
                top.push(index);
                continue;
            }
            let id = if mode == "ap" {
                let via = dev.get("mesh").map(|m| s(m, "via_mac")).unwrap_or("");
                if buckets.contains_key(via) { via.to_string() } else { "none".to_string() }
            } else {
                self.group_of(dev)
            };
            buckets
                .entry(id.clone())
                .or_insert_with(|| {
                    let (title, icon) = group_info(&id);
                    (title.to_string(), icon, String::new(), Vec::new())
                })
                .3
                .push(index);
        }
        let mut out: Vec<Entry> = top.into_iter().map(|index| Entry::Device { index, in_group: false }).collect();
        for id in order {
            let Some((title, icon, detail, members)) = buckets.remove(&id) else { continue };
            if members.is_empty() {
                continue;
            }
            let is_collapsed = collapsed.contains(&id);
            out.push(Entry::Header {
                id: id.clone(),
                title,
                icon,
                detail,
                count: members.len(),
                online: members.iter().filter(|&&i| b(&self.devices[i], "online")).count(),
                has_new: members.iter().any(|&i| b(&self.devices[i], "new")),
                collapsed: is_collapsed,
            });
            // The user's choice always wins; a collapsed group with a new device shows NEW on its header.
            if !is_collapsed {
                out.extend(members.into_iter().map(|index| Entry::Device { index, in_group: true }));
            }
        }
        out
    }

    // The network as a tree: Internet › router › groups › devices, repeaters as their own branches.
    fn map_rows(&self) -> Vec<MapRow> {
        let devs: Vec<usize> = (0..self.devices.len()).filter(|&i| !b(&self.devices[i], "hidden")).collect();
        let collapsed = self.collapsed();
        let gateway = devs.iter().copied().find(|&i| b(&self.devices[i], "is_gateway"));
        let ap_macs: Vec<&str> = self.aps().iter().map(|ap| s(ap, "mac")).collect();
        let parent_of = |dev: &Value| -> String {
            let via = dev.get("mesh").map(|m| s(m, "via_mac")).unwrap_or("");
            if !via.is_empty() && ap_macs.contains(&via) {
                via.to_string()
            } else {
                gateway.map(|g| s(&self.devices[g], "mac").to_string()).unwrap_or_default()
            }
        };
        let is_repeater = |dev: &Value| {
            s(dev, "kind") == "router" && !b(dev, "is_gateway") && dev.get("mesh").is_some_and(|m| s(m, "role") == "slave")
        };
        let mut rows = vec![MapRow { depth: 0, kind: MapKind::Internet, dev: None, collapsed: false }];
        let mode = self.view_mode();
        // Under each router / repeater: its devices in the groups, then its repeaters.
        fn walk(
            this: &Monitor,
            rows: &mut Vec<MapRow>,
            devs: &[usize],
            ap: usize,
            depth: usize,
            mode: &str,
            collapsed: &[String],
            parent_of: &dyn Fn(&Value) -> String,
            is_repeater: &dyn Fn(&Value) -> bool,
        ) {
            let dev = &this.devices[ap];
            let mac = s(dev, "mac").to_string();
            let folded = !b(dev, "is_gateway") && collapsed.contains(&mac);
            rows.push(MapRow {
                depth,
                kind: MapKind::Ap { id: mac.clone(), title: s(dev, "name").to_string() },
                dev: Some(ap),
                collapsed: folded,
            });
            if folded {
                return;
            }
            let members: Vec<usize> = devs
                .iter()
                .copied()
                .filter(|&i| {
                    let d = &this.devices[i];
                    i != ap && !b(d, "is_gateway") && !is_repeater(d) && parent_of(d) == mac
                })
                .collect();
            if mode == "list" || mode == "ap" {
                for i in members {
                    rows.push(MapRow { depth: depth + 1, kind: MapKind::Device, dev: Some(i), collapsed: false });
                }
            } else {
                for id in order_for(mode) {
                    let group: Vec<usize> = members.iter().copied().filter(|&i| this.group_of(&this.devices[i]) == *id).collect();
                    if group.is_empty() {
                        continue;
                    }
                    let (title, icon) = group_info(id);
                    let folded = collapsed.iter().any(|c| c == id);
                    rows.push(MapRow {
                        depth: depth + 1,
                        kind: MapKind::Group {
                            id: id.to_string(),
                            title: title.into(),
                            icon,
                            count: group.len(),
                            online: group.iter().filter(|&&i| b(&this.devices[i], "online")).count(),
                        },
                        dev: None,
                        collapsed: folded,
                    });
                    if !folded {
                        for i in group {
                            rows.push(MapRow { depth: depth + 2, kind: MapKind::Device, dev: Some(i), collapsed: false });
                        }
                    }
                }
            }
            for &i in devs {
                let d = &this.devices[i];
                if is_repeater(d) && d.get("mesh").map(|m| s(m, "via_mac")) == Some(mac.as_str()) {
                    walk(this, rows, devs, i, depth + 1, mode, collapsed, parent_of, is_repeater);
                }
            }
        }
        if let Some(g) = gateway {
            walk(self, &mut rows, &devs, g, 1, &mode, &collapsed, &parent_of, &is_repeater);
        }
        rows
    }

    // ------------------------------------------------------------------ panel button

    fn panel_content(&self) -> Element<'_, Message> {
        let active = self.active();
        let link = self.link();
        let size = self.core.applet.suggested_size(true).0 as f32;
        let color = active.then(|| if self.vpn().is_some() { BLUE } else { GREEN });
        let alpha = if self.scanning() && self.pulse_dim {
            0.3
        } else if active || self.scanning() {
            1.0
        } else {
            0.6
        };
        let mut glyph = row![radar(size, color, alpha)].align_y(Alignment::Start);
        // Dot: devices joined recently and are not marked as known yet.
        if n(&self.stats, "new_count") > 0.0 {
            glyph = glyph.push(
                container(space::horizontal())
                    .width(Length::Fixed(5.0))
                    .height(Length::Fixed(5.0))
                    .class(fill(Color::from_rgb8(0xe0, 0x1b, 0x24), 3.0)),
            );
        }
        let quality = num(link, "quality").filter(|_| active);
        let line: Element<Message> = match quality {
            Some(q) => {
                let f = (q / 100.0).clamp(0.0, 1.0);
                container(bar(f.max(0.05), fill(signal_color(f), 1.0), 2.0)).width(Length::Fixed(size)).into()
            }
            None => space::vertical().height(Length::Fixed(2.0)).into(),
        };
        column![glyph, line].spacing(2).align_x(Alignment::Center).into()
    }

    // ------------------------------------------------------------------ popup

    fn hero(&self) -> Element<'_, Message> {
        let link = self.link();
        let active = self.active();
        let meta = if active && !s(link, "ssid").is_empty() {
            let mut m = s(link, "ssid").to_string();
            if !s(link, "band").is_empty() {
                m += &format!(" · {}", s(link, "band"));
            }
            if n(link, "width_mhz") > 0.0 {
                m += &format!(" ({} MHz)", shown(link, "width_mhz"));
            }
            m
        } else {
            "Not connected".into()
        };
        let color = active.then(|| if self.vpn().is_some() { BLUE } else { GREEN });
        let mut hero = row![
            radar(32.0, color, if active { 1.0 } else { 0.6 }),
            column![text::title4("WLAN Monitor"), dim(meta)].spacing(2),
            space::horizontal().width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if active {
            hero = hero.push(
                container(text::caption(format!("{} online", n(&self.stats, "online_count"))))
                    .padding([2, 10])
                    .class(track(10.0)),
            );
        }
        hero.into()
    }

    fn tabs(&self) -> Element<'_, Message> {
        let map = self.map_page();
        row![
            choice("Devices", !map)
                .leading_icon(icon::from_name("view-list-symbolic").size(16).handle())
                .on_press_maybe(map.then(|| Message::ToggleGroup("mappage".into()))),
            choice("Network map", map)
                .leading_icon(icon::from_name("network-workgroup-symbolic").size(16).handle())
                .on_press_maybe((!map).then(|| Message::ToggleGroup("mappage".into()))),
        ]
        .spacing(6)
        .into()
    }

    fn notices(&self) -> Vec<Element<'_, Message>> {
        let mut out = Vec::new();
        // A VPN blocks the home network: explain why everything looks idle, and how to fix it.
        if self.lan_blocked() {
            let vpn = self.vpn().unwrap_or(&Value::Null);
            let name = if s(vpn, "name").is_empty() { "A VPN" } else { s(vpn, "name") };
            let command = self.vpn_command();
            let hint = if command.is_empty() {
                "Allow local network (LAN) access in your VPN settings.".to_string()
            } else {
                format!("Allow it with: {command}.")
            };
            let mut block = column![
                row![small_icon("network-vpn-symbolic", 16), text::heading(format!("{name} is on and blocks your home network"))]
                    .spacing(8)
                    .align_y(Alignment::Center),
                dim(format!(
                    "Devices can't be reached, so they show as idle or sleeping, and the router page, names from the \
                     network and FRITZ!Box details are unavailable. {hint} Or disconnect the VPN."
                )),
            ]
            .spacing(6);
            if !command.is_empty() {
                let label = if self.flashing("vpn") { "Copied".to_string() } else { format!("Copy command: {command}") };
                block = block.push(link("edit-copy-symbolic", label, Some(Message::CopyVpnCommand)));
            }
            out.push(container(block).padding(10).width(Length::Fill).class(notice(BLUE)).into());
        }
        // The FRITZ!Box login needs to be typed again (new certificate, changed password).
        if self.fritz_needs_login() && !self.fritz_form_open && !self.lan_blocked() {
            let f = self.fritz();
            let code = if s(f, "message_code").is_empty() { "fzCert" } else { s(f, "message_code") };
            out.push(
                container(
                    column![
                        row![small_icon("dialog-warning-symbolic", 16), text::heading(fritz_message(code, arr(f, "message_args")))]
                            .spacing(8)
                            .align_y(Alignment::Center),
                        link("dialog-password-symbolic", "Connect FRITZ!Box again".into(), Some(Message::StartFritzLogin)),
                    ]
                    .spacing(6),
                )
                .padding(10)
                .width(Length::Fill)
                .class(notice(Color::from_rgb8(0xff, 0x78, 0x00)))
                .into(),
            );
        }
        out
    }

    fn connection(&self) -> Element<'_, Message> {
        let link = self.link();
        let traffic = self.stats.get("traffic").unwrap_or(&Value::Null);
        let active = self.active();
        let ip = if s(link, "ip").is_empty() { "—" } else { s(link, "ip") };
        let gateway = s(link, "gateway");
        let mut block = column![
            section("Connection"),
            row![
                text::heading(format!("This PC  {ip}")),
                space::horizontal().width(Length::Fill),
                dim(if gateway.is_empty() { String::new() } else { format!("Router  {gateway}") }),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(6);
        if active {
            let cidr = if !s(link, "cidr").is_empty() { s(link, "cidr") } else { ip };
            let iface = if s(link, "iface").is_empty() { "?" } else { s(link, "iface") };
            let mut parts = vec![format!("Address {cidr} on {iface}")];
            if !s(link, "mac").is_empty() {
                parts.push(format!("MAC {}", s(link, "mac")));
            }
            if !s(link, "domain").is_empty() {
                parts.push(format!("Local domain {}", s(link, "domain")));
            }
            block = block.push(dim(parts.join("  ·  ")));
        }
        if let Some(quality) = num(link, "quality") {
            let fraction = (quality / 100.0).clamp(0.0, 1.0);
            let mut tip = Vec::new();
            if let Some(dbm) = num(link, "signal_dbm") {
                tip.push(format!("{dbm} dBm"));
            }
            if !shown(link, "channel").is_empty() {
                tip.push(format!("channel {}", shown(link, "channel")));
            }
            if n(link, "tx_mbps") > 0.0 {
                let rx = shown(link, "rx_mbps");
                tip.push(format!("link {}{} Mbit/s", shown(link, "tx_mbps"), if rx.is_empty() { String::new() } else { format!(" / {rx}") }));
            }
            if quality < 30.0 {
                tip.push("move closer to the router or remove obstacles".into());
            }
            block = block.push(meter(
                format!("Signal  ·  {}", signal_word(quality)),
                format!("{} %", quality.round()),
                fraction,
                fill(signal_color(fraction), 3.0),
                tip.join("  ·  "),
            ));
        }
        let rate = n(traffic, "rx_rate") + n(traffic, "tx_rate");
        block = block.push(meter(
            "Traffic now".into(),
            format!("↓ {}  ·  ↑ {}", format_rate(num(traffic, "rx_rate")), format_rate(num(traffic, "tx_rate"))),
            if rate <= 0.0 { 0.0 } else { ((1.0 + rate / 1024.0).log10() / 4.0).min(1.0) },
            neutral_fill(3.0),
            format!("Since connecting: {} down, {} up", format_bytes(n(traffic, "rx_bytes")), format_bytes(n(traffic, "tx_bytes"))),
        ));
        // Always say when a VPN is on, also when it lets the home network through.
        if let Some(vpn) = self.vpn() {
            block = block.push(accent(format!(
                "{} is on  ·  {}",
                s(vpn, "name"),
                if self.lan_blocked() { "blocks your home network" } else { "home network reachable" }
            )));
        }
        if active {
            // One summary line; the explanations and the FRITZ!Box login open on click.
            let mut summary = if self.trusted() { "Your network".to_string() } else { "Not your network (passive)".to_string() };
            let via = s(link, "via");
            if !via.is_empty() {
                let via = via.strip_prefix("Router (").and_then(|v| v.strip_suffix(')')).unwrap_or(via);
                summary += &format!("  ·  via {via}");
                if !s(link, "via_band").is_empty() {
                    summary += &format!(" · {}", s(link, "via_band"));
                }
            }
            let info = row![
                small_icon(if self.trusted() { "security-high-symbolic" } else { "security-low-symbolic" }, 16),
                text::caption(summary).width(Length::Fill),
                small_icon(if self.info_open() { "pan-down-symbolic" } else { "pan-end-symbolic" }, 16),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            block = block.push(
                button::custom(info)
                    .padding([4, 6])
                    .width(Length::Fill)
                    .class(theme::Button::MenuItem)
                    .on_press(Message::ToggleGroup("showinfo".into())),
            );
            let fritz = self.fritz();
            let state = s(fritz, "state");
            if self.info_open() || self.fritz_form_open || state == "error" || state == "connecting" {
                let mut info = column![dim(if self.trusted() {
                    if self.stats.get("alerts").and_then(Value::as_bool) != Some(false) {
                        "Your network (trusted): looks for devices every 2 minutes and tells you when a new one joins."
                    } else {
                        "Your network (trusted): looks for devices every 2 minutes. New-device alerts are off."
                    }
                } else {
                    "Not marked as your network, so only devices that announce themselves are shown. If this is your \
                     home network, click Trust to find every device and get alerts about new ones."
                })]
                .spacing(6);
                if b(fritz, "available") {
                    info = info.push(self.fritz_block());
                }
                block = block.push(container(info).padding([0, 0, 0, 24]));
            }
        }
        block.into()
    }

    // Optional FRITZ!Box login for mesh details (which access point, band and speed).
    fn fritz_block(&self) -> Element<'_, Message> {
        let f = self.fritz();
        let state = s(f, "state");
        let user = s(f, "user");
        let mut links = Row::new().spacing(6).align_y(Alignment::Center);
        if state == "connected" {
            let count = self.aps().len();
            links = links.push(dim(if count == 1 {
                "FRITZ!Box connected · 1 access point".to_string()
            } else {
                format!("FRITZ!Box connected · {count} access points")
            }));
        }
        if state != "connected" && state != "connecting" && !self.fritz_form_open {
            let label = if state == "error" && !user.is_empty() { "Connect FRITZ!Box again" } else { "Show mesh details from the FRITZ!Box" };
            links = links.push(link("dialog-password-symbolic", label.into(), Some(Message::StartFritzLogin)));
        }
        if !user.is_empty() && !self.fritz_form_open {
            links = links.push(link("view-conceal-symbolic", "Disconnect".into(), Some(Message::FritzLogout)));
        }
        let mut block = column![links].spacing(6);
        if (state == "error" || state == "connecting") && !s(f, "message").is_empty() {
            let message = if s(f, "message_code").is_empty() {
                s(f, "message").to_string()
            } else {
                fritz_message(s(f, "message_code"), arr(f, "message_args"))
            };
            block = block.push(if state == "error" {
                text::caption(message).class(theme::Text::Custom(warn_text)).into()
            } else {
                dim(message)
            });
        }
        if self.fritz_form_open {
            // The password goes to the collector over its stdin pipe and is never shown again.
            let mut form = column![
                dim(FRITZ_EXPLAIN),
                text_input("User name, e.g. fritz1234", self.fritz_user.as_str())
                    .id(FRITZ_USER_ID.clone())
                    .on_input(|v| Message::FritzUser(v.chars().take(64).collect()))
                    .on_submit(|_| Message::FritzFocusPassword),
                link(
                    if self.fritz_help_open { "pan-down-symbolic" } else { "pan-end-symbolic" },
                    "Where do I find user name and password?".into(),
                    Some(Message::FritzHelp)
                ),
            ]
            .spacing(6);
            if self.fritz_help_open {
                form = form.push(container(dim(FRITZ_HELP)).padding([0, 0, 0, 12]));
            }
            form = form.push(
                secure_input("Password", self.fritz_password.as_str(), Some(Message::FritzPasswordVisible), !self.fritz_password_visible)
                    .id(FRITZ_PASSWORD_ID.clone())
                    .on_input(|v| Message::FritzPassword(v.chars().take(128).collect()))
                    .on_submit(|_| Message::FritzConnect),
            );
            form = form.push(
                row![
                    button::suggested("Connect").on_press_maybe((!self.fritz_password.is_empty()).then_some(Message::FritzConnect)),
                    button::standard("Cancel").on_press(Message::FritzCancel),
                ]
                .spacing(8),
            );
            block = block.push(form);
        }
        block.into()
    }

    fn chooser(&self) -> Element<'_, Message> {
        let current = self.view_mode();
        let mut chips = Row::new().spacing(4);
        for mode in self.view_modes() {
            chips = chips.push(
                choice(view_short(mode), *mode == current).on_press(Message::SetViewMode(mode.to_string())),
            );
        }
        chips.wrap().into()
    }

    fn devices_page(&self) -> Element<'_, Message> {
        let total = n(&self.stats, "total_count") as usize;
        let hidden = n(&self.stats, "hidden_count") as usize;
        let new_count = n(&self.stats, "new_count") as usize;
        let active = self.active();
        let mut page = column![self.connection(), divider::horizontal::default()].spacing(8);

        let foldable = self.view_mode() != "list" && self.filter.is_empty();
        let ids = self.list_fold_ids();
        page = page.push(
            row![
                text::caption_heading(format!("DEVICES ({total})")).class(theme::Text::Custom(dim_text)).width(Length::Fill),
                button::text(view_short(&self.view_mode()))
                    .trailing_icon(icon::from_name("pan-down-symbolic").size(16).handle())
                    .on_press(Message::ToggleChooser),
                icon_button("list-add-symbolic", foldable.then(|| Message::FoldAll(false, ids.clone()))),
                icon_button("list-remove-symbolic", foldable.then(|| Message::FoldAll(true, ids.clone()))),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        );
        if self.chooser {
            page = page.push(self.chooser());
        }
        // New devices since the user last checked (trusted networks only).
        if new_count > 0 {
            let joined = if new_count == 1 { "1 new device joined".to_string() } else { format!("{new_count} new devices joined") };
            page = page.push(
                container(
                    row![
                        small_icon("emblem-important-symbolic", 16),
                        text::caption(format!("{joined}. Not yours? Change your Wi-Fi password in the router.")).width(Length::Fill),
                        link("object-select-symbolic", "All known".into(), Some(Message::KnownAll)),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding(8)
                .class(notice(Color::from_rgb8(0xff, 0x78, 0x00))),
            );
        }
        if n(&self.stats, "named_count") == 0.0 && self.devices.len() > 1 && new_count == 0 {
            page = page.push(dim(
                "Tip: click a device to see what it is, give it a name you recognise, pin it to the top or open its web page.",
            ));
        }
        if total.saturating_sub(hidden) > 12 || !self.filter.is_empty() {
            page = page.push(
                text_input("Search devices", self.filter.as_str())
                    .id(SEARCH_ID.clone())
                    .leading_icon(small_icon("system-search-symbolic", 16))
                    .on_input(|v| Message::Filter(v.chars().take(40).collect())),
            );
        }
        if active && !self.trusted() && total <= 3 {
            page = page.push(dim("Only a few devices visible. Click Rescan to look for more."));
        }

        let entries = self.list_entries();
        let mut list = column![].spacing(2);
        for entry in &entries {
            list = list.push(match entry {
                Entry::Header { .. } => self.group_header(entry),
                Entry::Device { index, in_group } => {
                    let row = self.device_row(*index);
                    if *in_group { container(row).padding([0, 0, 0, 10]).into() } else { row }
                }
            });
        }
        if entries.is_empty() && !self.filter.is_empty() {
            list = list.push(dim(format!("No device matches “{}”.", self.filter)));
        }
        page = page.push(container(scrollable(list).height(Length::Shrink)).max_height(LIST_MAX_HEIGHT));

        let mut footer = Row::new().spacing(6);
        if !self.devices.is_empty() {
            footer = footer.push(link(
                "edit-copy-symbolic",
                if self.flashing("list") { "Copied".into() } else { "Copy list".into() },
                Some(Message::CopyList),
            ));
        }
        if hidden > 0 {
            footer = footer.push(link(
                if self.show_hidden { "view-conceal-symbolic" } else { "view-reveal-symbolic" },
                if self.show_hidden { format!("Hide {hidden} hidden") } else { format!("Show {hidden} hidden") },
                Some(Message::ToggleHidden),
            ));
        }
        page = page.push(footer);
        page.into()
    }

    fn group_header<'a>(&'a self, entry: &Entry) -> Element<'a, Message> {
        let Entry::Header { id, title, icon, detail, count, online, has_new, collapsed } = entry else {
            return space::horizontal().into();
        };
        let mut count_text = String::new();
        if *has_new {
            count_text += "NEW  ";
        }
        count_text += &count.to_string();
        if online != count {
            count_text += &format!(" · {online} online");
        }
        let title = if detail.is_empty() { title.clone() } else { format!("{title}  ·  {detail}") };
        let content = row![
            small_icon(icon, 16),
            text::heading(title).width(Length::Fill),
            if *has_new { accent(count_text) } else { dim(count_text) },
            small_icon(if *collapsed { "pan-end-symbolic" } else { "pan-down-symbolic" }, 16),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        button::custom(content)
            .padding([6, 6])
            .width(Length::Fill)
            .class(theme::Button::MenuItem)
            .on_press(Message::ToggleGroup(id.clone()))
            .into()
    }

    // One device row; a click on it toggles the details.
    fn device_row(&self, index: usize) -> Element<'_, Message> {
        let dev = &self.devices[index];
        let key = device_key(dev);
        let online = b(dev, "online");
        let expanded = self.expanded == key;
        let editing = self.editing == key && b(dev, "can_edit");
        let web_url = s(dev, "web_url");
        let mut block = column![].spacing(4);

        if editing {
            block = block.push(
                row![
                    small_icon(kind_icon(s(dev, "kind")), 18),
                    text_input(if s(dev, "auto_name").is_empty() { "Device name" } else { s(dev, "auto_name") }, self.edit_draft.as_str())
                        .id(RENAME_ID.clone())
                        .on_input(|v| Message::RenameDraft(v.chars().take(40).collect()))
                        .on_submit(|_| Message::FinishRename),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
            block = block.push(container(dim("Enter saves · empty restores the automatic name")).padding([0, 0, 0, 26]));
        } else {
            let mut main = row![small_icon(kind_icon(s(dev, "kind")), 18)].spacing(8).align_y(Alignment::Center);
            let name = s(dev, "name").to_string();
            let name: Element<Message> = if online {
                text::heading(name).width(Length::Fill).into()
            } else {
                text::body(name).class(theme::Text::Custom(dim_text)).width(Length::Fill).into()
            };
            main = main.push(name);
            if b(dev, "new") {
                main = main.push(accent("NEW"));
            }
            if b(dev, "pinned") {
                main = main.push(small_icon("view-pin-symbolic", 14));
            }
            let right = if self.flashing(&format!("ip:{key}")) { "copied to clipboard".to_string() } else { self.right_label(dev) };
            main = main.push(if b(dev, "is_gateway") || b(dev, "is_self") { accent(right) } else { dim(right) });
            let mut top = row![
                button::custom(main)
                    .padding([4, 6])
                    .width(Length::Fill)
                    .class(theme::Button::MenuItem)
                    .on_press(Message::ToggleDevice(key.clone()))
            ]
            .spacing(2)
            .align_y(Alignment::Center);
            if !web_url.is_empty() {
                top = top.push(icon_button("web-browser-symbolic", Some(Message::OpenWeb(web_url.to_string()))));
            }
            if !s(dev, "ip").is_empty() {
                top = top.push(icon_button(
                    if self.flashing(&format!("ip:{key}")) { "object-select-symbolic" } else { "edit-copy-symbolic" },
                    Some(Message::CopyIp(key.clone())),
                ));
            }
            block = block.push(top);
            let subtitle = self.subtitle(dev);
            if !subtitle.is_empty() {
                block = block.push(container(dim(subtitle)).padding([0, 0, 0, 32]));
            }
        }

        // Expanded details: what the device is, in plain words, plus actions.
        if expanded {
            let mut details = column![text::caption(status_explain(dev))].spacing(3);
            let via = dev.get("mesh").map(|m| s(m, "via")).unwrap_or("");
            let vendor = s(dev, "vendor");
            let hostname = s(dev, "hostname");
            for (name, value) in [
                ("Connected via", if !via.is_empty() && !b(dev, "is_gateway") { via.to_string() } else { String::new() }),
                ("Connection", self.mesh_detail(dev)),
                ("Maker", if vendor == "Private MAC" { "unknown (hidden by a private address)".into() } else { vendor.to_string() }),
                ("Model", s(dev, "model").to_string()),
                ("Automatic name", if s(dev, "custom_name").is_empty() { String::new() } else { s(dev, "auto_name").to_string() }),
                ("Network name", if !hostname.is_empty() && hostname != s(dev, "name") { hostname.to_string() } else { String::new() }),
                ("IP address", s(dev, "ip").to_string()),
                ("MAC address", s(dev, "mac").to_string()),
            ] {
                if !value.is_empty() {
                    details = details.push(dim(format!("{name}:  {value}")));
                }
            }
            let can_edit = b(dev, "can_edit");
            let editable = can_edit && !b(dev, "is_gateway") && !b(dev, "is_self");
            let mac = s(dev, "mac").to_string();
            let mut actions = Row::new().spacing(2);
            if can_edit && !editing {
                actions = actions.push(link("document-edit-symbolic", "Rename".into(), Some(Message::StartRename(key.clone()))));
            }
            if editing {
                actions = actions.push(link("object-select-symbolic", "Save".into(), Some(Message::FinishRename)));
                actions = actions.push(link("window-close-symbolic", "Cancel".into(), Some(Message::CancelRename)));
            }
            if editable && !editing {
                let pinned = b(dev, "pinned");
                actions = actions.push(link(
                    "view-pin-symbolic",
                    if pinned { "Unpin".into() } else { "Pin to top".into() },
                    Some(Message::DeviceCommand("pin", mac.clone(), json!({"on": !pinned}))),
                ));
                let hidden = b(dev, "hidden");
                actions = actions.push(link(
                    if hidden { "view-reveal-symbolic" } else { "view-conceal-symbolic" },
                    if hidden { "Show".into() } else { "Hide".into() },
                    Some(Message::DeviceCommand("hide", mac.clone(), json!({"on": !hidden}))),
                ));
            }
            if !web_url.is_empty() && !editing {
                actions = actions.push(link("web-browser-symbolic", "Open web page".into(), Some(Message::OpenWeb(web_url.to_string()))));
            }
            if b(dev, "new") && !editing {
                actions = actions.push(link(
                    "object-select-symbolic",
                    "It's mine".into(),
                    Some(Message::DeviceCommand("known", mac.clone(), json!({"on": true}))),
                ));
            }
            details = details.push(actions.wrap());
            if !can_edit {
                details = details.push(dim(
                    "This device's hardware address is unknown, so it cannot be renamed yet. Try again when it is online.",
                ));
            }
            block = block.push(container(details).padding([0, 6, 6, 32]));
        }
        if expanded { container(block).class(selected_row()).into() } else { block.into() }
    }

    fn map_page_view(&self) -> Element<'_, Message> {
        let n_devices = self.devices.iter().filter(|d| !b(d, "hidden") && !b(d, "is_gateway")).count();
        let usable = self.view_mode() != "list" && self.view_mode() != "ap";
        let ids = self.map_fold_ids();
        let mut page = column![
            row![
                dim(if n_devices == 1 { "1 device".to_string() } else { format!("{n_devices} devices") }),
                space::horizontal().width(Length::Fill),
                button::text(view_short(&self.view_mode()))
                    .trailing_icon(icon::from_name("pan-down-symbolic").size(16).handle())
                    .on_press(Message::ToggleChooser),
                icon_button("list-add-symbolic", usable.then(|| Message::FoldAll(false, ids.clone()))),
                icon_button("list-remove-symbolic", usable.then(|| Message::FoldAll(true, ids.clone()))),
            ]
            .spacing(4)
            .align_y(Alignment::Center)
        ]
        .spacing(8);
        if self.chooser {
            page = page.push(self.chooser());
        }
        let mut tree = column![].spacing(1);
        for row_data in self.map_rows() {
            tree = tree.push(self.map_row(row_data));
        }
        page = page.push(container(scrollable(tree).height(Length::Shrink)).max_height(LIST_MAX_HEIGHT + 120.0));
        if !self.mesh_active() {
            page = page.push(dim(if b(self.fritz(), "available") {
                "Connect your FRITZ!Box (Devices page, click the network line) to see the band, speed and signal of every device."
            } else {
                "Band, speed and signal per device are shown for FRITZ!Box routers after an optional login."
            }));
        }
        page = page.push(dim(
            "Click a group or repeater to fold it, a device to see its details. The grouping is shared with the Devices page.",
        ));
        page.into()
    }

    fn map_row(&self, r: MapRow) -> Element<'_, Message> {
        let dev = r.dev.map(|i| &self.devices[i]);
        let (glyph, label, strong, online, message): (&'static str, String, bool, bool, Option<Message>) = match &r.kind {
            MapKind::Internet => {
                let via = self.vpn().map(|v| format!("  ·  via {}", s(v, "name"))).unwrap_or_default();
                ("web-browser-symbolic", format!("Internet{via}"), true, true, None)
            }
            MapKind::Ap { id, title } => {
                let d = dev.unwrap_or(&Value::Null);
                let model = s(d, "model");
                let title = if title.is_empty() { "Router".to_string() } else { title.clone() };
                let label = if b(d, "is_gateway") && !model.is_empty() { format!("{title}  ·  {model}") } else { title };
                let message =
                    if b(d, "is_gateway") { r.dev.map(Message::OpenDevice) } else { Some(Message::ToggleGroup(id.clone())) };
                (kind_icon("router"), label, true, true, message)
            }
            MapKind::Group { id, title, icon, count, online } => (
                icon,
                format!("{title}  ·  {}", if *count == 1 { "1 device".to_string() } else { format!("{count} devices") }),
                true,
                *online > 0,
                Some(Message::ToggleGroup(id.clone())),
            ),
            MapKind::Device => {
                let d = dev.unwrap_or(&Value::Null);
                (kind_icon(s(d, "kind")), s(d, "name").to_string(), b(d, "is_self"), b(d, "online"), r.dev.map(Message::OpenDevice))
            }
        };
        let mut content = row![small_icon(glyph, if matches!(r.kind, MapKind::Internet | MapKind::Ap { .. }) { 18 } else { 16 })]
            .spacing(8)
            .align_y(Alignment::Center);
        let label: Element<Message> = if strong {
            text::heading(label).width(Length::Fill).into()
        } else if online {
            text::caption(label).width(Length::Fill).into()
        } else {
            text::caption(label).class(theme::Text::Custom(dim_text)).width(Length::Fill).into()
        };
        content = content.push(label);
        if let (MapKind::Device, Some(d)) = (&r.kind, dev) {
            if b(d, "new") {
                content = content.push(accent("NEW"));
            }
            let mesh = d.get("mesh").unwrap_or(&Value::Null);
            let detail = if n(mesh, "rate_mbps") > 0.0 {
                format!("{} Mbit/s", shown(mesh, "rate_mbps"))
            } else if !b(d, "online") {
                status_word(d)
            } else {
                String::new()
            };
            if !detail.is_empty() {
                content = content.push(dim(detail));
            }
            let quality = signal_quality(num(mesh, "signal_dbm"));
            if quality >= 0.0 {
                content = content.push(signal_bars(quality));
            }
        }
        let foldable = matches!(r.kind, MapKind::Group { .. }) || (matches!(r.kind, MapKind::Ap { .. }) && dev.is_some_and(|d| !b(d, "is_gateway")));
        if foldable {
            content = content.push(small_icon(if r.collapsed { "pan-end-symbolic" } else { "pan-down-symbolic" }, 14));
        }
        // The tree: indentation and an elbow per level.
        let lead = if r.depth == 0 { String::new() } else { "└ ".to_string() };
        let main = row![dim(lead), content].align_y(Alignment::Center);
        let main: Element<Message> = match message {
            Some(m) => button::custom(main).padding([3, 6]).width(Length::Fill).class(theme::Button::MenuItem).on_press(m).into(),
            None => container(main).padding([3, 6]).into(),
        };
        container(main).padding([0, 0, 0, (r.depth.saturating_sub(1) * 18) as u16]).into()
    }

    fn footer(&self) -> Element<'_, Message> {
        let gateway_url = self.gateway().map(|g| s(g, "web_url")).filter(|u| !u.is_empty());
        let mut buttons = Row::new().spacing(6).align_y(Alignment::Center);
        buttons = buttons.push(
            button::standard("Rescan")
                .leading_icon(icon::from_name("view-refresh-symbolic").size(16).handle())
                .on_press_maybe((!self.scanning()).then_some(Message::Rescan)),
        );
        if let Some(url) = gateway_url {
            buttons = buttons.push(
                button::standard("Router")
                    .leading_icon(icon::from_name("network-wireless-symbolic").size(16).handle())
                    .on_press(Message::OpenWeb(url.to_string())),
            );
        }
        if self.active() {
            let trusted = self.trusted();
            buttons = buttons.push(
                button::standard(if trusted { "Untrust" } else { "Trust" })
                    .leading_icon(
                        icon::from_name(if trusted { "security-low-symbolic" } else { "security-high-symbolic" }).size(16).handle(),
                    )
                    .on_press_maybe(b(self.link(), "can_trust").then_some(Message::SetTrusted(!trusted))),
            );
        }
        buttons = buttons.push(space::horizontal().width(Length::Fill));
        buttons = buttons.push(icon_button("preferences-system-symbolic", Some(Message::OpenWifiSettings)));
        let mut footer = column![buttons].spacing(8);
        // New-device alerts (trusted networks): a switch at the bottom, as in the GNOME menu.
        if self.active() && self.trusted() {
            footer = footer.push(
                toggler(self.stats.get("alerts").and_then(Value::as_bool) != Some(false))
                    .label("New-device alerts".to_string())
                    .on_toggle(Message::Alerts),
            );
        }
        footer.into()
    }

    fn panel(&self) -> Element<'_, Message> {
        let mut content = column![self.hero(), self.tabs()].spacing(10);
        for item in self.notices() {
            content = content.push(item);
        }
        content = content.push(divider::horizontal::default());
        content = content.push(if self.map_page() { self.map_page_view() } else { self.devices_page() });
        content = content.push(divider::horizontal::default());
        content = content.push(self.footer());
        content.into()
    }

    fn flashing(&self, key: &str) -> bool {
        self.flash.get(key).is_some_and(|at| at.elapsed() < FLASH)
    }

    // The device list as a Markdown table (text from the collector is already
    // sanitized; `|` is replaced so cells stay intact).
    fn list_markdown(&self) -> String {
        let cell = |v: &str| v.replace(['|', '\r', '\n'], " ").trim().to_string();
        let now = std::process::Command::new("date")
            .arg("+%x %X")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let ssid = s(self.link(), "ssid");
        let mut lines = vec![
            format!("# Devices on {} ({now})", cell(if ssid.is_empty() { "the network" } else { ssid })),
            String::new(),
            "| Name | Type | Status | IP | Maker | Model | MAC |".into(),
            "| --- | --- | --- | --- | --- | --- | --- |".into(),
        ];
        for dev in self.devices.iter().take(256) {
            let status = status_word(dev) + if b(dev, "hidden") { " (hidden)" } else { "" };
            let cells = [s(dev, "name"), kind_label(s(dev, "kind")), &status, s(dev, "ip"), s(dev, "vendor"), s(dev, "model"), s(dev, "mac")];
            lines.push(format!("| {} |", cells.iter().map(|c| cell(c)).collect::<Vec<_>>().join(" | ")));
        }
        lines.join("\n") + "\n"
    }

    fn close_fritz_form(&mut self) {
        self.fritz_password.clear();
        self.fritz_password_visible = false;
        self.fritz_form_open = false;
        self.fritz_form_net.clear();
        self.fritz_form_net_id.clear();
    }

    fn popup_closed(&mut self) {
        self.send("closed");
        self.chooser = false;
        if !self.editing.is_empty() {
            self.editing.clear();
            self.devices_rev.clear();
        }
        self.close_fritz_form();
    }

    fn take_snapshot(&mut self, stats: Value) {
        self.stats = stats;
        // A login form opened for another network is dropped with what was typed.
        if self.fritz_form_open && self.fritz_form_net != self.network_id() {
            self.close_fritz_form();
            self.fritz_user.clear();
        }
        // A stored view from the collector replaces the local copy once it caught up.
        if self.local_view.is_some() && self.stats.get("view") == self.local_view.as_ref() {
            self.local_view = None;
        }
        // Only replace the device list when it changed, and never while a name is
        // being edited.
        if self.editing.is_empty() {
            let rev = shown(&self.stats, "devices_rev");
            if rev.is_empty() || rev != self.devices_rev {
                self.devices_rev = rev;
                self.devices = arr(&self.stats, "devices").to_vec();
            }
        }
    }

    fn copy(&mut self, flash: String, value: String) -> Task<Message> {
        let value: String = value.chars().take(CLIPBOARD_LIMIT).collect();
        if value.is_empty() {
            return Task::none();
        }
        self.flash.insert(flash, std::time::Instant::now());
        cosmic::iced::clipboard::write(value)
    }
}

impl cosmic::Application for Monitor {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        (Self { core, ..Default::default() }, Task::none())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            rectangle_tracker_subscription(0).map(|update| Message::Rectangle(update.1)),
            Subscription::run_with("wlan-monitor-collector", |_| {
                stream::channel(8, |mut output| async move {
                    run_collector(&mut output).await;
                })
            }),
        ];
        if self.scanning() {
            subscriptions.push(cosmic::iced::time::every(Duration::from_millis(650)).map(|_| Message::Pulse));
        }
        if !self.flash.is_empty() {
            subscriptions.push(cosmic::iced::time::every(Duration::from_millis(500)).map(|_| Message::Tick));
        }
        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Ready(commands) => {
                self.commands = Some(commands);
                if self.popup.is_some() {
                    self.send("open");
                }
            }
            Message::Line(data) => {
                if s(&data, "event") == "scanning" {
                    self.scan_busy = true;
                } else if data.get("error").is_none() {
                    self.scan_busy = false;
                    self.rescan_pending = false;
                    self.take_snapshot(data);
                }
            }
            Message::Exited => {
                self.commands = None;
                self.scan_busy = false;
                self.rescan_pending = false;
            }
            Message::Pulse => self.pulse_dim = !self.pulse_dim,
            Message::Tick => self.flash.retain(|_, at| at.elapsed() < FLASH),
            Message::Rectangle(update) => match update {
                RectangleUpdate::Rectangle(r) => self.rectangle = r.1,
                RectangleUpdate::Init(tracker) => self.tracker = Some(tracker),
            },
            Message::CloseRequested(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                    self.popup_closed();
                }
            }
            Message::TogglePopup => {
                if let Some(popup) = self.popup.take() {
                    self.popup_closed();
                    return destroy_popup(popup);
                }
                self.send("open");
                return cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                    |_| Default::default(),
                    |app: &mut Self| {
                        let id = window::Id::unique();
                        app.popup = Some(id);
                        let mut settings =
                            app.core.applet.get_popup_settings(app.core.main_window_id().unwrap(), id, None, None, None);
                        let Rectangle { x, y, width, height } = app.rectangle;
                        settings.positioner.anchor_rect = Rectangle::<i32> {
                            x: x.max(1.0) as i32,
                            y: y.max(1.0) as i32,
                            width: width.max(1.0) as i32,
                            height: height.max(1.0) as i32,
                        };
                        settings.positioner.size = None;
                        settings
                    },
                    None,
                ));
            }
            Message::Rescan => {
                if !self.rescan_pending {
                    self.rescan_pending = true;
                    self.send("rescan");
                }
            }
            Message::SetTrusted(on) => self.send(if on { "trust" } else { "untrust" }),
            Message::Alerts(on) => self.send_json(json!({"cmd": "alerts", "on": on})),
            Message::KnownAll => self.send("known_all"),
            Message::FritzLogout => self.send("fritz_logout"),
            Message::DeviceCommand(cmd, mac, extra) => {
                if !mac.is_empty() {
                    let mut payload = json!({"cmd": cmd, "mac": mac});
                    if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
                        p.extend(e.clone());
                    }
                    self.send_json(payload);
                    if cmd == "hide" && extra.get("on") == Some(&Value::Bool(true)) {
                        self.expanded.clear();
                    }
                }
            }
            Message::ToggleGroup(id) => self.toggle_group(&id),
            Message::SetViewMode(mode) => {
                self.chooser = false;
                if self.view_modes().contains(&mode.as_str()) && mode != self.view_mode() {
                    let collapsed = self.collapsed();
                    self.save_view(mode, collapsed);
                }
            }
            Message::FoldAll(fold, ids) => {
                let mut collapsed: Vec<String> = self.collapsed().into_iter().filter(|c| !ids.contains(c)).collect();
                if fold {
                    collapsed.extend(ids);
                }
                let mode = self.view_mode();
                self.save_view(mode, collapsed);
            }
            Message::ToggleChooser => self.chooser = !self.chooser,
            Message::ToggleDevice(key) => {
                self.editing.clear();
                self.expanded = if self.expanded == key { String::new() } else { key };
            }
            Message::StartRename(key) => {
                let draft = self.devices.iter().find(|d| device_key(d) == key).map(|d| s(d, "name").to_string());
                self.expanded = key.clone();
                self.editing = key;
                self.edit_draft = draft.unwrap_or_default();
                return cosmic::widget::text_input::focus(RENAME_ID.clone());
            }
            Message::RenameDraft(text) => self.edit_draft = text,
            Message::FinishRename => {
                let dev = self.devices.iter().find(|d| device_key(d) == self.editing && b(d, "can_edit"));
                if let Some(dev) = dev {
                    let name: String = self.edit_draft.trim().chars().take(40).collect();
                    self.send_json(json!({"cmd": "rename", "mac": s(dev, "mac"), "name": name}));
                }
                self.editing.clear();
                self.edit_draft.clear();
                // The list was frozen while editing: take the newest one now.
                self.devices_rev.clear();
                let stats = self.stats.clone();
                self.take_snapshot(stats);
            }
            Message::CancelRename => {
                self.editing.clear();
                self.edit_draft.clear();
                self.devices_rev.clear();
                let stats = self.stats.clone();
                self.take_snapshot(stats);
            }
            Message::Filter(text) => self.filter = text,
            Message::ToggleHidden => self.show_hidden = !self.show_hidden,
            Message::CopyIp(key) => {
                let ip = self.devices.iter().find(|d| device_key(d) == key).map(|d| s(d, "ip").to_string()).unwrap_or_default();
                if is_ipv4(&ip) {
                    return self.copy(format!("ip:{key}"), ip);
                }
            }
            Message::CopyList => {
                let text = self.list_markdown();
                return self.copy("list".into(), text);
            }
            Message::CopyVpnCommand => {
                let command = self.vpn_command();
                return self.copy("vpn".into(), command);
            }
            Message::OpenWeb(url) => {
                if is_web_url(&url) {
                    spawn_detached(&["xdg-open", &url]);
                    if let Some(popup) = self.popup.take() {
                        self.popup_closed();
                        return destroy_popup(popup);
                    }
                }
            }
            Message::OpenWifiSettings => {
                spawn_detached(&["cosmic-settings", "wireless"]);
                if let Some(popup) = self.popup.take() {
                    self.popup_closed();
                    return destroy_popup(popup);
                }
            }
            Message::OpenDevice(index) => {
                // From the map: back to the Devices page, open the device's group, expand and show it.
                if let Some(dev) = self.devices.get(index).cloned() {
                    let drop = ["mappage".to_string(), self.group_id_of(&dev)];
                    if b(&dev, "hidden") {
                        self.show_hidden = true;
                    }
                    self.filter.clear();
                    self.expanded = device_key(&dev);
                    let collapsed = self.collapsed().into_iter().filter(|c| !drop.contains(c)).collect();
                    let mode = self.view_mode();
                    self.save_view(mode, collapsed);
                }
            }
            Message::StartFritzLogin => {
                if self.map_page() {
                    self.toggle_group("mappage"); // the form lives on the Devices page
                }
                let net = self.network_id();
                // A user name typed for another router (form cancelled there) is not offered here.
                if self.fritz_user_net != net {
                    self.fritz_user.clear();
                }
                self.fritz_user_net = net.clone();
                self.fritz_form_open = true;
                self.fritz_form_net = net;
                self.fritz_form_net_id = shown(self.link(), "net_id");
                self.fritz_password_visible = false;
                if self.fritz_user.is_empty() {
                    self.fritz_user = s(self.fritz(), "user").to_string();
                }
                return cosmic::widget::text_input::focus(if self.fritz_user.is_empty() {
                    FRITZ_USER_ID.clone()
                } else {
                    FRITZ_PASSWORD_ID.clone()
                });
            }
            Message::FritzUser(text) => self.fritz_user = text,
            Message::FritzPassword(text) => self.fritz_password = text,
            Message::FritzPasswordVisible => self.fritz_password_visible = !self.fritz_password_visible,
            Message::FritzFocusPassword => return cosmic::widget::text_input::focus(FRITZ_PASSWORD_ID.clone()),
            Message::FritzConnect => {
                // Typed for another router: the network changed while the form was open.
                if !self.fritz_password.is_empty() && !self.fritz_form_net_id.is_empty() && self.fritz_form_net == self.network_id() {
                    self.send_json(json!({
                        "cmd": "fritz_login",
                        "user": self.fritz_user.trim(),
                        "password": self.fritz_password,
                        "net": self.fritz_form_net_id,
                    }));
                }
                self.close_fritz_form();
            }
            Message::FritzCancel => self.close_fritz_form(),
            Message::FritzHelp => self.fritz_help_open = !self.fritz_help_open,
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = matches!(self.core.applet.anchor, PanelAnchor::Top | PanelAnchor::Bottom);
        let padding = self.core.applet.suggested_padding(true).0;
        let button = button::custom(self.panel_content())
            .padding(if horizontal { [0, padding] } else { [padding, 0] })
            .on_press_down(Message::TogglePopup)
            .class(theme::Button::AppletIcon);
        // Right click: an immediate rescan.
        let button = mouse_area(button).on_right_press(Message::Rescan);
        let content: Element<Message> = match self.tracker.as_ref() {
            Some(tracker) => tracker.container(0, button).ignore_bounds(true).into(),
            None => button.into(),
        };
        autosize::autosize(content, AUTOSIZE_ID.clone()).into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let body = scrollable(container(self.panel()).padding([12, 16])).height(Length::Shrink);
        let content = column![container(body).max_height(POPUP_MAX_HEIGHT)].width(Length::Fixed(POPUP_WIDTH));
        self.core.applet.popup_container(content).into()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

// ------------------------------------------------------------------------- processes

// Next to the binary (installed: ../share/cosmic-wlan-monitor/scripts), or in
// the data home; WLAN_MONITOR_SCRIPTS wins (development).
fn script_path(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("WLAN_MONITOR_SCRIPTS") {
        dirs.push(dir.into());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(bin) = exe.parent()
    {
        dirs.push(bin.join("../share/cosmic-wlan-monitor/scripts"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(data) = data_home {
        dirs.push(data.join("cosmic-wlan-monitor/scripts"));
    }
    dirs.push("/usr/share/cosmic-wlan-monitor/scripts".into());
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

// Streams the collector's lines and passes the app's commands to its stdin
// (one per line, in order); starts it again RESTART_DELAY after it exits.
async fn run_collector(output: &mut mpsc::Sender<Message>) {
    loop {
        if let Some(script) = script_path("wlan-monitor.py") {
            let child = tokio::process::Command::new("python3")
                .arg("-I")
                .arg(&script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            if let Ok(mut child) = child {
                let (tx, mut rx) = unbounded_channel::<String>();
                if output.send(Message::Ready(tx)).await.is_err() {
                    return;
                }
                let mut stdin = child.stdin.take();
                if let Some(stdout) = child.stdout.take() {
                    let mut lines = BufReader::new(stdout).lines();
                    loop {
                        tokio::select! {
                            line = lines.next_line() => {
                                let Ok(Some(line)) = line else { break };
                                // A partial line while the collector exits is skipped.
                                if let Ok(data) = serde_json::from_str::<Value>(&line)
                                    && output.send(Message::Line(data)).await.is_err()
                                {
                                    return;
                                }
                            }
                            Some(command) = rx.recv() => {
                                if let Some(pipe) = stdin.as_mut()
                                    && pipe.write_all(format!("{command}\n").as_bytes()).await.is_err()
                                {
                                    stdin = None; // the collector went away; it starts again by itself
                                }
                            }
                        }
                    }
                }
                let _ = child.kill().await;
                if output.send(Message::Exited).await.is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(RESTART_DELAY).await;
    }
}

// Runs argv (never through a shell) without waiting for it; a thread reaps it.
fn spawn_detached(argv: &[&str]) {
    let child = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
