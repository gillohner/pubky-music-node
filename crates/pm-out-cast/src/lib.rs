//! Chromecast / Google Cast receivers as outputs (handoff M7): the node
//! launches the default media receiver and asks it to play the zone's pull
//! WAV stream, so the receiver fetches exactly the post-DSP PCM and paces
//! the zone. Cast v2 protocol: TLS on 8009 (self-signed device certs, so the
//! verifier is deliberately permissive), length-prefixed protobuf
//! `CastMessage`s carrying JSON on the connection, heartbeat, receiver and
//! media namespaces. Discovery is mDNS `_googlecast._tcp`.
//!
//! Quality: Cast devices resample to 48 kHz internally and the default
//! receiver decodes WAV in software, so the output's ceiling is declared
//! `LosslessUpTo 24/48` and the signal path shows the resample stage when
//! the source is anything else.

pub mod link;
pub mod output;
pub mod proto;
pub mod session;

use std::net::SocketAddr;
use std::time::Duration;

pub use output::CastOutput;

pub const SERVICE_TYPE: &str = "_googlecast._tcp.local.";
pub const DEFAULT_PORT: u16 = 8009;
/// The default media receiver app.
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";

#[derive(Debug, thiserror::Error)]
pub enum CastError {
    #[error("network: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("receiver: {0}")]
    Receiver(String),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
}

/// A Cast device as found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub friendly_name: String,
    pub model: Option<String>,
    pub addr: SocketAddr,
}

impl Device {
    /// A Cast group made in the Google Home app: one target that plays on all its speakers in sync
    /// (the group's leader distributes the audio). It has its own id and port.
    pub fn is_group(&self) -> bool {
        self.model.as_deref() == Some("Google Cast Group")
    }
}

/// What the TV or speaker shows while it plays.
pub use pm_zones::TrackMeta as Meta;

/// `MusicTrackMediaMetadata` (metadataType 3) for the default media receiver.
pub fn meta_json(meta: &Meta, fallback_title: &str) -> serde_json::Value {
    let mut m = serde_json::json!({
        "metadataType": 3,
        "title": meta.title.clone().unwrap_or_else(|| fallback_title.to_string()),
    });
    if let Some(a) = &meta.artist {
        m["artist"] = a.clone().into();
    }
    if let Some(a) = &meta.album {
        m["albumName"] = a.clone().into();
    }
    if let Some(i) = &meta.image {
        m["images"] = serde_json::json!([{ "url": i }]);
    }
    m
}

/// mDNS browse for Cast devices within `timeout`.
pub fn discover(timeout: Duration) -> Vec<Device> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(rx) = daemon.browse(SERVICE_TYPE) else {
        return Vec::new();
    };
    let deadline = std::time::Instant::now() + timeout;
    let mut out: Vec<Device> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                // IPv4 first: a link-local IPv6 address needs a scope a socket address may lack.
                let addrs: Vec<std::net::IpAddr> =
                    info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                let Some(ip) = addrs
                    .iter()
                    .find(|a| a.is_ipv4())
                    .or_else(|| addrs.first())
                    .copied()
                else {
                    continue;
                };
                let id = info
                    .get_property_val_str("id")
                    .map(str::to_string)
                    .unwrap_or_else(|| info.get_fullname().to_string());
                if out.iter().any(|d| d.id == id) {
                    continue;
                }
                out.push(Device {
                    id,
                    friendly_name: info
                        .get_property_val_str("fn")
                        .map(str::to_string)
                        .unwrap_or_else(|| info.get_fullname().trim_end_matches(SERVICE_TYPE).to_string()),
                    model: info.get_property_val_str("md").map(str::to_string),
                    addr: SocketAddr::new(ip, info.get_port()),
                });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    out
}
