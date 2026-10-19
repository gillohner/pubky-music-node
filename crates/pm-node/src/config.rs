//! Node configuration: one TOML file, every section optional with defaults.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NodeConfig {
    /// Where the SQLite library and other state live.
    pub data_dir: PathBuf,
    /// Contact address for outbound User-Agent strings (MusicBrainz requires one).
    pub contact: String,
    pub library: LibraryConfig,
    pub pubky: PubkyConfig,
    pub api: ApiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LibraryConfig {
    /// Folders to scan. Mounted shares count as folders.
    pub folders: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PubkyConfig {
    /// `mainnet` or `testnet`.
    pub network: String,
    pub pinakes_base: String,
    pub nexus_base: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ApiConfig {
    pub listen: String,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            contact: String::new(),
            library: LibraryConfig::default(),
            pubky: PubkyConfig::default(),
            api: ApiConfig::default(),
        }
    }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self { folders: Vec::new() }
    }
}

impl Default for PubkyConfig {
    fn default() -> Self {
        Self {
            network: "mainnet".into(),
            pinakes_base: "https://8092--main--pinakes--gil.coder.riginode.xyz".into(),
            nexus_base: "https://nexus.staging.pubky.app".into(),
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { listen: "127.0.0.1:7788".into() }
    }
}

impl NodeConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn load_or_default(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            Some(p) if p.exists() => Self::load(p),
            Some(p) => anyhow::bail!("config file {} not found", p.display()),
            None => Ok(Self::default()),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }

    pub fn user_agent(&self) -> String {
        let contact = if self.contact.is_empty() { "no-contact-configured" } else { self.contact.as_str() };
        format!("pubky-music-node/{} ({contact})", env!("CARGO_PKG_VERSION"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_toml() {
        let cfg: NodeConfig = toml::from_str(
            r#"
contact = "ops@example.org"
[library]
folders = ["/music", "/mnt/nas/flac"]
[api]
listen = "0.0.0.0:7788"
"#,
        )
        .unwrap();
        assert_eq!(cfg.library.folders.len(), 2);
        assert_eq!(cfg.api.listen, "0.0.0.0:7788");
        assert_eq!(cfg.pubky.network, "mainnet");
        assert!(cfg.user_agent().contains("ops@example.org"));
    }

    #[test]
    fn default_roundtrip() {
        let cfg = NodeConfig::default();
        let text = toml::to_string(&cfg).unwrap();
        let back: NodeConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }
}
