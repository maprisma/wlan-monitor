//! Bundled English-key catalogs and the app's independent interface preference.
use serde_json::Value;
use std::{path::PathBuf, sync::LazyLock};

pub const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "Deutsch"),
    ("fr", "Français"),
    ("es", "Español"),
    ("it", "Italiano"),
    ("pt", "Português"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
];
static CATALOGS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../translations/bundled.json")).expect("bundled catalogs")
});
pub fn system_language(values: [&str; 3]) -> &'static str {
    let value = values.into_iter().find(|v| !v.is_empty()).unwrap_or("en");
    let code = value
        .split(['_', '.', '@', '-'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or("en", |(c, _)| *c)
}
pub fn system_code() -> &'static str {
    let values = ["LC_ALL", "LC_MESSAGES", "LANG"].map(|k| std::env::var(k).unwrap_or_default());
    system_language([&values[0], &values[1], &values[2]])
}
pub fn settings_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("ubruckhaus-wlan-monitor/maprisma-settings.json")
}
pub fn setting() -> String {
    std::fs::read(settings_path())
        .ok()
        .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
        .and_then(|v| v["language"].as_str().map(str::to_owned))
        .filter(|s| s == "auto" || LANGUAGES.iter().any(|(c, _)| c == s))
        .unwrap_or_else(|| "auto".into())
}
pub fn resolve(setting: &str, system: &str) -> String {
    if setting != "auto" && LANGUAGES.iter().any(|(c, _)| *c == setting) {
        setting.into()
    } else {
        system.into()
    }
}
pub fn language() -> String {
    resolve(&setting(), system_code())
}
pub fn save_language(code: &str) -> std::io::Result<()> {
    if code != "auto" && !LANGUAGES.iter().any(|(c, _)| *c == code) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "language",
        ));
    }
    let path = settings_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    value["language"] = code.into();
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(&value)?)?;
    std::fs::rename(temp, path)
}
pub fn tr(key: &'static str) -> &'static str {
    CATALOGS
        .get(language())
        .and_then(|v| v.get(key))
        .and_then(Value::as_str)
        .unwrap_or(key)
}
static PLURALS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../translations/plurals.json")).expect("bundled plurals")
});
fn plural_index(code: &str, n: u64, len: usize) -> usize {
    if code == "pl" && len == 3 {
        if n == 1 {
            0
        } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
            1
        } else {
            2
        }
    } else if n == 1 || ((code == "fr" || code == "pt") && n == 0) {
        0
    } else {
        len - 1
    }
}
pub fn trf(key: &'static str, args: &[String]) -> String {
    // Replace only placeholders from the template, never placeholders inside data.
    let code = language();
    let forms = PLURALS
        .get(&code)
        .and_then(|v| v.get(key))
        .and_then(Value::as_array);
    let template = match (forms, args.first().and_then(|v| v.parse::<u64>().ok())) {
        (Some(forms), Some(n)) if !forms.is_empty() => forms[plural_index(&code, n, forms.len())]
            .as_str()
            .unwrap_or(key),
        _ => tr(key),
    };
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(end) = rest.find('}') {
            if let Ok(index) = rest[1..end].parse::<usize>() {
                if let Some(value) = args.get(index) {
                    out.push_str(value);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        out.push('{');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}
pub fn choices() -> Vec<(String, String)> {
    let name = LANGUAGES
        .iter()
        .find(|(c, _)| *c == system_code())
        .unwrap()
        .1;
    let mut out = vec![("auto".into(), trf("System ({0})", &[name.into()]))];
    out.extend(
        LANGUAGES
            .iter()
            .map(|(c, n)| (c.to_string(), n.to_string())),
    );
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overrides_and_plural_forms() {
        assert_eq!(resolve("de", "fr"), "de");
        assert_eq!(resolve("auto", "fr"), "fr");
        assert_eq!(resolve("unknown", "fr"), "fr");
        for (n, index) in [(1, 0), (2, 1), (4, 1), (5, 2), (12, 2), (22, 1), (112, 2)] {
            assert_eq!(plural_index("pl", n, 3), index);
        }
        assert_eq!(plural_index("fr", 0, 2), 0);
        assert_eq!(plural_index("de", 0, 2), 1);
    }
    #[test]
    fn locale_precedence_and_fallback() {
        for (values, expected) in [
            (["de_DE.UTF-8", "fr", "pl"], "de"),
            (["", "fr-FR", "de"], "fr"),
            (["", "", "pl_PL@x"], "pl"),
            (["xx", "de", "fr"], "en"),
            (["C", "de", "fr"], "en"),
            (["POSIX", "de", "fr"], "en"),
            (["", "", ""], "en"),
        ] {
            assert_eq!(system_language(values), expected);
        }
    }
}
