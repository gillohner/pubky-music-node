//! Output discovery for this node.

use pm_zones::{outputs::NullOutput, OutputRegistry};
use std::sync::Arc;

/// Collect every output the node can see: the null output always, ALSA
/// `hw:` devices when built with the `alsa` feature, network endpoints in
/// later milestones.
pub fn discover() -> OutputRegistry {
    let registry = OutputRegistry::new();
    registry.register(Arc::new(NullOutput::new()));
    #[cfg(feature = "alsa")]
    for out in pm_out_local::alsa::AlsaOutput::enumerate() {
        registry.register(Arc::new(out));
    }
    registry
}
