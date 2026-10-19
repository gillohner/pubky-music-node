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
    /// Upstreams require a real contact in the User-Agent and some block
    /// agents that name a code host (answers §7). Call before any network use.
    pub fn validate_contact(&self) -> Result<(), String> {
        validate_contact(&self.user_agent_contact)
    }

    pub fn user_agent(&self) -> String {
        format!(
            "pubky-music-node/{} ({})",
            env!("CARGO_PKG_VERSION"),
            self.user_agent_contact
        )
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

/// A contact is an email address or a URL, not empty, not a placeholder and
/// not a code-host link.
pub fn validate_contact(contact: &str) -> Result<(), String> {
    let c = contact.trim();
    if c.is_empty() || c.eq_ignore_ascii_case("unknown") {
        return Err("set a contact (email or URL) for outbound User-Agent strings".into());
    }
    if c.to_ascii_lowercase().contains("github") {
        return Err("the contact must not reference github; use an email or your own site".into());
    }
    if !(c.contains('@') || c.starts_with("http://") || c.starts_with("https://")) {
        return Err("the contact must be an email address or an http(s) URL".into());
    }
    Ok(())
}

#[cfg(test)]
mod contact_tests {
    use super::validate_contact;

    #[test]
    fn contact_rules() {
        assert!(validate_contact("ops@example.org").is_ok());
        assert!(validate_contact("https://example.org/contact").is_ok());
        assert!(validate_contact("").is_err());
        assert!(validate_contact("unknown").is_err());
        assert!(validate_contact("https://github.com/x/y").is_err());
        assert!(validate_contact("just-a-name").is_err());
    }
}
