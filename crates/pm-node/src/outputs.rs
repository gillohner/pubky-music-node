//! Output discovery for this node.

use crate::config::OutputsConfig;
use pm_zones::{outputs::NullOutput, Output, OutputRegistry};
use std::sync::Arc;
use std::time::Duration;

/// Local outputs only: the null output always, ALSA `hw:` devices when
/// built with the `alsa` feature.
pub fn discover() -> OutputRegistry {
    let registry = OutputRegistry::new();
    registry.register(Arc::new(NullOutput::from_env()));
    #[cfg(feature = "alsa")]
    for out in pm_out_local::alsa::AlsaOutput::enumerate() {
        registry.register(Arc::new(out));
    }
    registry
}

/// Local outputs plus `pm-endpoint`s: configured addresses and, when
/// enabled, whatever mDNS answers within `discover_ms`.
pub fn discover_all(cfg: &OutputsConfig) -> OutputRegistry {
    let registry = discover();
    let mut addrs: Vec<(String, String)> = cfg.endpoints.iter().map(|a| (a.clone(), a.clone())).collect();
    if cfg.discover {
        for (name, addr) in pm_out_net::discover(Duration::from_millis(cfg.discover_ms)) {
            let a = addr.to_string();
            if !addrs.iter().any(|(_, x)| *x == a) {
                addrs.push((name, a));
            }
        }
    }
    for (name, addr) in addrs {
        match pm_out_net::NetOutput::connect(addr.as_str()) {
            Ok(outs) => {
                for o in outs {
                    tracing::info!(endpoint = %name, output = %o.id(), "endpoint output");
                    registry.register(Arc::new(o));
                }
            }
            Err(e) => tracing::warn!(endpoint = %name, %addr, "cannot reach endpoint: {e}"),
        }
    }
    registry
}
