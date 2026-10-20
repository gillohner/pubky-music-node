//! Local playback: the controller is also an output. A `pm_zones::Zone` on
//! this machine's DAC (ALSA `hw:` when built with `alsa`, else the null
//! output) plays library tracks fetched from the node into a cache, through
//! the same decoder and pipeline the node uses, so the signal path shown
//! here is the real one.

use pm_audio::{LocalFiles, PipelineConfig};
use pm_client::NodeClient;
use pm_model::OutputCaps;
use pm_zones::{
    outputs::NullOutput, FrameSource, Output, Processor, QueueItem, SourceError, SourceFactory, Zone, ZoneId,
};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub const LOCAL_ZONE_ID: &str = "local";

pub struct LocalPlayer {
    pub zone: Arc<Zone>,
    pub output_name: String,
    factory: Arc<RemoteTracks>,
}

impl LocalPlayer {
    pub fn new(
        client: Arc<RwLock<Option<NodeClient>>>,
        handle: tokio::runtime::Handle,
        cache: PathBuf,
    ) -> Self {
        let _ = std::fs::create_dir_all(&cache);
        let output = local_output();
        let output_name = output.name();
        let factory = Arc::new(RemoteTracks {
            client,
            handle,
            cache,
            files: LocalFiles::new(PipelineConfig::default()),
            cfg: RwLock::new(PipelineConfig::default()),
        });
        let zone = Arc::new(Zone::new(
            ZoneId::new(LOCAL_ZONE_ID),
            "This computer",
            output,
            factory.clone(),
        ));
        Self {
            zone,
            output_name,
            factory,
        }
    }

    /// DSP settings of the local zone (applied at the next track start).
    pub fn dsp(&self) -> PipelineConfig {
        self.factory.cfg.read().expect("cfg lock").clone()
    }

    pub fn set_dsp(&self, cfg: PipelineConfig) {
        *self.factory.cfg.write().expect("cfg lock") = cfg;
    }
}

/// Pick this machine's output: the first ALSA `hw:` device, else null.
fn local_output() -> Arc<dyn Output> {
    #[cfg(feature = "alsa")]
    {
        if let Some(out) = pm_out_local::alsa::AlsaOutput::enumerate().into_iter().next() {
            return Arc::new(out);
        }
    }
    Arc::new(NullOutput::new())
}

/// `track:<id>` → download from the node into the cache once, then play the
/// cached file; anything else is a local path.
struct RemoteTracks {
    client: Arc<RwLock<Option<NodeClient>>>,
    handle: tokio::runtime::Handle,
    cache: PathBuf,
    files: LocalFiles,
    cfg: RwLock<PipelineConfig>,
}

impl RemoteTracks {
    fn fetch(&self, id: i64, uri: &str) -> Result<(PathBuf, pm_zones::ReplayGain), SourceError> {
        let client = self
            .client
            .read()
            .expect("client lock")
            .clone()
            .ok_or_else(|| SourceError::Open {
                uri: uri.to_string(),
                reason: "not connected to a node".into(),
            })?;
        let host = client
            .base()
            .host_str()
            .unwrap_or("node")
            .replace([':', '/'], "_");
        let dir = self.cache.join(host);
        std::fs::create_dir_all(&dir).map_err(|e| SourceError::Open {
            uri: uri.to_string(),
            reason: e.to_string(),
        })?;
        // Keep the original extension so DSF/DFF are detected like local files.
        let track = self
            .handle
            .block_on(client.track(id))
            .map_err(|e| SourceError::Open {
                uri: uri.to_string(),
                reason: e.to_string(),
            })?;
        let ext = track
            .file
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "bin".into());
        let path = dir.join(format!("{id}.{ext}"));
        let fresh = std::fs::metadata(&path)
            .map(|m| m.len() == track.file.size_bytes)
            .unwrap_or(false);
        if !fresh {
            self.handle
                .block_on(client.download_track(id, &path))
                .map_err(|e| SourceError::Open {
                    uri: uri.to_string(),
                    reason: e.to_string(),
                })?;
        }
        Ok((
            path,
            pm_zones::ReplayGain {
                track_db: track.replaygain_track_gain_db,
                album_db: track.replaygain_album_gain_db,
            },
        ))
    }
}

impl SourceFactory for RemoteTracks {
    fn open(&self, item: &QueueItem) -> Result<Box<dyn FrameSource>, SourceError> {
        let Some(id) = item.uri.strip_prefix(pm_api::sources::TRACK_SCHEME) else {
            return self.files.open(item);
        };
        let id: i64 = id.parse().map_err(|_| SourceError::Open {
            uri: item.uri.clone(),
            reason: "malformed track id".into(),
        })?;
        let (path, gain) = self.fetch(id, &item.uri)?;
        let mut local = QueueItem::new(path.to_string_lossy());
        local.id = item.id;
        let inner = self.files.open(&local)?;
        Ok(Box::new(pm_zones::WithReplayGain { inner, gain }))
    }

    fn plan(
        &self,
        src: &dyn FrameSource,
        caps: &OutputCaps,
        _output: &pm_zones::OutputId,
    ) -> Result<Box<dyn Processor>, SourceError> {
        let source = src.format();
        let cfg = self.cfg.read().expect("cfg lock").clone();
        let plan = pm_audio::Plan::negotiate_for(source, caps, &cfg, src.replay_gain()).map_err(|e| {
            SourceError::Negotiate {
                format: source,
                reason: e.to_string(),
            }
        })?;
        Ok(Box::new(plan))
    }
}
