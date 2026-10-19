//! Node configuration: one TOML file, every section optional with defaults.

use pm_pubky::PmPubkyConfig;
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
    /// Pinakes, Nexus and network settings (see `pm_pubky::PmPubkyConfig`).
    pub pubky: PmPubkyConfig,
    pub api: ApiConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LibraryConfig {
    /// Folders to scan. Mounted shares count as folders.
    pub folders: Vec<PathBuf>,
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
            pubky: PmPubkyConfig::default(),
            api: ApiConfig::default(),
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7788".into(),
        }
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

    /// The pm-pubky config with the node-level contact filled in.
    pub fn pubky_config(&self) -> PmPubkyConfig {
        let mut c = self.pubky.clone();
        if !self.contact.is_empty() {
            c.user_agent_contact = self.contact.clone();
        }
        c
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
        assert_eq!(cfg.pubky.network, pm_pubky::Network::Mainnet);
        assert!(cfg.pubky_config().user_agent().contains("ops@example.org"));
    }

    #[test]
    fn default_roundtrip() {
        let cfg = NodeConfig::default();
        let text = toml::to_string(&cfg).unwrap();
        let back: NodeConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }
}
