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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LibraryConfig {
    /// Folders to scan. Mounted shares count as folders.
    pub folders: Vec<PathBuf>,
    /// Rescan automatically when files change in the folders (default on).
    pub watch: bool,
    /// Seconds without changes before an automatic rescan starts.
    pub watch_quiet_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ApiConfig {
    pub listen: String,
    /// Bearer token clients must send. Empty = no auth: keep `listen` on localhost then.
    pub token: String,
    /// Name shown to controllers.
    pub name: String,
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

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            watch: true,
            watch_quiet_secs: 15,
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7788".into(),
            token: String::new(),
            name: "Pubky Music Node".into(),
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

    /// The pm-pubky config with the node-level contact filled in. Errors
    /// when no usable contact is set, since every upstream requires one.
    pub fn pubky_config(&self) -> anyhow::Result<PmPubkyConfig> {
        let mut c = self.pubky.clone();
        if !self.contact.is_empty() {
            c.user_agent_contact = self.contact.clone();
        }
        c.validate_contact()
            .map_err(|e| anyhow::anyhow!("{e}: set `contact` in the node config"))?;
        Ok(c)
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
        assert!(cfg
            .pubky_config()
            .unwrap()
            .user_agent()
            .contains("ops@example.org"));
        assert!(NodeConfig::default().pubky_config().is_err());
    }

    #[test]
    fn default_roundtrip() {
        let cfg = NodeConfig::default();
        let text = toml::to_string(&cfg).unwrap();
        let back: NodeConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }
}
