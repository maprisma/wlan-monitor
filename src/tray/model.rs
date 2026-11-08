use maprisma_wlan_monitor::APP_NAME;
use serde_json::Value;
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Open,
    Quit,
    Activate,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    pub children: Vec<Item>,
    pub action: Option<Action>,
}
impl Item {
    pub fn find(&self, id: i32) -> Option<&Self> {
        if self.id == id {
            Some(self)
        } else {
            self.children.iter().find_map(|c| c.find(id))
        }
    }
    pub fn activate(&self, id: i32) -> Option<Action> {
        self.find(id).filter(|i| i.enabled).and_then(|i| i.action)
    }
}
pub fn menu() -> Item {
    let item = |id, label, action| Item {
        id,
        label,
        action,
        enabled: true,
        separator: false,
        children: vec![],
    };
    let mut root = item(0, String::new(), None);
    root.children = vec![
        item(1, format!("Open {APP_NAME}"), Some(Action::Open)),
        item(2, "Quit tray".into(), Some(Action::Quit)),
    ];
    root
}
pub fn tooltip(data: &Value) -> String {
    if data["active"].as_bool() != Some(true) {
        return "Disconnected".into();
    }
    let mut parts = vec![
        data["ssid"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| data["iface"].as_str())
            .unwrap_or("Connected")
            .to_string(),
    ];
    if let Some(q) = data["quality"].as_f64().filter(|q| q.is_finite()) {
        let word = if q >= 70.0 {
            "Excellent"
        } else if q >= 50.0 {
            "Good"
        } else if q >= 30.0 {
            "Fair"
        } else {
            "Weak"
        };
        parts.push(format!("{word} signal"));
    }
    if let Some(vpn) = data["vpn"].as_object() {
        parts.push(format!(
            "VPN: {}",
            vpn.get("name").and_then(Value::as_str).unwrap_or("active")
        ));
    }
    parts.join(" · ")
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn menu_actions() {
        let m = menu();
        assert_eq!(m.children.len(), 2);
        assert_eq!(m.find(1).unwrap().label, "Open Maprisma WLAN Monitor");
        assert_eq!(m.activate(1), Some(Action::Open));
        assert_eq!(m.activate(2), Some(Action::Quit));
        assert_eq!(m.activate(0), None);
        assert_eq!(m.activate(99), None);
    }
    #[test]
    fn connection_tooltips() {
        assert_eq!(tooltip(&Value::Null), "Disconnected");
        assert_eq!(tooltip(&json!({"active":true,"iface":"eth0"})), "eth0");
        for (q, word) in [
            (0, "Weak"),
            (29, "Weak"),
            (30, "Fair"),
            (49, "Fair"),
            (50, "Good"),
            (69, "Good"),
            (70, "Excellent"),
            (100, "Excellent"),
        ] {
            assert_eq!(
                tooltip(&json!({"active":true,"ssid":"Home","quality":q})),
                format!("Home · {word} signal")
            );
        }
        assert_eq!(
            tooltip(&json!({"active":true,"ssid":"Home","vpn":{"name":"WireGuard"}})),
            "Home · VPN: WireGuard"
        );
    }
}
