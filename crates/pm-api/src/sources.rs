//! The node's [`SourceFactory`]: queue items are library tracks
//! (`track:<id>`), local paths or `file://` URLs. Streaming services (M12)
//! plug in here as further URI schemes.

use pm_audio::{LocalFiles, PipelineConfig};
use pm_library::Library;
use pm_model::{OutputCaps, TrackId};
use pm_zones::{FrameSource, OutputId, Processor, QueueItem, SourceError, SourceFactory};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub const TRACK_SCHEME: &str = "track:";
/// Settings key prefix for per-output DSP configs.
pub const DSP_KEY_PREFIX: &str = "dsp/";

pub struct NodeSources {
    library: Arc<Mutex<Library>>,
    files: LocalFiles,
    default_cfg: PipelineConfig,
    /// Per-output pipeline settings (DSP); the default applies otherwise.
    dsp: RwLock<HashMap<OutputId, PipelineConfig>>,
}

impl NodeSources {
    pub fn new(library: Arc<Mutex<Library>>, cfg: PipelineConfig) -> Self {
        // Restore persisted per-output configs.
        let mut dsp = HashMap::new();
        if let Ok(lib) = library.lock() {
            if let Ok(rows) = lib.settings_with_prefix(DSP_KEY_PREFIX) {
                for (k, v) in rows {
                    if let Ok(c) = serde_json::from_str::<PipelineConfig>(&v) {
                        dsp.insert(OutputId(k[DSP_KEY_PREFIX.len()..].to_string()), c);
                    }
                }
            }
        }
        Self {
            library,
            files: LocalFiles::new(cfg.clone()),
            default_cfg: cfg,
            dsp: RwLock::new(dsp),
        }
    }

    /// The pipeline settings in force for `output`.
    pub fn dsp_for(&self, output: &OutputId) -> PipelineConfig {
        self.dsp
            .read()
            .expect("dsp lock")
            .get(output)
            .cloned()
            .unwrap_or_else(|| self.default_cfg.clone())
    }

    /// Set (and persist) the settings for `output`; takes effect at the next track start.
    pub fn set_dsp(
        &self,
        output: &OutputId,
        cfg: Option<PipelineConfig>,
    ) -> Result<(), pm_library::LibraryError> {
        let key = format!("{DSP_KEY_PREFIX}{}", output.0);
        let lib = self.library.lock().expect("library poisoned");
        match &cfg {
            Some(c) => lib.set_setting(&key, &serde_json::to_string(c).expect("config serializes"))?,
            None => {
                lib.delete_setting(&key)?;
            }
        }
        drop(lib);
        let mut map = self.dsp.write().expect("dsp lock");
        match cfg {
            Some(c) => {
                map.insert(output.clone(), c);
            }
            None => {
                map.remove(output);
            }
        }
        Ok(())
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
        let inner = self.files.open(&file_item)?;
        Ok(Box::new(pm_zones::WithReplayGain {
            inner,
            gain: pm_zones::ReplayGain {
                track_db: track.replaygain_track_gain_db,
                album_db: track.replaygain_album_gain_db,
            },
        }))
    }

    fn plan(
        &self,
        src: &dyn FrameSource,
        caps: &OutputCaps,
        output: &OutputId,
    ) -> Result<Box<dyn Processor>, SourceError> {
        let source = src.format();
        let cfg = self.dsp_for(output);
        let plan = pm_audio::Plan::negotiate_for(source, caps, &cfg, src.replay_gain()).map_err(|e| {
            SourceError::Negotiate {
                format: source,
                reason: e.to_string(),
            }
        })?;
        Ok(Box::new(plan))
    }
}
