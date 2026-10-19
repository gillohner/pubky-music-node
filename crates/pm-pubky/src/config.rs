use serde::{Deserialize, Serialize};

/// Which Pubky network the node talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Mainnet,
    /// A local `pubky-testnet` on the default ports.
    Testnet,
}

/// Configuration for every upstream this crate talks to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PmPubkyConfig {
    /// Pinakes API base, without `/v1`.
    pub pinakes_base: String,
    /// Nexus API base, without `/v0`.
    pub nexus_base: String,
    /// Contact put in the `User-Agent` of every request, as upstreams ask.
    pub user_agent_contact: String,
    pub network: Network,
}

impl Default for PmPubkyConfig {
    fn default() -> Self {
        Self {
            pinakes_base: "https://8092--main--pinakes--gil.coder.riginode.xyz".into(),
            nexus_base: "https://nexus.staging.pubky.app".into(),
            user_agent_contact: "unknown".into(),
            network: Network::Mainnet,
        }
    }
}

impl PmPubkyConfig {
    pub fn user_agent(&self) -> String {
        format!("pubky-music-node/{} ({})", env!("CARGO_PKG_VERSION"), self.user_agent_contact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_toml() {
        let c = PmPubkyConfig::default();
        assert!(c.pinakes_base.starts_with("https://"));
        assert!(c.user_agent().starts_with("pubky-music-node/"));
        let partial: PmPubkyConfig = serde_json::from_str(r#"{"network":"testnet"}"#).unwrap();
        assert_eq!(partial.network, Network::Testnet);
        assert_eq!(partial.nexus_base, c.nexus_base);
    }
}
