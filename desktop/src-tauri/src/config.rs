//! Settings, endpoint rules, and vocabulary. Same file format and paths as the Python client,
//! so both read ~/.config/voicecode/config.json on Linux.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const KEY_ENV: &str = "VOICECODE_API_KEY";
pub const ENDPOINT_ENV: &str = "VOICECODE_ENDPOINT";

/// Tray label -> stored value. "" language means auto-detect (no ?language= param).
pub const LANGUAGES: &[(&str, &str)] = &[
    ("English (en-US)", "en-US"),
    ("Chinese (zh-CN)", "zh-CN"),
    ("Auto-detect", ""),
];
/// Tray label -> stored value (X11 keysym names, kept for compatibility with the Python client).
/// Modifier keys first: pressed alone they do nothing in other apps. F9 is a shortcut in some apps.
pub const PTT_KEYS: &[(&str, &str)] = &[
    ("Right Ctrl", "Control_R"),
    ("Right Alt / Option", "Alt_R"),
    ("Right Shift", "Shift_R"),
    ("F9", "F9"),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub language: String,
    pub ptt_key: String,
    pub paused: bool,
    /// "" = use $VOICECODE_ENDPOINT
    pub endpoint: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            language: "en-US".into(),
            ptt_key: "Control_R".into(),
            paused: false,
            endpoint: String::new(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("voicecode")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

pub fn vocab_file() -> PathBuf {
    config_dir().join("vocabulary.txt")
}

/// Missing or broken file -> defaults; unknown or invalid values are dropped one by one,
/// so a hand-edited typo cannot stop the app from starting.
pub fn load_config(path: &Path) -> Config {
    let mut cfg = Config::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return cfg;
    };
    let saved: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Ignoring unreadable {}: {e}", path.display());
            return cfg;
        }
    };
    if let Some(v) = saved["language"]
        .as_str()
        .filter(|v| LANGUAGES.iter().any(|(_, x)| x == v))
    {
        cfg.language = v.into();
    }
    if let Some(v) = saved["ptt_key"]
        .as_str()
        .filter(|v| PTT_KEYS.iter().any(|(_, x)| x == v))
    {
        cfg.ptt_key = v.into();
    }
    if let Some(v) = saved["paused"].as_bool() {
        cfg.paused = v;
    }
    if let Some(v) = saved["endpoint"].as_str().filter(|v| valid_endpoint(v)) {
        cfg.endpoint = v.into();
    }
    cfg
}

/// Write to a temp file, then rename: a crash never leaves a half-written config.
pub fn save_config(cfg: &Config, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg)? + "\n")?;
    std::fs::rename(&tmp, path)
}

/// wss:// anywhere; plain ws:// only to localhost, because the API key rides in a header.
pub fn valid_endpoint(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    match (u.scheme(), u.host_str()) {
        ("wss", Some(h)) => !h.is_empty(),
        ("ws", Some(h)) => matches!(h, "localhost" | "127.0.0.1" | "[::1]"),
        _ => false,
    }
}

pub fn resolve_endpoint(cfg: &Config, env_endpoint: &str) -> String {
    if cfg.endpoint.is_empty() {
        env_endpoint.trim().to_string()
    } else {
        cfg.endpoint.clone()
    }
}

pub fn stream_url(language: &str, endpoint: &str) -> String {
    if language.is_empty() {
        return endpoint.to_string();
    }
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    let lang: String = url::form_urlencoded::byte_serialize(language.as_bytes()).collect();
    format!("{endpoint}{sep}language={lang}")
}

/// One phrase per line; blank lines and `#` comments are skipped.
pub fn load_vocabulary(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "voicecode-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn config_load_save() {
        let d = tmp_dir();
        let f = d.join("config.json");
        assert_eq!(load_config(&f), Config::default()); // missing file
        std::fs::write(&f, "{not json").unwrap();
        assert_eq!(load_config(&f), Config::default()); // corrupt file
        std::fs::write(
            &f,
            r#"{"language": "fr-FR", "ptt_key": "F9", "paused": "yes"}"#,
        )
        .unwrap();
        assert_eq!(
            load_config(&f),
            Config {
                ptt_key: "F9".into(),
                ..Config::default()
            }
        );
        std::fs::write(&f, r#"{"endpoint": "ws://evil.example.com/s"}"#).unwrap(); // insecure -> no override
        assert_eq!(load_config(&f).endpoint, "");

        let saved = Config {
            language: "".into(),
            ptt_key: "Shift_R".into(),
            paused: true,
            endpoint: "wss://my.host/s".into(),
        };
        let f2 = d.join("sub").join("config.json");
        save_config(&saved, &f2).unwrap();
        assert_eq!(load_config(&f2), saved);
    }

    #[test]
    fn endpoints() {
        for ok in [
            "wss://api.example.com/v1/audio/stream",
            "ws://localhost:8000/s",
            "ws://127.0.0.1/s",
        ] {
            assert!(valid_endpoint(ok), "{ok}");
        }
        for bad in [
            "ws://api.example.com/s",
            "https://api.example.com",
            "wss://",
            "not a url",
            "",
            "wss://[bad",
        ] {
            assert!(!valid_endpoint(bad), "{bad}");
        }
        let ep = "wss://stt.example.com/v1/stream";
        assert_eq!(stream_url("", ep), ep);
        assert_eq!(stream_url("zh-CN", ep), format!("{ep}?language=zh-CN"));
        assert_eq!(
            stream_url("en-US", "wss://h/x?region=eu"),
            "wss://h/x?region=eu&language=en-US"
        );

        let cfg = Config::default();
        assert_eq!(resolve_endpoint(&cfg, ep), ep); // no override -> env
        let cfg = Config {
            endpoint: "wss://my.host/s".into(),
            ..Config::default()
        };
        assert_eq!(resolve_endpoint(&cfg, ep), "wss://my.host/s");
        assert_eq!(resolve_endpoint(&Config::default(), ""), "");
    }

    #[test]
    fn vocabulary() {
        let f = tmp_dir().join("vocabulary.txt");
        std::fs::write(&f, "# comment\n kubectl \n\nuseEffect\n").unwrap();
        assert_eq!(load_vocabulary(&f), ["kubectl", "useEffect"]);
    }
}
