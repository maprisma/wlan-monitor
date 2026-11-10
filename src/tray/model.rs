use maprisma_wlan_monitor::i18n::{self, tr, trf};
use maprisma_wlan_monitor::APP_NAME;
use serde_json::Value;
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Open,
    Quit,
    Activate,
    Language(u8),
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
        item(1, trf("Open {0}", &[tr(APP_NAME).into()]), Some(Action::Open)),
        item(2, tr("Quit tray").into(), Some(Action::Quit)),
    ];
    let current = i18n::setting();
    let mut languages = item(3, tr("Language").into(), None);
    languages.children = i18n::choices().into_iter().enumerate().map(|(index, (code, label))| {
        item(10 + index as i32, if current == code { format!("✓ {label}") } else { label }, Some(Action::Language(index as u8)))
    }).collect();
    root.children.push(languages);
    root
}
pub fn tooltip(data: &Value) -> String {
    if data["active"].as_bool() != Some(true) {
        return tr("Not connected").into();
    }
    let mut parts = vec![
        data["ssid"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| data["iface"].as_str())
            .unwrap_or(tr("Connected"))
            .to_string(),
    ];
    if let Some(q) = data["quality"].as_f64().filter(|q| q.is_finite()) {
        let word = if q >= 70.0 {
            tr("Excellent")
        } else if q >= 50.0 {
            tr("Good")
        } else if q >= 30.0 {
            tr("Fair")
        } else {
            tr("Weak")
        };
        parts.push(trf("{0} signal", &[word.into()]));
    }
    if let Some(vpn) = data["vpn"].as_object() {
        parts.push(trf(
            "VPN: {0}", &[
            vpn.get("name").and_then(Value::as_str).unwrap_or(tr("active")).into()]
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
        assert_eq!(m.children.len(), 3);
        assert_eq!(m.find(1).unwrap().label, trf("Open {0}", &[tr(APP_NAME).into()]));
        assert_eq!(m.activate(1), Some(Action::Open));
        assert_eq!(m.activate(2), Some(Action::Quit));
        assert_eq!(m.activate(0), None);
        assert_eq!(m.activate(99), None);
    }
    #[test]
    fn connection_tooltips() {
        assert_eq!(tooltip(&Value::Null), tr("Not connected"));
        assert_eq!(tooltip(&json!({"active":true,"iface":"eth0"})), "eth0");
        for (q, word) in [
            (0, tr("Weak")),
            (29, tr("Weak")),
            (30, tr("Fair")),
            (49, tr("Fair")),
            (50, tr("Good")),
            (69, tr("Good")),
            (70, tr("Excellent")),
            (100, tr("Excellent")),
        ] {
            assert_eq!(
                tooltip(&json!({"active":true,"ssid":"Home","quality":q})),
                format!("Home · {}", trf("{0} signal", &[word.into()]))
            );
        }
        assert_eq!(
            tooltip(&json!({"active":true,"ssid":"Home","vpn":{"name":"WireGuard"}})),
            format!("Home · {}", trf("VPN: {0}", &["WireGuard".into()]))
        );
    }
}
