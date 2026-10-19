use crate::sink::{Output, OutputId, OutputInfo};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// The outputs the node currently knows about. Discovery (ALSA enumeration,
/// mDNS, SSDP) adds and removes entries; the API lists them.
#[derive(Default, Clone)]
pub struct OutputRegistry {
    inner: Arc<RwLock<BTreeMap<OutputId, Arc<dyn Output>>>>,
}

impl OutputRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, output: Arc<dyn Output>) {
        self.inner.write().expect("registry poisoned").insert(output.id(), output);
    }

    pub fn remove(&self, id: &OutputId) -> Option<Arc<dyn Output>> {
        self.inner.write().expect("registry poisoned").remove(id)
    }

    pub fn get(&self, id: &OutputId) -> Option<Arc<dyn Output>> {
        self.inner.read().expect("registry poisoned").get(id).cloned()
    }

    pub fn list(&self) -> Vec<OutputInfo> {
        self.inner.read().expect("registry poisoned").values().map(|o| o.info()).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("registry poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
