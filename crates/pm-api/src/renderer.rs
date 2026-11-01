//! The node's zones as UPnP/DLNA MediaRenderers (`pm-renderer`): what a controller (Music
//! Assistant, Home Assistant's `dlna_dmr`, BubbleUPnP) sets becomes the zone's queue.
//!
//! - `SetAVTransportURI` replaces the queue with that URI (an HTTP stream the zone decodes);
//!   `Play` starts it. `SetNextAVTransportURI` queues the next one right after the current.
//! - Transport, seek and next/previous are the zone's own.
//! - Volume and mute go to the zone's output when it has hardware volume; the level is
//!   remembered for `GetVolume` (outputs do not report theirs).

use crate::state::NodeState;
use pm_model::Volume;
use pm_renderer::{Act, Host, Meta, Transport, View};
use pm_zones::{Insert, QueueItem, ZoneId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

pub struct ZoneRenderers {
    state: Weak<NodeState>,
    /// Volume (0-100) and mute last set through a renderer, per zone.
    levels: Mutex<HashMap<String, (u8, bool)>>,
}

impl ZoneRenderers {
    pub fn new(state: &Arc<NodeState>) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::downgrade(state),
            levels: Mutex::new(HashMap::new()),
        })
    }

    fn zone(&self, id: &str) -> Option<Arc<pm_zones::Zone>> {
        self.state.upgrade()?.zones.get(&ZoneId::new(id))
    }
}

/// A controller's URI as a queue item, fetched only where the controller at `peer` could reach.
fn item(uri: String, meta: Meta, peer: Option<std::net::IpAddr>) -> QueueItem {
    QueueItem {
        id: 0,
        uri,
        title: meta.title,
        artist: meta.artist,
        album: meta.album,
        duration_ms: meta.duration_ms,
        image: meta.art,
        limit: Some(pm_zones::FetchLimit { peer, lan: true }),
        by: None,
    }
}

fn meta_of(i: &QueueItem) -> Meta {
    Meta {
        title: i.title.clone(),
        artist: i.artist.clone(),
        album: i.album.clone(),
        art: i.image.clone(),
        duration_ms: i.duration_ms,
    }
}

#[async_trait::async_trait]
impl Host for ZoneRenderers {
    fn zones(&self) -> Vec<(String, String)> {
        let Some(s) = self.state.upgrade() else {
            return Vec::new();
        };
        s.zones
            .list()
            .into_iter()
            .map(|z| (z.id.to_string(), z.name))
            .collect()
    }

    fn view(&self, zone: &str) -> Option<View> {
        let z = self.zone(zone)?;
        let st = z.state();
        let index = st
            .now_playing
            .as_ref()
            .map(|n| n.index)
            .or(st.queue_index)
            .unwrap_or(0);
        let current = st
            .now_playing
            .as_ref()
            .map(|n| n.item.clone())
            .or_else(|| st.queue.get(index).cloned());
        let (volume, muted) = self
            .levels
            .lock()
            .expect("levels")
            .get(zone)
            .copied()
            .unwrap_or((100, false));
        let state = match st.transport {
            pm_zones::Transport::Playing => Transport::Playing,
            pm_zones::Transport::Paused => Transport::Paused,
            pm_zones::Transport::Stopped if st.queue.is_empty() => Transport::NoMedia,
            pm_zones::Transport::Stopped => Transport::Stopped,
        };
        Some(View {
            state,
            uri: current.as_ref().map(|i| i.uri.clone()).unwrap_or_default(),
            meta: current.as_ref().map(meta_of).unwrap_or_default(),
            next_uri: st.queue.get(index + 1).map(|i| i.uri.clone()).unwrap_or_default(),
            position_ms: st.now_playing.as_ref().map(|n| n.position_ms).unwrap_or(0),
            volume,
            muted,
        })
    }

    fn has_volume(&self, zone: &str) -> bool {
        self.zone(zone)
            .is_some_and(|z| z.output().caps().hw_volume.is_some())
    }

    async fn act(&self, zone: &str, act: Act) -> Result<(), String> {
        let z = self.zone(zone).ok_or("no such zone")?;
        match act {
            // Fetched only where the controller could reach itself, every redirect and address
            Act::SetUri { uri, meta, peer } => {
                z.stop();
                z.clear();
                z.enqueue(vec![item(uri, meta, peer)], Insert::End);
            }
            Act::SetNextUri { uri, meta, peer } => {
                if !uri.is_empty() {
                    z.enqueue(vec![item(uri, meta, peer)], Insert::Next);
                }
            }
            Act::Play => z.play(),
            Act::Pause => z.pause(),
            Act::Stop => z.stop(),
            Act::Next => z.next(),
            Act::Previous => z.previous(),
            Act::Seek(ms) => z.seek_ms(ms),
            Act::Volume(v) => {
                z.set_volume(Volume::Percent(v))
                    .await
                    .map_err(|e| e.to_string())?;
                self.levels
                    .lock()
                    .expect("levels")
                    .entry(zone.to_string())
                    .or_insert((100, false))
                    .0 = v;
            }
            Act::Mute(m) => {
                z.set_volume(Volume::Mute(m)).await.map_err(|e| e.to_string())?;
                self.levels
                    .lock()
                    .expect("levels")
                    .entry(zone.to_string())
                    .or_insert((100, false))
                    .1 = m;
            }
        }
        Ok(())
    }
}
