//! The node this app talks to, remembered between starts.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Settings {
    pub node_url: String,
    pub token: String,
    /// The `host:port` this machine's player was last registered under.
    #[serde(default)]
    pub player_address: String,
    /// The node's stable id (`/v1/info`), to find it again on the LAN when its address changes.
    #[serde(default)]
    pub node_id: String,
}

impl Settings {
    pub fn new(node_url: &str, token: &str) -> Self {
        Settings {
            node_url: normalize_url(node_url),
            token: token.trim().to_string(),
            player_address: String::new(),
            node_id: String::new(),
        }
    }

    /// `scheme://host:port` of the node, as the web UI's page origin.
    pub fn origin(&self) -> Option<String> {
        let u = url::Url::parse(&self.node_url).ok()?;
        Some(u.origin().ascii_serialization())
    }
}

/// A typed address becomes a URL: `192.168.1.10:7788` → `http://192.168.1.10:7788`,
/// trailing slashes dropped.
pub fn normalize_url(input: &str) -> String {
    let s = input.trim().trim_end_matches('/');
    if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

pub fn load(path: &Path) -> Option<Settings> {
    let s: Settings = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (!s.node_url.trim().is_empty()).then_some(s)
}

/// Where the wry shell kept its settings, so an upgrade keeps the connection.
pub fn legacy_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("pubky-music").join("desktop.json"))
}

pub fn save(path: &Path, s: Option<&Settings>) -> anyhow::Result<()> {
    let Some(s) = s else {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(s)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_become_urls() {
        assert_eq!(normalize_url(" 192.168.1.10:7788/ "), "http://192.168.1.10:7788");
        assert_eq!(
            normalize_url("https://music.example.org/"),
            "https://music.example.org"
        );
    }

    #[test]
    fn origin_has_the_port() {
        let s = Settings::new("http://nas.local:7788/", "t");
        assert_eq!(s.origin().as_deref(), Some("http://nas.local:7788"));
        let s = Settings::new("https://music.example.org", "t");
        assert_eq!(s.origin().as_deref(), Some("https://music.example.org"));
    }

    #[test]
    fn round_trip_and_forget() {
        let dir = std::env::temp_dir().join(format!("pm-desktop-test-{}", std::process::id()));
        let path = dir.join("desktop.json");
        let s = Settings::new("http://a:1", "tok");
        save(&path, Some(&s)).unwrap();
        assert_eq!(load(&path), Some(s));
        // The wry shell's file had no player_address; it still loads.
        std::fs::write(&path, br#"{"node_url":"http://b:2","token":"x"}"#).unwrap();
        assert_eq!(load(&path).unwrap().node_url, "http://b:2");
        save(&path, None).unwrap();
        assert_eq!(load(&path), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
