use maprisma_wlan_monitor::i18n::{self, tr};
mod model;
use maprisma_wlan_monitor::{APP_NAME, ICON, WINDOW_BUS, script_path};
use model::Action;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc};
use zbus::{
    Connection,
    object_server::SignalEmitter,
    zvariant::{OwnedObjectPath, OwnedValue, Value as Variant},
};

type Props = HashMap<String, OwnedValue>;
type Layout = (i32, Props, Vec<OwnedValue>);
type Pixmaps = Vec<(i32, i32, Vec<u8>)>;
struct State {
    tooltip: String,
    menu: model::Item,
    revision: u32,
}
impl State {
    fn status(&self) -> &'static str {
        "Active"
    }
    fn tooltip(&self) -> String {
        self.tooltip.clone()
    }
}
type Shared = Arc<Mutex<State>>;
#[derive(Clone)]
struct Menu {
    state: Shared,
    tx: mpsc::UnboundedSender<Action>,
}
fn owned<T: Into<Variant<'static>>>(value: T) -> OwnedValue {
    OwnedValue::try_from(value.into()).unwrap()
}
fn properties(item: &model::Item, names: &[String]) -> Props {
    let mut props = Props::new();
    props.insert("label".into(), owned(item.label.clone()));
    props.insert("enabled".into(), owned(item.enabled));
    props.insert("visible".into(), owned(true));
    if !item.children.is_empty() {
        props.insert("children-display".into(), owned("submenu"));
    }
    if item.separator {
        props.insert("type".into(), owned("separator"));
    }
    if !names.is_empty() {
        props.retain(|key, _| names.contains(key));
    }
    props
}
fn layout(item: &model::Item, depth: i32, names: &[String]) -> Layout {
    (
        item.id,
        properties(item, names),
        if depth == 0 {
            vec![]
        } else {
            item.children
                .iter()
                .map(|c| {
                    OwnedValue::try_from(Variant::new(layout(
                        c,
                        if depth < 0 { -1 } else { depth - 1 },
                        names,
                    )))
                    .unwrap()
                })
                .collect()
        },
    )
}
impl Menu {
    async fn dispatch(&self, id: i32, event: &str) -> bool {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        let Some(item) = menu.find(id) else {
            return false;
        };
        if event == "clicked" {
            if let Some(action) = menu.activate(item.id) {
                let _ = self.tx.send(action);
            }
        }
        true
    }
}
#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        vec![]
    }
    async fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        let item = menu
            .find(parent_id)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Unknown menu ID".into()))?;
        Ok((s.revision, layout(item, recursion_depth, &property_names)))
    }
    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, Props)> {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        ids.iter()
            .filter_map(|id| {
                menu.find(*id)
                    .map(|i| (*id, properties(i, &property_names)))
            })
            .collect()
    }
    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        menu.find(id)
            .and_then(|i| properties(i, &[]).remove(&name))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Unknown menu property".into()))
    }
    async fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        self.dispatch(id, &event_id).await;
    }
    async fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        let mut errors = vec![];
        for (id, event, _, _) in events {
            if !self.dispatch(id, &event).await {
                errors.push(id);
            }
        }
        errors
    }
    async fn about_to_show(&self, _id: i32) -> bool {
        false
    }
    async fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        (
            vec![],
            ids.into_iter()
                .filter(|id| menu.find(*id).is_none())
                .collect(),
        )
    }
    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}
struct Tray {
    state: Shared,
    tx: mpsc::UnboundedSender<Action>,
}
#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Tray {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "maprisma-wlan-monitor"
    }
    #[zbus(property)]
    fn title(&self) -> &str {
        tr(APP_NAME)
    }
    #[zbus(property)]
    async fn status(&self) -> &str {
        self.state.lock().await.status()
    }
    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        ICON
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        vec![]
    }
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Pixmaps {
        vec![]
    }
    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ICON
    }
    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Pixmaps {
        vec![]
    }
    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    async fn tool_tip(&self) -> (String, Pixmaps, String, String) {
        (
            ICON.into(),
            vec![],
            tr(APP_NAME).into(),
            self.state.lock().await.tooltip(),
        )
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/MenuBar").unwrap()
    }
    fn activate(&self, _x: i32, _y: i32) {
        let _ = self.tx.send(Action::Activate);
    }
    fn secondary_activate(&self, _x: i32, _y: i32) {
        let _ = self.tx.send(Action::Activate);
    }
    fn context_menu(&self, _x: i32, _y: i32) {} // The host displays the exported Menu.
    fn scroll(&self, _delta: i32, _orientation: String) {}
    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
// Read only the collector's interface/signal/VPN helpers. Never construct its
// Monitor or run --once: the tray must not start discovery or FRITZ!Box polling.
async fn read_status() -> String {
    let Some(script) = script_path("wlan-monitor.py") else {
        return tr("Collector not installed").into();
    };
    let code = r#"import importlib.util,json,sys
spec=importlib.util.spec_from_file_location('wlan',sys.argv[1])
m=importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
n=m.detect_interface({})
d={'active': bool(n)}
if n:
 d.update(m.wireless_details(n['iface']) if n['is_wireless'] else {})
 d['iface']=n['iface']
 d['vpn']=m.detect_vpn(n['iface'])
print(json.dumps(d))
"#;
    let mut command = tokio::process::Command::new("python3");
    command
        .args(["-I", "-B", "-c", code])
        .arg(script)
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(8), command.output()).await {
        Ok(Ok(output)) if output.status.success() => serde_json::from_slice(&output.stdout)
            .map(|v| model::tooltip(&v))
            .unwrap_or_else(|_| tr("Status unavailable").into()),
        _ => tr("Status unavailable").into(),
    }
}
/// A window this tray started is still running (it may not have claimed its name yet).
static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The open window's connection. A window this tray just started gets up to 2 s to claim its
/// name, so a second click while it is starting closes it instead of being lost.
async fn window_owner(dbus: &zbus::fdo::DBusProxy<'_>) -> Option<zbus::names::OwnedUniqueName> {
    for _ in 0..40 {
        if let Ok(owner) = dbus.get_name_owner(WINDOW_BUS.try_into().ok()?).await {
            return Some(owner);
        }
        if !STARTED.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}
async fn window_action(
    connection: &Connection,
    toggle: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let dbus = zbus::fdo::DBusProxy::new(connection).await?;
    if let Some(owner) = window_owner(&dbus).await {
        if toggle {
            let proxy = zbus::Proxy::new(connection, owner.as_str(), "/Window", WINDOW_BUS).await?;
            proxy.call::<_, _, ()>("Close", &()).await?;
        }
    } else {
        let executable = std::env::current_exe()?.with_file_name("maprisma-wlan-monitor");
        let mut child = tokio::process::Command::new(executable)
            .arg("--window")
            .stdin(std::process::Stdio::null())
            .spawn()?;
        STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            let _ = child.wait().await;
            STARTED.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let initial = State {
        tooltip: tr("Checking connection…").into(),
        menu: model::menu(),
        revision: 1,
    };
    let state = Arc::new(Mutex::new(initial));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let connection = zbus::connection::Builder::session()?
        .serve_at(
            "/StatusNotifierItem",
            Tray {
                state: state.clone(),
                tx: tx.clone(),
            },
        )?
        .serve_at(
            "/MenuBar",
            Menu {
                state: state.clone(),
                tx,
            },
        )?
        .build()
        .await?;
    // One tray per session: a second one ends here (zbus would otherwise
    // queue the name request and keep running).
    match connection
        .request_name_with_flags(
            "io.github.ubruckhaus.CosmicMaprismaWlanMonitor.Tray",
            zbus::fdo::RequestNameFlags::DoNotQueue.into(),
        )
        .await
    {
        Ok(_) => {}
        Err(zbus::Error::NameTaken) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    // Checking the unique owner also handles Plasma starting after us and watcher restarts.
    let watcher_connection = connection.clone();
    tokio::spawn(async move {
        let mut registered = String::new();
        loop {
            let dbus = zbus::fdo::DBusProxy::new(&watcher_connection).await;
            let owner = match dbus {
                Ok(p) => p
                    .get_name_owner("org.kde.StatusNotifierWatcher".try_into().unwrap())
                    .await
                    .ok()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            if owner.is_empty() {
                registered.clear();
            } else if owner != registered {
                if let Ok(proxy) = zbus::Proxy::new(
                    &watcher_connection,
                    owner.as_str(),
                    "/StatusNotifierWatcher",
                    "org.kde.StatusNotifierWatcher",
                )
                .await
                {
                    if proxy
                        .call::<_, _, ()>(
                            "RegisterStatusNotifierItem",
                            &(watcher_connection.unique_name().unwrap().as_str(),),
                        )
                        .await
                        .is_ok()
                    {
                        registered = owner;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    let (status_tx, mut status_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        loop {
            if status_tx.send(read_status().await).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    loop {
        tokio::select! {
            Some(tooltip) = status_rx.recv() => {
                let mut s = state.lock().await;
                let menu = model::menu();
                if s.menu != menu {
                    s.menu = menu; s.revision += 1;
                    Menu::layout_updated(&SignalEmitter::new(&connection, "/MenuBar")?, s.revision, 0).await?;
                }
                if s.tooltip != tooltip {
                    s.tooltip = tooltip;
                    drop(s);
                    Tray::new_tool_tip(&SignalEmitter::new(&connection, "/StatusNotifierItem")?).await?;
                }
            }
            Some(action) = rx.recv() => {
                match action {
                    Action::Language(index) => {
                        if let Some((code, _)) = i18n::choices().get(index as usize) {
                            let mut s = state.lock().await;
                            if i18n::save_language(code).is_err() {
                                s.tooltip = tr("Could not save the language setting.").into();
                            }
                            s.menu = model::menu(); s.revision += 1;
                            Menu::layout_updated(&SignalEmitter::new(&connection, "/MenuBar")?, s.revision, 0).await?;
                            Tray::new_tool_tip(&SignalEmitter::new(&connection, "/StatusNotifierItem")?).await?;
                        }
                    }
                    Action::Quit => return Ok(()),
                    Action::Open | Action::Activate => {
                        if let Err(error) = window_action(&connection, action == Action::Activate).await {
                            eprintln!("Window: {error}");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_signature_and_filter() {
        let menu = model::menu();
        assert_eq!(
            Variant::new(layout(&menu, -1, &[]))
                .value_signature()
                .to_string(),
            "(ia{sv}av)"
        );
        assert!(layout(&menu, 0, &[]).2.is_empty());
        assert_eq!(layout(&menu, 1, &[]).2.len(), 3);
        let props = properties(menu.find(1).unwrap(), &["label".into()]);
        assert_eq!(props.len(), 1);
    }
    #[tokio::test]
    async fn menu_events() {
        let state = Arc::new(Mutex::new(State {
            tooltip: String::new(),
            menu: model::menu(),
            revision: 1,
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let menu = Menu { state, tx };
        menu.event(1, "hovered".into(), owned(0i32), 0).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(
            menu.event_group(vec![
                (1, "clicked".into(), owned(0i32), 0),
                (2, "clicked".into(), owned(0i32), 0),
                (99, "clicked".into(), owned(0i32), 0)
            ])
            .await,
            vec![99]
        );
        assert_eq!(rx.try_recv().unwrap(), Action::Open);
        assert_eq!(rx.try_recv().unwrap(), Action::Quit);
    }
}
