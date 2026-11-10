use serde_json::Value;
use std::collections::BTreeSet;

#[test]
fn all_literal_keys_have_seven_translations_and_bundle_is_current() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut sources = Vec::new();
    rust_sources(&root.join("src"), &mut sources);
    let mut keys = BTreeSet::new();
    for source in sources {
        let text = std::fs::read_to_string(source).unwrap();
        // Literal gettext calls. JSON and Rust share the escapes used by our keys.
        for marker in ["tr(", "trf("] {
            for tail in text.split(marker).skip(1) {
                let tail = tail.trim_start();
                if tail.starts_with('"') {
                    let key = serde_json::Deserializer::from_str(tail)
                        .into_iter::<String>()
                        .next()
                        .unwrap()
                        .unwrap();
                    keys.insert(key);
                }
            }
        }
        // The two long FRITZ!Box help texts are constants passed to tr().
        for constant in ["FRITZ_EXPLAIN", "FRITZ_HELP", "APP_NAME"] {
            if let Some(tail) = text.split(&format!("const {constant}: &str = ")).nth(1) {
                keys.insert(
                    serde_json::Deserializer::from_str(tail)
                        .into_iter::<String>()
                        .next()
                        .unwrap()
                        .unwrap(),
                );
            }
        }
    }
    let desktop = std::fs::read_to_string(
        root.join("data/io.github.ubruckhaus.CosmicMaprismaWlanMonitor.desktop"),
    )
    .unwrap();
    for line in desktop
        .lines()
        .filter(|l| l.starts_with("Name=") || l.starts_with("Comment="))
    {
        keys.insert(line.split_once('=').unwrap().1.into());
    }
    let bundle: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("translations/bundled.json")).unwrap(),
    )
    .unwrap();
    for code in ["en", "de", "fr", "es", "it", "pt", "nl", "pl"] {
        let mut catalog: serde_json::Map<String, Value> = serde_json::from_str(
            &std::fs::read_to_string(root.join(format!("translations/{code}.json"))).unwrap(),
        )
        .unwrap();
        let extra: serde_json::Map<String, Value> = serde_json::from_str(
            &std::fs::read_to_string(root.join(format!("translations/extra/{code}.json"))).unwrap(),
        )
        .unwrap();
        assert!(
            extra.keys().all(|k| !catalog.contains_key(k)),
            "extra overrides original: {code}"
        );
        catalog.extend(extra);
        for key in &keys {
            assert!(
                catalog
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty()),
                "{code} missing {key:?}"
            );
            let translated = catalog[key].as_str().unwrap();
            for i in 0..8 {
                let placeholder = format!("{{{i}}}");
                assert_eq!(
                    key.contains(&placeholder),
                    translated.contains(&placeholder),
                    "placeholder mismatch in {code}: {key}"
                );
            }
        }
        assert_eq!(
            bundle[code],
            Value::Object(catalog),
            "run tools/gen-translations"
        );
        if code != "en" {
            for field in ["Name", "Comment"] {
                assert!(
                    desktop
                        .lines()
                        .any(|l| l.starts_with(&format!("{field}[{code}]=")))
                );
            }
        }
    }
}

#[test]
fn settings_round_trip_and_english_fallback() {
    use maprisma_wlan_monitor::i18n;
    if std::env::var_os("MAPRISMA_I18N_TEST_CHILD").is_none() {
        let dir = std::env::temp_dir().join(format!("maprisma-i18n-{}", std::process::id()));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "settings_round_trip_and_english_fallback"])
            .env("MAPRISMA_I18N_TEST_CHILD", "1")
            .env("XDG_CONFIG_HOME", &dir)
            .env("LC_ALL", "fr_FR.UTF-8")
            .status()
            .unwrap();
        let _ = std::fs::remove_dir_all(dir);
        assert!(status.success());
        return;
    }
    assert_eq!(i18n::setting(), "auto");
    assert_eq!(i18n::language(), "fr");
    assert_eq!(i18n::tr("Language"), "Langue");
    i18n::save_language("pl").unwrap();
    assert_eq!(i18n::language(), "pl");
    assert_eq!(i18n::trf("{0} devices", &["22".into()]), "22 urządzenia");
    assert_eq!(i18n::trf("{0} devices", &["12".into()]), "12 urządzeń");
    assert_eq!(
        i18n::tr("This key deliberately has no translation"),
        "This key deliberately has no translation"
    );
    assert_eq!(i18n::trf("Open {0}", &["{1}".into()]), "Otwórz {1}");
    assert!(i18n::save_language("unknown").is_err());
    i18n::save_language("auto").unwrap();
    assert_eq!(i18n::language(), "fr");
}
