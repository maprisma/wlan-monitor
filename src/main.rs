use maprisma_wlan_monitor::i18n::{self, read_pref, tr, trf, write_pref};
use maprisma_wlan_monitor::{corners, shot};
use maprisma_wlan_monitor::look::Desktop;
mod window_size;
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
use std::process::Stdio;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;
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
use cosmic::{ApplicationExt, Element, theme};
use maprisma_wlan_monitor::{APP_NAME, CLOSE_WINDOW, WINDOW_BUS, WindowControl, script_path};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

const APP_ID: &str = "io.github.ubruckhaus.CosmicMaprismaWlanMonitor";
const WINDOW_APP_ID: &str = "io.github.ubruckhaus.CosmicMaprismaWlanMonitor.Window";
static AUTOSIZE_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-autosize"));
static SEARCH_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-search"));
static RENAME_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-rename"));
static FRITZ_USER_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-fritz-user"));
static FRITZ_PASSWORD_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-fritz-password"));
static NOTE_ID: LazyLock<Id> = LazyLock::new(|| Id::new("wlan-monitor-shot-note"));
// F12 pictures go to ~/Pictures/<this>/ (the app's name, the same in every language).
const SHOT_FOLDER: &str = APP_NAME;
const RADAR: &[u8] = include_bytes!("../data/icons/io.github.ubruckhaus.CosmicMaprismaWlanMonitor-symbolic.svg");
const RADAR_OFF: &[u8] = include_bytes!("../data/icons/io.github.ubruckhaus.CosmicMaprismaWlanMonitor-off-symbolic.svg");

const RESTART_DELAY: Duration = Duration::from_secs(5); // before a collector that exited starts again
const POPUP_WIDTH: f32 = 440.0;
const POPUP_MAX_HEIGHT: f32 = 780.0;
const LIST_MAX_HEIGHT: f32 = 420.0;
const CLIPBOARD_LIMIT: usize = 262144;
const FLASH: Duration = Duration::from_millis(1500);

/// A second `--window` start toggles (layout rule 5a): the open window is told to close, the way
/// its tray does, then this one ends. Every entry point (tray, Maprisma Shell's glyph, launcher)
/// gets the same toggle.
async fn close_running_window(bus: &str) {
    if let Ok(connection) = zbus::Connection::session().await {
        let _ = connection.call_method(Some(bus), "/Window", Some("io.github.ubruckhaus.CosmicMaprismaWlanMonitor.Window"), "Close", &()).await;
    }
}

fn main() -> cosmic::iced::Result {
    if !std::env::args().any(|arg| arg == "--window") {
        return cosmic::applet::run::<Monitor>(false);
    }
    let runtime = tokio::runtime::Runtime::new().expect("D-Bus runtime");
    let connection = runtime.block_on(async {
        let connection = zbus::connection::Builder::session()?
            .serve_at("/Window", WindowControl)?.build().await?;
        connection.request_name_with_flags(WINDOW_BUS, zbus::fdo::RequestNameFlags::DoNotQueue.into()).await?;
        Ok::<_, zbus::Error>(connection)
    });
    let _connection = match connection {
        Ok(connection) => connection,
        Err(zbus::Error::NameTaken) => {
            runtime.block_on(close_running_window(WINDOW_BUS));
            return Ok(());
        }
        Err(error) => {
            eprintln!("Window D-Bus: {error}");
            std::process::exit(1);
        }
    };
    cosmic::app::run::<Monitor<true>>(
        cosmic::app::Settings::default()
            .size({ let (width, height) = window_start_size(); cosmic::iced::Size::new(width, height) })
            .resizable(Some(8.0))
            .transparent(false)
            .is_daemon(false),
        true,
    )
}

// The `--window` size: the one the user left it at (layout rule 5a), else the popup's.
fn window_start_size() -> window_size::Size {
    window_size::parse(read_pref(window_size::KEY).as_ref()).unwrap_or((POPUP_WIDTH, POPUP_MAX_HEIGHT))
}

/// What Esc does (layout rule 5a): the innermost open thing first, then the window or popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscStep {
    /// Skip the screenshot's note (or close its error).
    Note,
    /// Cancel the device rename.
    Rename,
    /// Close the FRITZ!Box login form.
    FritzForm,
    /// Close the open Language / Corners / view list.
    Menus,
    /// Clear the device search.
    Filter,
    /// Close the window (`--window`) or the popup.
    Close,
    Nothing,
}

/// What is open, innermost first; `shown`: Esc came from the window or the open popup.
#[derive(Debug, Default, Clone, Copy)]
struct EscState {
    shown: bool,
    note: bool,
    rename: bool,
    fritz_form: bool,
    menus: bool,
    filter: bool,
}

fn esc_step(state: EscState) -> EscStep {
    if !state.shown {
        EscStep::Nothing
    } else if state.note {
        EscStep::Note
    } else if state.rename {
        EscStep::Rename
    } else if state.fritz_form {
        EscStep::FritzForm
    } else if state.menus {
        EscStep::Menus
    } else if state.filter {
        EscStep::Filter
    } else {
        EscStep::Close
    }
}

// ------------------------------------------------------------------------- texts and values

// Signal level: green >= 70 %, yellow >= 50 %, orange >= 30 %, red below. A data scale (the same
// levels as the original's), so fixed colours that do not change with the theme.
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

// State colours from the theme: connected (ok), VPN (info), warnings and errors.
#[derive(Debug, Clone, Copy)]
enum Tone {
    Ok,
    Info,
    Warn,
    Error,
}

impl Tone {
    fn color(self, theme: &cosmic::Theme) -> Color {
        let c = theme.cosmic();
        Color::from(match self {
            Tone::Ok => c.success_color(),
            Tone::Info => c.accent_color(),
            Tone::Warn => c.warning_color(),
            Tone::Error => c.destructive_color(),
        })
    }
}

fn signal_word(quality: f64) -> &'static str {
    if quality >= 70.0 {
        tr("Excellent")
    } else if quality >= 50.0 {
        tr("Good")
    } else if quality >= 30.0 {
        tr("Fair")
    } else {
        tr("Weak")
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
        "router" => tr("Router"),
        "computer" => tr("Computer"),
        "laptop" => tr("Laptop"),
        "phone" => tr("Phone"),
        "tablet" => tr("Tablet"),
        "watch" => tr("Watch"),
        "tv" => tr("TV"),
        "speaker" => tr("Speaker"),
        "printer" => tr("Printer"),
        "console" => tr("Game console"),
        "camera" => tr("Camera"),
        "light" => tr("Lighting"),
        "nas" => tr("Storage / server"),
        "appliance" => tr("Appliance"),
        "smarthome" => tr("Smart-home device"),
        _ => tr("Device"),
    }
}

fn device_kind_label(dev: &Value) -> &'static str {
    if s(dev, "kind") == "router" && !b(dev, "is_gateway") { tr("Repeater / access point") } else { kind_label(s(dev, "kind")) }
}

// Group id → (title, icon).
fn group_info(id: &str) -> (&'static str, &'static str) {
    match id {
        "favourites" => (tr("Favourites"), "starred-symbolic"),
        "apple" => ("Apple", "phone-apple-iphone-symbolic"),
        "android" => ("Android", "phone-symbolic"),
        "mobile" => (tr("Phones & tablets"), "tablet-symbolic"),
        "computers" => (tr("Computers"), "computer-symbolic"),
        "media" => (tr("TV & audio"), "tv-symbolic"),
        "smarthome" => (tr("Smart home"), "user-home-symbolic"),
        "office" => (tr("Printers & storage"), "printer-symbolic"),
        "network" => (tr("Network & mesh"), "network-workgroup-symbolic"),
        "none" => (tr("Not reported by the FRITZ!Box"), "dialog-question-symbolic"),
        "gsix" => ("6 GHz", "network-wireless-symbolic"),
        "gfive" => ("5 GHz", "network-wireless-symbolic"),
        "gtwo" => ("2.4 GHz", "network-wireless-symbolic"),
        "glan" => (tr("LAN (cable)"), "network-wired-symbolic"),
        "gother" => (tr("Band not reported"), "dialog-question-symbolic"),
        "gsleep" => (tr("Sleeping"), "weather-clear-night-symbolic"),
        _ => (tr("Other devices"), "dialog-question-symbolic"),
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
        "platform" => tr("By platform"),
        "type" => tr("By type"),
        "band" => tr("By band"),
        "ap" => tr("By access point"),
        _ => tr("No groups"),
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
        Some(m) if m < 1.0 => tr("just now").into(),
        Some(m) if m < 60.0 => trf("{0} min ago", &[m.round().to_string()]),
        Some(m) => trf("{0} h ago", &[(m / 60.0).floor().to_string()]),
    }
}

fn status_word(dev: &Value) -> String {
    if b(dev, "is_gateway") {
        return tr("router").into();
    }
    if b(dev, "is_self") {
        return tr("this PC").into();
    }
    if b(dev, "online") {
        return tr("online").into();
    }
    if s(dev, "state") == "idle" {
        return tr("idle").into();
    }
    let ago = ago_text(dev);
    if ago.is_empty() { tr("sleeping").into() } else { format!("{} · {ago}", tr("sleeping")) }
}

fn status_explain(dev: &Value) -> String {
    if b(dev, "is_gateway") {
        return tr("Your router: it connects all devices here to each other and to the internet.").into();
    }
    if b(dev, "is_self") {
        return tr("This computer.").into();
    }
    if b(dev, "online") {
        return tr("Online: the device answered just now.").into();
    }
    if s(dev, "state") == "idle" {
        return tr("Idle: connected recently but quiet for a while. On a network you have not trusted, the monitor does not ask devices directly, so it cannot tell whether they are asleep.")
            .into();
    }
    let ago = ago_text(dev);
    format!(
        "{} {}", tr("Sleeping: not answering right now. Phones and tablets do this to save battery and still count as connected."),
        if ago.is_empty() { tr("Known from your router's address list.").to_string() } else { trf("Last seen {0}.", &[ago]) }
    )
}

fn device_key(dev: &Value) -> String {
    let mac = s(dev, "mac");
    if mac.is_empty() { s(dev, "ip").to_string() } else { mac.to_string() }
}

fn fritz_message(code: &str, args: &[Value]) -> String {
    let arg = |i: usize| args.get(i).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)).unwrap_or_default();
    match code {
        "fzConnecting" => tr("Connecting to the FRITZ!Box…").into(),
        "fzPaused" => tr("Paused while the VPN blocks the home network.").into(),
        "fzMethod" => tr("The FRITZ!Box asked for an unsupported login method.").into(),
        "fzCert" => tr("The FRITZ!Box certificate changed. Connect again to trust the new one.").into(),
        "fzNetworkChanged" => tr("The network changed before the login was sent. Connect again.").into(),
        "fzAppAccess" => tr("The FRITZ!Box does not accept app access. Turn on “Allow access for applications” under Home Network › Network › Network Settings.")
            .into(),
        "fzNoAnswer" => tr("The FRITZ!Box did not answer.").into(),
        "fzNotStored" => tr("The login could not be stored in the settings folder.").into(),
        "fzNotDeleted" => {
            tr("The saved login could not be deleted yet. WLAN Monitor keeps trying and does not use it.").into()
        }
        "fzNotSent" => tr("The request could not be passed to the collector that talks to the FRITZ!Box.").into(),
        "fzWrongLogin" => tr("Wrong user name or password.").into(),
        "fzNoRight" => {
            tr("This FRITZ!Box user may not read the home network. Give it the right “FRITZ!Box settings”.").into()
        }
        "fzRefused" => trf("The FRITZ!Box refused the request (error {0}).", &[arg(0)]),
        "fzNoMesh" => tr("The FRITZ!Box sent no mesh list.").into(),
        "fzMeshUnreadable" => tr("The FRITZ!Box mesh list could not be read.").into(),
        "fzMeshEmpty" => tr("The FRITZ!Box mesh list was empty.").into(),
        _ => String::new(),
    }
}

const FRITZ_EXPLAIN: &str = "Shows which access point, band and speed every device uses, and the names from your FRITZ!Box. Use a FRITZ!Box user with the right “FRITZ!Box settings” (System › FRITZ!Box Users). The password is stored only on this PC (readable by you alone) and used only with this FRITZ!Box.";
const FRITZ_HELP: &str = "• There is no fixed default user name.\n• FRITZ!OS before 7.24: the FRITZ!Box asks only for a password. Leave the user name empty.\n• FRITZ!OS 7.24 and newer: if you never created a user, an update created one named “fritz” plus four random digits (for example fritz1234). Log in at http://fritz.box and look under System › FRITZ!Box Users.\n• Password: unless you changed it, it is printed on the label on the bottom or back of the FRITZ!Box (4 to 6 lowercase letters followed by 4 digits).";

// ------------------------------------------------------------------------- widgets

fn dim_text(theme: &cosmic::Theme) -> iced_text::Style {
    let mut color = Color::from(theme.cosmic().on_bg_color());
    color.a = 0.6;
    iced_text::Style { color: Some(color), ..Default::default() }
}

fn accent_text(theme: &cosmic::Theme) -> iced_text::Style {
    iced_text::Style { color: Some(Color::from(theme.cosmic().accent_text_color())), ..Default::default() }
}

fn warn_text(theme: &cosmic::Theme) -> iced_text::Style {
    iced_text::Style { color: Some(Tone::Warn.color(theme)), ..Default::default() }
}

fn dim<'a>(content: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Message> {
    text::caption(content).class(theme::Text::Custom(dim_text)).into()
}

fn accent<'a>(content: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Message> {
    text::caption(content).class(theme::Text::Custom(accent_text)).into()
}

// Settings › Corners: small parts (tracks, fills, swatches, signal bars), controls (notice boxes,
// the open device's row) and round ones (the status pill).
fn small() -> f32 {
    corners::now().small
}

fn control() -> f32 {
    corners::now().control
}

fn pill_radius() -> f32 {
    corners::now().pill
}

fn fill(color: Color, radius: f32) -> theme::Container<'static> {
    theme::Container::custom(move |_| iced_container::Style {
        background: Some(Background::Color(color)),
        border: Border { radius: radius.into(), ..Default::default() },
        ..Default::default()
    })
}

fn tone_fill(tone: Tone, radius: f32) -> theme::Container<'static> {
    theme::Container::custom(move |theme: &cosmic::Theme| iced_container::Style {
        background: Some(Background::Color(tone.color(theme))),
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
fn notice(tone: Tone) -> theme::Container<'static> {
    theme::Container::custom(move |theme: &cosmic::Theme| {
        let color = tone.color(theme);
        let mut back = color;
        back.a = 0.12;
        let mut edge = color;
        edge.a = 0.5;
        iced_container::Style {
            background: Some(Background::Color(back)),
            border: Border { radius: control().into(), width: 1.0, color: edge },
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
            border: Border { radius: control().into(), ..Default::default() },
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
    container(line).width(Length::Fill).class(track((height / 2.0).min(small()))).into()
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
    container(text::caption_heading(title.to_uppercase()).class(theme::Text::Custom(dim_text))).padding([8, 0, 0, 0]).into()
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
        let class = if lit { fill(signal_color(quality / 100.0), small().min(1.0)) } else { track(small().min(1.0)) };
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
fn radar<'a>(size: f32, tone: Option<Tone>, alpha: f32) -> Element<'a, Message> {
    radar_shape(RADAR, size, tone, alpha)
}

// The bar's radar: rings without sweep and dot while not connected (as the tray shows it).
fn bar_radar<'a>(on: bool, size: f32, tone: Option<Tone>, alpha: f32) -> Element<'a, Message> {
    radar_shape(if on { RADAR } else { RADAR_OFF }, size, tone, alpha)
}

fn radar_shape<'a>(bytes: &'static [u8], size: f32, tone: Option<Tone>, alpha: f32) -> Element<'a, Message> {
    svg(iced_svg::Handle::from_memory(bytes))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .class(theme::Svg::custom(move |theme: &cosmic::Theme| {
            let mut c = tone.map_or_else(|| Color::from(theme.cosmic().on_bg_color()), |t| t.color(theme));
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
struct Monitor<const WINDOW_MODE: bool = false> {
    core: Core,
    window_mode: bool,
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
    language_open: bool,
    language_error: bool,
    last_language: String,
    fritz_form_open: bool,
    fritz_form_net: String,
    fritz_form_net_id: String,
    fritz_user: String,
    fritz_user_net: String,
    fritz_password: String,
    fritz_password_visible: bool,
    fritz_help_open: bool,
    flash: HashMap<String, std::time::Instant>,
    // F12: the note's state, whether a capture is on its way, the settings.
    shot: shot::Phase,
    capturing: bool,
    shot_key: bool,
    corners: String,
    corners_open: bool,
    setting_error: bool,
    resize: window_size::Debounce,
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
    WindowTick,
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
    ToggleLanguage,
    Language(String),
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
    ShotKey(window::Id),
    Escape(window::Id),
    Captured(window::Screenshot),
    ShotSaved(Result<shot::Saved, String>),
    NoteInput(String),
    NoteDone,
    NoteSaved(Result<(), String>),
    SetShotKey(bool),
    ToggleCorners,
    Corners(String),
}

// Runs `work` on its own thread (saving must not hold up the popup) and answers with `done`.
fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    done: fn(Result<T, String>) -> Message,
) -> Task<Message> {
    let (sender, receiver) = cosmic::iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    Task::perform(receiver, move |result| {
        cosmic::Action::App(done(result.unwrap_or_else(|_| Err("the file could not be written".into()))))
    })
}

// Settings › F12 saves a screenshot (on unless switched off) and Corners, from maprisma-settings.json.
fn shot_key_setting() -> bool {
    read_pref("screenshot_key").and_then(|v| v.as_bool()).unwrap_or(true)
}

fn corners_setting() -> String {
    corners::normalize(read_pref("corners").as_ref().and_then(Value::as_str).unwrap_or("")).to_string()
}

impl<const WINDOW_MODE: bool> Monitor<WINDOW_MODE> {
    // Done, Enter or the popup closing (keep: the note typed so far is saved), Esc (skip).
    fn finish_note(&mut self, keep: bool) -> Task<Message> {
        match std::mem::take(&mut self.shot) {
            shot::Phase::Note { path, text, .. } => {
                if keep && shot::note_text(&text).is_some() {
                    // Busy until the note is written, so a new F12 cannot replace it.
                    self.shot = shot::Phase::Noting;
                    return off_thread(
                        move || shot::save_note(&path, SHOT_FOLDER, &text).map(|_| ()).map_err(|e| e.to_string()),
                        Message::NoteSaved,
                    );
                }
            }
            shot::Phase::Error(_) => {}
            other => self.shot = other,
        }
        Task::none()
    }

    // A capture still on its way is dropped with its popup; a typed note is kept (saved).
    fn shot_popup_closed(&mut self) -> Task<Message> {
        if std::mem::take(&mut self.capturing) && self.shot == shot::Phase::Busy {
            self.shot = shot::Phase::Idle;
        }
        if matches!(self.shot, shot::Phase::Note { .. }) { self.finish_note(true) } else { Task::none() }
    }

    // After F12: "Screenshot saved", where, the optional note and Done (or the error and Done).
    fn shot_overlay(&self) -> Option<Element<'_, Message>> {
        let body: Element<Message> = match &self.shot {
            shot::Phase::Note { shown, text: note, .. } => column![
                text::heading(tr("Screenshot saved")),
                dim(trf("Saved as {0}", &[shown.clone()])),
                text_input(tr("What is wrong? (optional)"), note.as_str())
                    .id(NOTE_ID.clone())
                    .on_input(Message::NoteInput)
                    .on_submit(|_| Message::NoteDone),
                row![space::horizontal().width(Length::Fill), button::suggested(tr("Done")).on_press(Message::NoteDone)],
            ]
            .spacing(12)
            .into(),
            shot::Phase::Error(reason) => column![
                text::body(reason.clone()),
                row![space::horizontal().width(Length::Fill), button::suggested(tr("Done")).on_press(Message::NoteDone)],
            ]
            .spacing(12)
            .into(),
            _ => return None,
        };
        let card = container(body).padding(16).width(Length::Fill).class(theme::Container::Card);
        let card = container(card).padding(12).width(if self.window_mode { Length::Fill } else { Length::Fixed(POPUP_WIDTH) });
        Some(card.into())
    }

    fn popup_content(&self) -> Element<'_, Message> {
        if let Some(overlay) = self.shot_overlay() {
            return overlay;
        }
        let body = scrollable(container(self.panel()).padding([12, 16]))
            .height(if self.window_mode { Length::Fill } else { Length::Shrink });
        if self.window_mode {
            container(body).width(Length::Fill).height(Length::Fill).into()
        } else {
            column![container(body).max_height(POPUP_MAX_HEIGHT)].width(Length::Fixed(POPUP_WIDTH)).into()
        }
    }

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
            return if s(m, "band") == "LAN" { tr("cable").into() } else { s(m, "band").into() };
        }
        let band = s(m, "band");
        format!("{}{}", trf("via {0}", &[s(m, "via").into()]), if band.is_empty() { String::new() } else { format!(" · {band}") })
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
            parts.push(if band == "LAN" { tr("cable (LAN)").to_string() } else { band.to_string() });
        }
        if rate > 0.0 {
            parts.push(format!("{rate} Mbit/s"));
        }
        if let Some(dbm) = num(m, "signal_dbm") {
            parts.push(trf("signal {0} ({1} dBm)", &[signal_word(signal_quality(Some(dbm))).to_lowercase(), dbm.to_string()]));
        }
        parts.join(" · ")
    }

    fn right_label(&self, dev: &Value) -> String {
        let word = status_word(dev);
        if b(dev, "online") || b(dev, "is_gateway") { s(dev, "ip").into() } else { format!("{word} · {}", s(dev, "ip")) }
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
            parts.push(trf("model {0}", &[model.into()]));
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
                    tr("main router").to_string()
                } else if !s(ap, "via").is_empty() {
                    let band = s(ap, "band");
                    format!("{}{}", trf("via {0}", &[s(ap, "via").into()]), if band.is_empty() { String::new() } else { format!(" {band}") })
                } else {
                    tr("repeater").to_string()
                };
                let name = if s(ap, "name").is_empty() { tr("Access point").to_string() } else { s(ap, "name").to_string() };
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
        fn walk<const W: bool>(
            this: &Monitor<W>,
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
        let color = active.then(|| if self.vpn().is_some() { Tone::Info } else { Tone::Ok });
        let alpha = if self.scanning() && self.pulse_dim {
            0.3
        } else if active || self.scanning() {
            1.0
        } else {
            0.6
        };
        let mut glyph = row![bar_radar(active || self.scanning(), size, color, alpha)].align_y(Alignment::Start);
        // Dot: devices joined recently and are not marked as known yet.
        if n(&self.stats, "new_count") > 0.0 {
            glyph = glyph.push(
                container(space::horizontal())
                    .width(Length::Fixed(5.0))
                    .height(Length::Fixed(5.0))
                    .class(tone_fill(Tone::Error, 3.0)),
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
            tr("Not connected").into()
        };
        let color = active.then(|| if self.vpn().is_some() { Tone::Info } else { Tone::Ok });
        let mut hero = row![
            radar(32.0, color, if active { 1.0 } else { 0.6 }),
            column![text::title4(tr("Maprisma WLAN Monitor")), dim(meta)].spacing(4),
            space::horizontal().width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if active {
            hero = hero.push(
                container(text::caption(trf("{0} online", &[n(&self.stats, "online_count").to_string()])))
                    .padding([4, 8])
                    .class(track(pill_radius())),
            );
        }
        hero.into()
    }

    fn tabs(&self) -> Element<'_, Message> {
        let map = self.map_page();
        row![
            choice(tr("Devices"), !map)
                .leading_icon(icon::from_name("view-list-symbolic").size(16).handle())
                .on_press_maybe(map.then(|| Message::ToggleGroup("mappage".into()))),
            choice(tr("Network map"), map)
                .leading_icon(icon::from_name("network-workgroup-symbolic").size(16).handle())
                .on_press_maybe((!map).then(|| Message::ToggleGroup("mappage".into()))),
        ]
        .spacing(8)
        .into()
    }

    fn notices(&self) -> Vec<Element<'_, Message>> {
        let mut out = Vec::new();
        // A VPN blocks the home network: explain why everything looks idle, and how to fix it.
        if self.lan_blocked() {
            let vpn = self.vpn().unwrap_or(&Value::Null);
            let name = if s(vpn, "name").is_empty() { tr("A VPN") } else { s(vpn, "name") };
            let command = self.vpn_command();
            let hint = if command.is_empty() {
                tr("Allow local network (LAN) access in your VPN settings.").to_string()
            } else {
                trf("Allow it with: {0}", &[command.clone()])
            };
            let mut block = column![
                row![small_icon("network-vpn-symbolic", 16), text::heading(trf("{0} is on and blocks your home network", &[name.into()]))]
                    .spacing(8)
                    .align_y(Alignment::Center),
                dim(format!(
                    "{} {hint} {}", tr("Devices can't be reached, so they show as idle or sleeping, and the router page, names from the network and FRITZ!Box details are unavailable."), tr("Or disconnect the VPN.")
                )),
            ]
            .spacing(8);
            if !command.is_empty() {
                let label = if self.flashing("vpn") { tr("Copied").to_string() } else { trf("Copy command: {0}", &[command]) };
                block = block.push(link("edit-copy-symbolic", label, Some(Message::CopyVpnCommand)));
            }
            out.push(container(block).padding(12).width(Length::Fill).class(notice(Tone::Info)).into());
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
                        link("dialog-password-symbolic", tr("Connect FRITZ!Box again").into(), Some(Message::StartFritzLogin)),
                    ]
                    .spacing(8),
                )
                .padding(12)
                .width(Length::Fill)
                .class(notice(Tone::Warn))
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
            section(tr("Connection")),
            row![
                text::heading(format!("{}  {ip}", tr("This PC"))),
                space::horizontal().width(Length::Fill),
                dim(if gateway.is_empty() { String::new() } else { format!("{}  {gateway}", tr("Router")) }),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(8);
        if active {
            let cidr = if !s(link, "cidr").is_empty() { s(link, "cidr") } else { ip };
            let iface = if s(link, "iface").is_empty() { "?" } else { s(link, "iface") };
            let mut parts = vec![trf("Address {0} on {1}", &[cidr.into(), iface.into()])];
            if !s(link, "mac").is_empty() {
                parts.push(trf("MAC {0}", &[s(link, "mac").into()]));
            }
            if !s(link, "domain").is_empty() {
                parts.push(trf("Local domain {0}", &[s(link, "domain").into()]));
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
                tip.push(trf("channel {0}", &[shown(link, "channel")]));
            }
            if n(link, "tx_mbps") > 0.0 {
                let rx = shown(link, "rx_mbps");
                tip.push(trf("link {0} Mbit/s", &[format!("{}{}", shown(link, "tx_mbps"), if rx.is_empty() { String::new() } else { format!(" / {rx}") })]));
            }
            if quality < 30.0 {
                tip.push(tr("Weak? Move closer to the router or remove obstacles between you.").into());
            }
            block = block.push(meter(
                format!("{}  ·  {}", tr("Signal"), signal_word(quality)),
                format!("{} %", quality.round()),
                fraction,
                fill(signal_color(fraction), small()),
                tip.join("  ·  "),
            ));
        }
        let rate = n(traffic, "rx_rate") + n(traffic, "tx_rate");
        block = block.push(meter(
            tr("Traffic now").into(),
            format!("↓ {}  ·  ↑ {}", format_rate(num(traffic, "rx_rate")), format_rate(num(traffic, "tx_rate"))),
            if rate <= 0.0 { 0.0 } else { ((1.0 + rate / 1024.0).log10() / 4.0).min(1.0) },
            neutral_fill(small()),
            trf("Since connecting: {0} down, {1} up", &[format_bytes(n(traffic, "rx_bytes")), format_bytes(n(traffic, "tx_bytes"))]),
        ));
        // Always say when a VPN is on, also when it lets the home network through.
        if let Some(vpn) = self.vpn() {
            block = block.push(accent(format!(
                "{}  ·  {}",
                trf("{0} is on", &[s(vpn, "name").into()]),
                if self.lan_blocked() { tr("blocks your home network") } else { tr("home network reachable") }
            )));
        }
        if active {
            // One summary line; the explanations and the FRITZ!Box login open on click.
            let mut summary = if self.trusted() { tr("Your network").to_string() } else { tr("Not your network (passive)").to_string() };
            let via = s(link, "via");
            if !via.is_empty() {
                let via = via.strip_prefix("Router (").and_then(|v| v.strip_suffix(')')).unwrap_or(via);
                summary += &format!("  ·  {}", trf("via {0}", &[via.into()]));
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
                    .padding([4, 8])
                    .width(Length::Fill)
                    .class(theme::Button::MenuItem)
                    .on_press(Message::ToggleGroup("showinfo".into())),
            );
            let fritz = self.fritz();
            let state = s(fritz, "state");
            if self.info_open() || self.fritz_form_open || state == "error" || state == "connecting" {
                let mut info = column![dim(if self.trusted() {
                    if self.stats.get("alerts").and_then(Value::as_bool) != Some(false) {
                        tr("Your network (trusted): looks for devices every 2 minutes and tells you when a new one joins.")
                    } else {
                        tr("Your network (trusted): looks for devices every 2 minutes. New-device alerts are off.")
                    }
                } else {
                    tr("Not marked as your network, so only devices that announce themselves are shown. If this is your home network, click Trust to find every device and get alerts about new ones.")
                })]
                .spacing(8);
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
        let mut links = Row::new().spacing(8).align_y(Alignment::Center);
        if state == "connected" {
            let count = self.aps().len();
            links = links.push(dim(if count == 1 {
                tr("FRITZ!Box connected · 1 access point").to_string()
            } else {
                trf("FRITZ!Box connected · {0} access points", &[count.to_string()])
            }));
        }
        if state != "connected" && state != "connecting" && !self.fritz_form_open {
            let label = if state == "error" && !user.is_empty() { tr("Connect FRITZ!Box again") } else { tr("Show mesh details from the FRITZ!Box") };
            links = links.push(link("dialog-password-symbolic", label.into(), Some(Message::StartFritzLogin)));
        }
        if !user.is_empty() && !self.fritz_form_open {
            links = links.push(link("view-conceal-symbolic", tr("Disconnect").into(), Some(Message::FritzLogout)));
        }
        let mut block = column![links].spacing(8);
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
                dim(tr(FRITZ_EXPLAIN)),
                text_input(tr("User name, e.g. fritz1234"), self.fritz_user.as_str())
                    .id(FRITZ_USER_ID.clone())
                    .on_input(|v| Message::FritzUser(v.chars().take(64).collect()))
                    .on_submit(|_| Message::FritzFocusPassword),
                link(
                    if self.fritz_help_open { "pan-down-symbolic" } else { "pan-end-symbolic" },
                    tr("Where do I find user name and password?").into(),
                    Some(Message::FritzHelp)
                ),
            ]
            .spacing(8);
            if self.fritz_help_open {
                form = form.push(container(dim(tr(FRITZ_HELP))).padding([0, 0, 0, 12]));
            }
            form = form.push(
                secure_input(tr("Password"), self.fritz_password.as_str(), Some(Message::FritzPasswordVisible), !self.fritz_password_visible)
                    .id(FRITZ_PASSWORD_ID.clone())
                    .on_input(|v| Message::FritzPassword(v.chars().take(128).collect()))
                    .on_submit(|_| Message::FritzConnect),
            );
            form = form.push(
                row![
                    button::suggested(tr("Connect")).on_press_maybe((!self.fritz_password.is_empty()).then_some(Message::FritzConnect)),
                    button::standard(tr("Cancel")).on_press(Message::FritzCancel),
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
                text::caption_heading(trf("Devices ({0})", &[total.to_string()]).to_uppercase()).class(theme::Text::Custom(dim_text)).width(Length::Fill),
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
            let joined = if new_count == 1 { tr("1 new device joined").to_string() } else { trf("{0} new devices joined", &[new_count.to_string()]) };
            page = page.push(
                container(
                    row![
                        small_icon("emblem-important-symbolic", 16),
                        text::caption(format!("{joined}. {}", tr("Not yours? Change your Wi-Fi password in the router."))).width(Length::Fill),
                        link("object-select-symbolic", tr("All known").into(), Some(Message::KnownAll)),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding(12)
                .class(notice(Tone::Warn)),
            );
        }
        if n(&self.stats, "named_count") == 0.0 && self.devices.len() > 1 && new_count == 0 {
            page = page.push(dim(
                tr("Tip: click a device to see what it is, give it a name you recognise, pin it to the top or open its web page."),
            ));
        }
        if total.saturating_sub(hidden) > 12 || !self.filter.is_empty() {
            page = page.push(
                text_input(tr("Search devices (/)"), self.filter.as_str())
                    .id(SEARCH_ID.clone())
                    .leading_icon(small_icon("system-search-symbolic", 16))
                    .on_input(|v| Message::Filter(v.chars().take(40).collect())),
            );
        }
        if active && !self.trusted() && total <= 3 {
            page = page.push(dim(tr("Only a few devices visible. Click Rescan to look for more.")));
        }

        let entries = self.list_entries();
        let mut list = column![].spacing(4);
        for entry in &entries {
            list = list.push(match entry {
                Entry::Header { .. } => self.group_header(entry),
                Entry::Device { index, in_group } => {
                    let row = self.device_row(*index);
                    if *in_group { container(row).padding([0, 0, 0, 12]).into() } else { row }
                }
            });
        }
        if entries.is_empty() && !self.filter.is_empty() {
            list = list.push(dim(trf("No device matches “{0}”.", &[self.filter.clone()])));
        }
        page = page.push(container(scrollable(list).height(Length::Shrink)).max_height(LIST_MAX_HEIGHT));

        let mut footer = Row::new().spacing(8);
        if !self.devices.is_empty() {
            footer = footer.push(link(
                "edit-copy-symbolic",
                if self.flashing("list") { tr("Copied").into() } else { tr("Copy list").into() },
                Some(Message::CopyList),
            ));
        }
        if hidden > 0 {
            footer = footer.push(link(
                if self.show_hidden { "view-conceal-symbolic" } else { "view-reveal-symbolic" },
                if self.show_hidden { trf("Hide {0} hidden", &[hidden.to_string()]) } else { trf("Show {0} hidden", &[hidden.to_string()]) },
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
            count_text += tr("NEW");
            count_text += "  ";
        }
        count_text += &count.to_string();
        if online != count {
            count_text += &format!(" · {}", trf("{0} online", &[online.to_string()]));
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
            .padding([4, 8])
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
                    text_input(if s(dev, "auto_name").is_empty() { tr("Device name") } else { s(dev, "auto_name") }, self.edit_draft.as_str())
                        .id(RENAME_ID.clone())
                        .on_input(|v| Message::RenameDraft(v.chars().take(40).collect()))
                        .on_submit(|_| Message::FinishRename),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
            block = block.push(container(dim(tr("Enter saves · Esc cancels · empty restores the automatic name"))).padding([0, 0, 0, 26]));
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
                main = main.push(accent(tr("NEW")));
            }
            if b(dev, "pinned") {
                main = main.push(small_icon("view-pin-symbolic", 14));
            }
            let right = if self.flashing(&format!("ip:{key}")) { tr("copied to clipboard").to_string() } else { self.right_label(dev) };
            main = main.push(if b(dev, "is_gateway") || b(dev, "is_self") { accent(right) } else { dim(right) });
            let mut top = row![
                button::custom(main)
                    .padding([4, 8])
                    .width(Length::Fill)
                    .class(theme::Button::MenuItem)
                    .on_press(Message::ToggleDevice(key.clone()))
            ]
            .spacing(4)
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
            let mut details = column![text::caption(status_explain(dev))].spacing(4);
            let via = dev.get("mesh").map(|m| s(m, "via")).unwrap_or("");
            let vendor = s(dev, "vendor");
            let hostname = s(dev, "hostname");
            for (name, value) in [
                (tr("Connected via"), if !via.is_empty() && !b(dev, "is_gateway") { via.to_string() } else { String::new() }),
                (tr("Connection"), self.mesh_detail(dev)),
                (tr("Maker"), if vendor == "Private MAC" { tr("unknown (hidden by a private address)").into() } else { vendor.to_string() }),
                (tr("Model"), s(dev, "model").to_string()),
                (tr("Automatic name"), if s(dev, "custom_name").is_empty() { String::new() } else { s(dev, "auto_name").to_string() }),
                (tr("Network name"), if !hostname.is_empty() && hostname != s(dev, "name") { hostname.to_string() } else { String::new() }),
                (tr("IP address"), s(dev, "ip").to_string()),
                (tr("MAC address"), s(dev, "mac").to_string()),
            ] {
                if !value.is_empty() {
                    details = details.push(dim(format!("{name}:  {value}")));
                }
            }
            let can_edit = b(dev, "can_edit");
            let editable = can_edit && !b(dev, "is_gateway") && !b(dev, "is_self");
            let mac = s(dev, "mac").to_string();
            let mut actions = Row::new().spacing(4);
            if can_edit && !editing {
                actions = actions.push(link("document-edit-symbolic", tr("Rename").into(), Some(Message::StartRename(key.clone()))));
            }
            if editing {
                actions = actions.push(link("object-select-symbolic", tr("Save").into(), Some(Message::FinishRename)));
                actions = actions.push(link("window-close-symbolic", tr("Cancel").into(), Some(Message::CancelRename)));
            }
            if editable && !editing {
                let pinned = b(dev, "pinned");
                actions = actions.push(link(
                    "view-pin-symbolic",
                    if pinned { tr("Unpin").into() } else { tr("Pin to top").into() },
                    Some(Message::DeviceCommand("pin", mac.clone(), json!({"on": !pinned}))),
                ));
                let hidden = b(dev, "hidden");
                actions = actions.push(link(
                    if hidden { "view-reveal-symbolic" } else { "view-conceal-symbolic" },
                    if hidden { tr("Show").into() } else { tr("Hide").into() },
                    Some(Message::DeviceCommand("hide", mac.clone(), json!({"on": !hidden}))),
                ));
            }
            if !web_url.is_empty() && !editing {
                actions = actions.push(link("web-browser-symbolic", tr("Open web page").into(), Some(Message::OpenWeb(web_url.to_string()))));
            }
            if b(dev, "new") && !editing {
                actions = actions.push(link(
                    "object-select-symbolic",
                    tr("It's mine").into(),
                    Some(Message::DeviceCommand("known", mac.clone(), json!({"on": true}))),
                ));
            }
            details = details.push(actions.wrap());
            if !can_edit {
                details = details.push(dim(
                    tr("This device's hardware address is unknown, so it cannot be renamed yet. Try again when it is online."),
                ));
            }
            block = block.push(container(details).padding([0, 8, 8, 32]));
        }
        if expanded { container(block).class(selected_row()).into() } else { block.into() }
    }

    fn map_page_view(&self) -> Element<'_, Message> {
        let n_devices = self.devices.iter().filter(|d| !b(d, "hidden") && !b(d, "is_gateway")).count();
        let usable = self.view_mode() != "list" && self.view_mode() != "ap";
        let ids = self.map_fold_ids();
        let mut page = column![
            row![
                dim(if n_devices == 1 { trf("{0} device", &["1".into()]) } else { trf("{0} devices", &[n_devices.to_string()]) }),
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
        let mut tree = column![].spacing(0);
        for row_data in self.map_rows() {
            tree = tree.push(self.map_row(row_data));
        }
        page = page.push(container(scrollable(tree).height(Length::Shrink)).max_height(LIST_MAX_HEIGHT + 120.0));
        if !self.mesh_active() {
            page = page.push(dim(if b(self.fritz(), "available") {
                tr("Connect your FRITZ!Box (Devices page, click the network line) to see the band, speed and signal of every device.")
            } else {
                tr("Band, speed and signal per device are shown for FRITZ!Box routers after an optional login.")
            }));
        }
        page = page.push(dim(
            tr("Click a group or repeater to fold it, a device to see its details. The grouping is shared with the Devices page."),
        ));
        page.into()
    }

    fn map_row(&self, r: MapRow) -> Element<'_, Message> {
        let dev = r.dev.map(|i| &self.devices[i]);
        let (glyph, label, strong, online, message): (&'static str, String, bool, bool, Option<Message>) = match &r.kind {
            MapKind::Internet => {
                let via = self.vpn().map(|v| format!("  ·  {}", trf("via {0}", &[s(v, "name").into()]))).unwrap_or_default();
                ("web-browser-symbolic", format!("{}{via}", tr("Internet")), true, true, None)
            }
            MapKind::Ap { id, title } => {
                let d = dev.unwrap_or(&Value::Null);
                let model = s(d, "model");
                let title = if title.is_empty() { tr("Router").to_string() } else { title.clone() };
                let label = if b(d, "is_gateway") && !model.is_empty() { format!("{title}  ·  {model}") } else { title };
                let message =
                    if b(d, "is_gateway") { r.dev.map(Message::OpenDevice) } else { Some(Message::ToggleGroup(id.clone())) };
                (kind_icon("router"), label, true, true, message)
            }
            MapKind::Group { id, title, icon, count, online } => (
                icon,
                format!("{title}  ·  {}", if *count == 1 { trf("{0} device", &["1".into()]) } else { trf("{0} devices", &[count.to_string()]) }),
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
                content = content.push(accent(tr("NEW")));
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
            Some(m) => button::custom(main).padding([4, 8]).width(Length::Fill).class(theme::Button::MenuItem).on_press(m).into(),
            None => container(main).padding([4, 8]).into(),
        };
        container(main).padding([0, 0, 0, (r.depth.saturating_sub(1) * 18) as u16]).into()
    }

    fn footer(&self) -> Element<'_, Message> {
        let gateway_url = self.gateway().map(|g| s(g, "web_url")).filter(|u| !u.is_empty());
        let mut buttons = Row::new().spacing(8).align_y(Alignment::Center);
        buttons = buttons.push(
            button::standard(tr("Rescan"))
                .leading_icon(icon::from_name("view-refresh-symbolic").size(16).handle())
                .on_press_maybe((!self.scanning()).then_some(Message::Rescan)),
        );
        if let Some(url) = gateway_url {
            buttons = buttons.push(
                button::standard(tr("Router"))
                    .leading_icon(icon::from_name("network-wireless-symbolic").size(16).handle())
                    .on_press(Message::OpenWeb(url.to_string())),
            );
        }
        if self.active() {
            let trusted = self.trusted();
            buttons = buttons.push(
                button::standard(if trusted { tr("Untrust") } else { tr("Trust") })
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
                    .label(tr("New-device alerts").to_string())
                    .on_toggle(Message::Alerts),
            );
        }
        footer = footer.push(button::text(tr("Language")).on_press(Message::ToggleLanguage));
        if self.language_open {
            let current = i18n::setting();
            let mut languages = Row::new().spacing(8);
            for (code, label) in i18n::choices() {
                let selected = current == code;
                let button = if selected { button::suggested(label) } else { button::text(label) };
                languages = languages.push(button.on_press(Message::Language(code)));
            }
            footer = footer.push(languages.wrap());
        }
        if self.language_error { footer = footer.push(dim(tr("Could not save the language setting."))); }
        footer = footer.push(toggler(self.shot_key).label(tr("F12 saves a screenshot").to_string()).on_toggle(Message::SetShotKey));
        footer = footer.push(button::text(tr("Corners")).on_press(Message::ToggleCorners));
        if self.corners_open {
            let desktop = corners::desktop_radius().round().to_string();
            let mut choices = Row::new().spacing(8);
            for (choice, label) in [
                ("desktop", trf("Like the desktop ({0} px)", &[desktop])),
                ("slight", trf("Slightly rounded ({0} px)", &["6".into()])),
                ("rounded", trf("Rounded ({0} px)", &["12".into()])),
                ("square", tr("Square").to_string()),
            ] {
                let button = if self.corners == choice { button::suggested(label) } else { button::text(label) };
                choices = choices.push(button.on_press(Message::Corners(choice.into())));
            }
            footer = footer.push(choices.wrap());
        }
        if self.setting_error { footer = footer.push(dim(tr("Could not save the setting."))); }
        footer.into()
    }

    fn panel(&self) -> Element<'_, Message> {
        let mut content = column![self.hero(), self.tabs()].spacing(12);
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
            format!("# {} ({now})", trf("Devices on {0}", &[cell(if ssid.is_empty() { tr("the network") } else { ssid })])),
            String::new(),
            format!("| {} | {} | {} | IP | {} | {} | MAC |", tr("Name"), tr("Type"), tr("Status"), tr("Maker"), tr("Model")),
            "| --- | --- | --- | --- | --- | --- | --- |".into(),
        ];
        for dev in self.devices.iter().take(256) {
            let status = status_word(dev) + &if b(dev, "hidden") { format!(" {}", tr("(hidden)")) } else { String::new() };
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

    // A resize that has settled (or, on `closing`, any waiting one) goes to the settings; not
    // while maximised, fullscreen or tiled (libcosmic's sharp corners).
    fn save_size(&mut self, closing: bool) {
        let sharp = self.core.window.sharp_corners;
        let ignore = || window_size::not_users_size(sharp);
        let size = if closing { self.resize.flush(ignore) } else { self.resize.due(std::time::Instant::now(), ignore) };
        if let Some(size) = size {
            self.setting_error = write_pref(window_size::KEY, Some(window_size::to_value(size))).is_err();
        }
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

impl<const WINDOW_MODE: bool> cosmic::Application for Monitor<WINDOW_MODE> {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = bool;
    type Message = Message;
    // Wayland's app id: the panel applet's, or the window's own (`<id>.Window`, the id of the
    // `.Window.desktop` entry install.sh writes, so taskbars and window rules find it).
    const APP_ID: &'static str = if WINDOW_MODE { WINDOW_APP_ID } else { APP_ID };

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, window_mode: bool) -> (Self, Task<Message>) {
        let mut app = Self { core, window_mode, shot_key: shot_key_setting(), corners: corners_setting(), ..Default::default() };
        if window_mode { app.resize = window_size::Debounce::new(window_start_size()); }
        corners::set_current(&app.corners);
        let task = if window_mode {
            app.set_header_title(tr(APP_NAME).into());
            let id = app.core.main_window_id().unwrap();
            // The desktop's theme for the whole window (title bar too), see look.rs.
            Task::batch([app.set_window_title(tr(APP_NAME).into(), id), corners::set_app_theme(&app.corners)])
        } else { Task::none() };
        (app, task)
    }

    /// COSMIC's colours or light/dark mode changed: a window with its own corners (a custom
    /// theme, which libcosmic leaves alone) takes the new colours too.
    fn system_theme_update(&mut self, _keys: &[&'static str], new_theme: &cosmic::cosmic_theme::Theme) -> Task<Message> {
        corners::system_theme_update(&self.corners, new_theme, self.window_mode)
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }

    fn on_window_resize(&mut self, id: window::Id, width: f32, height: f32) {
        if self.window_mode && self.core.main_window_id() == Some(id) {
            self.resize.resized(width, height, std::time::Instant::now());
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            Subscription::run_with("wlan-monitor-collector", |_| {
                stream::channel(8, |mut output| async move {
                    run_collector(&mut output).await;
                })
            }),
        ];
        if !self.window_mode {
            subscriptions.push(rectangle_tracker_subscription(0).map(|update| Message::Rectangle(update.1)));
        }
        if self.window_mode {
            subscriptions.push(cosmic::iced::time::every(Duration::from_millis(250)).map(|_| Message::WindowTick));
        }
        // F12 (no modifiers) in the popup or window; Esc closes the innermost thing (esc_step).
        subscriptions.push(cosmic::iced::event::listen_with(|event, status, id| match event {
            cosmic::iced::Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                if shot::is_shot_key(&key, modifiers) {
                    Some(Message::ShotKey(id))
                } else if status == cosmic::iced::event::Status::Ignored
                    && matches!(key, cosmic::iced::keyboard::Key::Named(cosmic::iced::keyboard::key::Named::Escape))
                {
                    // Not one an open drop-down list took to close itself.
                    Some(Message::Escape(id))
                } else {
                    None
                }
            }
            _ => None,
        }));
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
            Message::ToggleLanguage => { self.language_open = !self.language_open; self.corners_open = false; }
            Message::ShotKey(id) => {
                let shown = if self.window_mode { self.core.main_window_id() == Some(id) } else { self.popup == Some(id) };
                if self.shot_key && shown && self.shot == shot::Phase::Idle {
                    self.shot = shot::Phase::Busy;
                    self.capturing = true;
                    return window::screenshot(id).map(|picture| cosmic::Action::App(Message::Captured(picture)));
                }
            }
            Message::Captured(picture) => {
                if !std::mem::take(&mut self.capturing) { return Task::none(); }
                let (width, height) = (picture.size.width, picture.size.height);
                return off_thread(move || shot::save_screenshot(SHOT_FOLDER, &picture.rgba, width, height), Message::ShotSaved);
            }
            Message::ShotSaved(result) => {
                self.shot = match result {
                    Ok(saved) => shot::Phase::Note { path: saved.path, shown: saved.shown, text: String::new() },
                    Err(reason) => shot::Phase::Error(trf("The screenshot could not be saved: {0}", &[reason])),
                };
                if matches!(self.shot, shot::Phase::Note { .. }) {
                    return text_input::focus(NOTE_ID.clone());
                }
            }
            Message::NoteInput(value) => {
                if let shot::Phase::Note { text, .. } = &mut self.shot {
                    *text = value.chars().take(shot::NOTE_LIMIT).collect();
                }
            }
            Message::NoteDone => return self.finish_note(true),
            Message::Escape(id) => {
                let shown = if self.window_mode { self.core.main_window_id() == Some(id) } else { self.popup == Some(id) };
                match esc_step(EscState {
                    shown,
                    note: self.shot.open(),
                    rename: !self.editing.is_empty(),
                    fritz_form: self.fritz_form_open,
                    menus: self.language_open || self.corners_open || self.chooser,
                    filter: !self.filter.is_empty(),
                }) {
                    EscStep::Note => return self.finish_note(false),
                    EscStep::Rename => return self.update(Message::CancelRename),
                    EscStep::FritzForm => self.close_fritz_form(),
                    EscStep::Menus => { self.language_open = false; self.corners_open = false; self.chooser = false; }
                    EscStep::Filter => self.filter.clear(),
                    // Like the window's own close (the tray keeps running); the popup like a click on the icon.
                    EscStep::Close if self.window_mode => return self.update(Message::CloseRequested(id)),
                    EscStep::Close => return self.update(Message::TogglePopup),
                    EscStep::Nothing => {}
                }
            }
            Message::NoteSaved(result) => match result {
                Ok(()) => { if self.shot == shot::Phase::Noting { self.shot = shot::Phase::Idle; } }
                Err(reason) => self.shot = shot::Phase::Error(trf("The note could not be saved: {0}", &[reason])),
            },
            Message::SetShotKey(on) => {
                self.setting_error = write_pref("screenshot_key", Some(on.into())).is_err();
                self.shot_key = on;
            }
            Message::ToggleCorners => { self.corners_open = !self.corners_open; self.language_open = false; }
            Message::Corners(choice) => {
                let choice = corners::normalize(&choice);
                // "Like the desktop" is the default: no key in the settings file.
                let value = (choice != "desktop").then(|| Value::from(choice));
                self.setting_error = write_pref("corners", value).is_err();
                self.corners = choice.to_string();
                corners::set_current(choice);
                self.corners_open = false;
                if self.window_mode { return corners::set_app_theme(choice); }
            }
            Message::Language(code) => {
                self.language_error = i18n::save_language(&code).is_err();
                self.send_json(json!({"cmd": "lang", "lang": i18n::language()}));
                if self.window_mode {
                    self.set_header_title(tr(APP_NAME).into());
                    return self.set_window_title(tr(APP_NAME).into(), self.core.main_window_id().unwrap());
                }
            }
            Message::Ready(commands) => {
                self.commands = Some(commands);
                self.send_json(json!({"cmd": "lang", "lang": i18n::language()}));
                if self.window_mode || self.popup.is_some() {
                    self.send("open");
                }
            }
            Message::Line(data) => {
                // Settings changed in another interface (window, applet) take effect here too.
                self.shot_key = shot_key_setting();
                let corners = corners_setting();
                let restyle = corners != self.corners;
                if restyle { corners::set_current(&corners); self.corners = corners; }
                let restyle = if restyle && self.window_mode { corners::set_app_theme(&self.corners) } else { Task::none() };
                let language = i18n::language();
                let language_changed = language != self.last_language;
                if language_changed {
                    self.send_json(json!({"cmd": "lang", "lang": language}));
                    self.last_language = language;
                }
                if s(&data, "event") == "scanning" {
                    self.scan_busy = true;
                } else if data.get("error").is_none() {
                    self.scan_busy = false;
                    self.rescan_pending = false;
                    self.take_snapshot(data);
                }
                if language_changed && self.window_mode {
                    self.set_header_title(tr(APP_NAME).into());
                    let title = self.set_window_title(tr(APP_NAME).into(), self.core.main_window_id().unwrap());
                    return Task::batch([title, restyle]);
                }
                return restyle;
            }
            Message::Exited => {
                self.commands = None;
                self.scan_busy = false;
                self.rescan_pending = false;
            }
            Message::Pulse => self.pulse_dim = !self.pulse_dim,
            Message::WindowTick => {
                if CLOSE_WINDOW.load(Ordering::Relaxed) { self.save_size(true); return cosmic::iced::exit(); }
                if self.resize.waiting() { self.save_size(false); }
                // The desktop's colours and corners (Maprisma Theme can change them any time);
                // look::refresh re-reads at most every 5 s.
                if maprisma_wlan_monitor::look::refresh() { return corners::set_app_theme(&self.corners); }
            }
            Message::Tick => self.flash.retain(|_, at| at.elapsed() < FLASH),
            Message::Rectangle(update) => match update {
                RectangleUpdate::Rectangle(r) => self.rectangle = r.1,
                RectangleUpdate::Init(tracker) => self.tracker = Some(tracker),
            },
            Message::CloseRequested(id) => {
                if self.window_mode {
                    self.save_size(true);
                    // A typed note is kept: written before the window goes.
                    if let shot::Phase::Note { path, text, .. } = std::mem::take(&mut self.shot) {
                        let _ = shot::save_note(&path, SHOT_FOLDER, &text);
                    }
                    return cosmic::iced::exit();
                }
                if self.popup == Some(id) {
                    self.popup = None;
                    self.popup_closed();
                    return self.shot_popup_closed();
                }
            }
            Message::TogglePopup => {
                if self.window_mode { return Task::none(); }
                if let Some(popup) = self.popup.take() {
                    self.popup_closed();
                    return Task::batch([self.shot_popup_closed(), destroy_popup(popup)]);
                }
                self.send("open");
                let focus = if matches!(self.shot, shot::Phase::Note { .. }) {
                    text_input::focus(NOTE_ID.clone())
                } else {
                    Task::none()
                };
                return Task::batch([cosmic::surface::surface_task(cosmic::surface::action::app_popup(
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
                )), focus]);
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
                        return Task::batch([self.shot_popup_closed(), destroy_popup(popup)]);
                    }
                }
            }
            Message::OpenWifiSettings => {
                // The applet runs only in COSMIC's panel; a window on whichever desktop it is on.
                let desktop = if self.window_mode { maprisma_wlan_monitor::look::desktop() } else { Desktop::Cosmic };
                if let Some(argv) = wifi_settings_command(desktop, in_path) {
                    spawn_detached(argv);
                }
                if let Some(popup) = self.popup.take() {
                    self.popup_closed();
                    return Task::batch([self.shot_popup_closed(), destroy_popup(popup)]);
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
        if self.window_mode { return corners::apply_window(&self.corners, self.popup_content()); }
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
        if self.window_mode { return corners::apply_window(&self.corners, self.popup_content()); }

        corners::apply(&self.corners, self.core.applet.popup_container(self.popup_content()))
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        if self.window_mode { None } else { Some(cosmic::applet::style()) }
    }
}

// ------------------------------------------------------------------------- processes

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

/// The Wi-Fi settings for this desktop: COSMIC's or Plasma's own; elsewhere (Hyprland) the first
/// one installed: Omarchy's Wi-Fi launcher, NetworkManager's editor, Plasma's, COSMIC's.
fn wifi_settings_command(desktop: Desktop, installed: impl Fn(&str) -> bool) -> Option<&'static [&'static str]> {
    const COSMIC: &[&str] = &["cosmic-settings", "wireless"];
    const KDE: &[&str] = &["systemsettings", "kcm_networkmanagement"];
    const OMARCHY: &[&str] = &["omarchy-launch-wifi"];
    const NM: &[&str] = &["nm-connection-editor"];
    match desktop {
        Desktop::Cosmic => Some(COSMIC),
        Desktop::Kde => Some(KDE),
        Desktop::Hyprland | Desktop::Other => [OMARCHY, NM, KDE, COSMIC].into_iter().find(|argv| installed(argv[0])),
    }
}

/// An executable of this name in PATH.
fn in_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_settings_follow_the_desktop() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        assert_eq!(wifi_settings_command(Desktop::Cosmic, none), Some(&["cosmic-settings", "wireless"][..]));
        assert_eq!(wifi_settings_command(Desktop::Kde, none), Some(&["systemsettings", "kcm_networkmanagement"][..]));
        assert_eq!(wifi_settings_command(Desktop::Hyprland, all), Some(&["omarchy-launch-wifi"][..]));
        assert_eq!(
            wifi_settings_command(Desktop::Hyprland, |n| n == "nm-connection-editor" || n == "systemsettings"),
            Some(&["nm-connection-editor"][..])
        );
        assert_eq!(
            wifi_settings_command(Desktop::Other, |n| n == "cosmic-settings"),
            Some(&["cosmic-settings", "wireless"][..])
        );
        assert_eq!(wifi_settings_command(Desktop::Hyprland, none), None);
        assert!(in_path("sh"));
        assert!(!in_path("maprisma-no-such-program"));
    }

    #[test]
    fn esc_closes_the_innermost_thing_first() {
        let all = EscState { shown: true, note: true, rename: true, fritz_form: true, menus: true, filter: true };
        assert_eq!(esc_step(all), EscStep::Note);
        assert_eq!(esc_step(EscState { note: false, ..all }), EscStep::Rename);
        assert_eq!(esc_step(EscState { note: false, rename: false, ..all }), EscStep::FritzForm);
        let menus = EscState { shown: true, menus: true, filter: true, ..Default::default() };
        assert_eq!(esc_step(menus), EscStep::Menus);
        assert_eq!(esc_step(EscState { menus: false, ..menus }), EscStep::Filter);
        assert_eq!(esc_step(EscState { shown: true, ..Default::default() }), EscStep::Close);
        // Not from the window or the open popup (e.g. the panel button): nothing.
        assert_eq!(esc_step(EscState { shown: false, ..all }), EscStep::Nothing);
    }
}
