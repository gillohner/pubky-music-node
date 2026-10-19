//! The node's [`SourceFactory`]: queue items are library tracks
//! (`track:<id>`), local paths or `file://` URLs. Streaming services (M12)
//! plug in here as further URI schemes.

use pm_audio::{LocalFiles, PipelineConfig};
use pm_library::Library;
use pm_model::{AudioFormat, OutputCaps, TrackId};
use pm_zones::{FrameSource, Processor, QueueItem, SourceError, SourceFactory};
use std::sync::{Arc, Mutex};

pub const TRACK_SCHEME: &str = "track:";

pub struct NodeSources {
    library: Arc<Mutex<Library>>,
    files: LocalFiles,
}

impl NodeSources {
    pub fn new(library: Arc<Mutex<Library>>, cfg: PipelineConfig) -> Self {
        Self {
            library,
            files: LocalFiles::new(cfg),
        }
    }

    /// `track:<id>` for a library track.
    pub fn track_uri(id: TrackId) -> String {
        format!("{TRACK_SCHEME}{}", id.0)
    }

    /// A queue item for a library track, with its display metadata.
    pub fn item_for(track: &pm_model::Track) -> Option<QueueItem> {
        let id = track.id?;
        let mut item = QueueItem::new(Self::track_uri(id));
        item.title = Some(track.title.clone());
        item.artist = track.artists.first().map(|a| a.name.clone());
        item.album = track.album.as_ref().map(|a| a.title.clone());
        item.duration_ms = track.duration_ms;
        Some(item)
    }
}

impl SourceFactory for NodeSources {
    fn open(&self, item: &QueueItem) -> Result<Box<dyn FrameSource>, SourceError> {
        let Some(id) = item.uri.strip_prefix(TRACK_SCHEME) else {
            return self.files.open(item);
        };
        let id: i64 = id.parse().map_err(|_| SourceError::Open {
            uri: item.uri.clone(),
            reason: "malformed track id".into(),
        })?;
        let track = self
            .library
            .lock()
            .expect("library poisoned")
            .get_track(TrackId(id))
            .map_err(|e| SourceError::Open {
                uri: item.uri.clone(),
                reason: e.to_string(),
            })?
            .ok_or_else(|| SourceError::Open {
                uri: item.uri.clone(),
                reason: "no such track".into(),
            })?;
        let mut file_item = QueueItem::new(track.file.path.to_string_lossy());
        file_item.id = item.id;
        self.files.open(&file_item)
    }

    fn plan(&self, source: AudioFormat, caps: &OutputCaps) -> Result<Box<dyn Processor>, SourceError> {
        self.files.plan(source, caps)
    }
}
