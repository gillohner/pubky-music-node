//! Plugging the decoders and the pipeline into a `pm-zones` zone.
//!
//! [`LocalFiles`] is the [`SourceFactory`] for queue items whose `uri` is a
//! local path (or `file://` URL): it opens them with [`Source`] and plans a
//! [`Plan`] against the output's caps. Streaming sources (M12) will be a
//! second factory behind the same trait.

use crate::error::AudioError;
use crate::pipeline::{PipelineConfig, Plan};
use crate::play::Source;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, SignalPath};
use pm_zones::{FrameSource, Processor, QueueItem, SourceError, SourceFactory};
use std::path::{Path, PathBuf};

impl FrameSource for Source {
    fn format(&self) -> AudioFormat {
        Source::format(self)
    }

    fn duration_ms(&self) -> Option<u64> {
        Source::duration_ms(self)
    }

    fn next_block(&mut self) -> Result<Option<AudioBlock>, SourceError> {
        Source::next_block(self).map_err(|e| SourceError::Decode(e.to_string()))
    }

    fn seek_ms(&mut self, ms: u64) -> Result<(), SourceError> {
        Source::seek_ms(self, ms).map_err(|e| SourceError::Seek(e.to_string()))
    }

    fn finalize(&mut self) -> Option<bool> {
        Source::finalize(self)
    }

    fn lossy_source(&self) -> bool {
        Source::is_lossy(self)
    }

    fn stream_info(&self) -> Option<pm_zones::StreamInfo> {
        Some(Source::stream_info(self))
    }
}

/// A library file: the decoder plus the file's average bitrate for lossy codecs (size over
/// length), for the player's quality label.
pub struct FileSource {
    src: Source,
    bitrate_kbps: Option<u32>,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<Self, AudioError> {
        let src = Source::open(path)?;
        let bitrate_kbps = match (src.is_lossy(), src.duration_ms(), std::fs::metadata(path)) {
            (true, Some(ms), Ok(m)) if ms > 0 => Some((m.len() * 8 / ms) as u32),
            _ => None,
        };
        Ok(Self { src, bitrate_kbps })
    }
}

impl FrameSource for FileSource {
    fn format(&self) -> AudioFormat {
        self.src.format()
    }
    fn duration_ms(&self) -> Option<u64> {
        self.src.duration_ms()
    }
    fn next_block(&mut self) -> Result<Option<AudioBlock>, SourceError> {
        FrameSource::next_block(&mut self.src)
    }
    fn seek_ms(&mut self, ms: u64) -> Result<(), SourceError> {
        FrameSource::seek_ms(&mut self.src, ms)
    }
    fn finalize(&mut self) -> Option<bool> {
        FrameSource::finalize(&mut self.src)
    }
    fn lossy_source(&self) -> bool {
        self.src.is_lossy()
    }
    fn stream_info(&self) -> Option<pm_zones::StreamInfo> {
        Some(pm_zones::StreamInfo {
            bitrate_kbps: self.bitrate_kbps,
            service: "library".into(),
            ..self.src.stream_info()
        })
    }
}

impl Processor for Plan {
    fn source_format(&self) -> AudioFormat {
        Plan::source_format(self)
    }

    fn output_format(&self) -> AudioFormat {
        Plan::output_format(self)
    }

    fn signal_path(&self) -> &SignalPath {
        Plan::signal_path(self)
    }

    fn process(&mut self, block: AudioBlock) -> Result<AudioBlock, SourceError> {
        Plan::process(self, block).map_err(|e| SourceError::Decode(e.to_string()))
    }

    fn finish(&mut self) -> Result<Option<AudioBlock>, SourceError> {
        Plan::finish(self).map_err(|e| SourceError::Decode(e.to_string()))
    }
}

/// Queue items are local files.
#[derive(Debug, Clone, Default)]
pub struct LocalFiles {
    cfg: PipelineConfig,
}

impl LocalFiles {
    pub fn new(cfg: PipelineConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &PipelineConfig {
        &self.cfg
    }

    /// `file:///a/b.flac` or a plain path → the path.
    pub fn path_of(uri: &str) -> PathBuf {
        match uri.strip_prefix("file://") {
            Some(rest) => PathBuf::from(percent_decode(rest)),
            None => PathBuf::from(uri),
        }
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_err(path: &Path, e: AudioError) -> SourceError {
    SourceError::Open {
        uri: path.display().to_string(),
        reason: e.to_string(),
    }
}

impl SourceFactory for LocalFiles {
    fn open(&self, item: &QueueItem) -> Result<Box<dyn FrameSource>, SourceError> {
        let path = Self::path_of(&item.uri);
        let src = FileSource::open(&path).map_err(|e| open_err(&path, e))?;
        Ok(Box::new(src))
    }

    fn plan(
        &self,
        src: &dyn FrameSource,
        caps: &OutputCaps,
        _output: &pm_zones::OutputId,
    ) -> Result<Box<dyn Processor>, SourceError> {
        let source = src.format();
        let plan = Plan::negotiate_for(source, caps, &self.cfg, src.replay_gain()).map_err(|e| {
            SourceError::Negotiate {
                format: source,
                reason: e.to_string(),
            }
        })?;
        Ok(Box::new(plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library file reports its codec from the decoder, is lossless with no bitrate, and
    /// says it comes from the library.
    #[test]
    fn a_file_says_what_it_streams() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("a.wav");
        write_wav(&wav, 44_100, 4410, 1);
        let f = FileSource::open(&wav).unwrap();
        let info = FrameSource::stream_info(&f).unwrap();
        assert_eq!(info.codec, "WAV");
        assert!(info.lossless);
        assert_eq!(info.bitrate_kbps, None, "no bitrate for a lossless file");
        assert_eq!(info.service, "library");
        assert!(!info.preview);

        let flac = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/14_wasted_bits.flac");
        let info = FrameSource::stream_info(&FileSource::open(&flac).unwrap()).unwrap();
        assert_eq!((info.codec.as_str(), info.lossless), ("FLAC", true));
    }
    use pm_zones::outputs::{read_wav, WavFileOutput};
    use pm_zones::{Insert, Zone, ZoneEvent, ZoneId};
    use std::sync::Arc;

    fn write_wav(path: &Path, rate: u32, frames: usize, seed: i32) -> Vec<i32> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let mut v = Vec::new();
        for i in 0..frames * 2 {
            let s = ((i as i64 * 7919 + seed as i64 * 104_729) % (1 << 24)) as i32 - (1 << 23);
            v.push(s);
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        v
    }

    /// Two real files through a zone into one WAV: the output is the exact
    /// concatenation, which also proves the sink was opened once (a second
    /// open would have truncated the file).
    #[tokio::test]
    async fn two_tracks_are_joined_gapless_into_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        let out = dir.path().join("out.wav");
        let mut want = write_wav(&a, 96_000, 5000, 1);
        want.extend(write_wav(&b, 96_000, 3000, 2));
        let output = Arc::new(WavFileOutput::new(&out));
        let zone = Zone::new(
            ZoneId::new("t"),
            "t",
            output,
            Arc::new(LocalFiles::new(PipelineConfig::default())),
        );
        let mut events = zone.subscribe();
        zone.enqueue(
            vec![
                QueueItem::new(a.to_string_lossy()),
                QueueItem::new(format!("file://{}", b.display())),
            ],
            Insert::End,
        );
        zone.play();
        let st = zone.wait_stopped().await;
        assert_eq!(st.error, None);
        let back = read_wav(&out).unwrap();
        assert_eq!(back.format, AudioFormat::pcm(96_000, 24, 2));
        assert_eq!(back.samples, pm_model::Samples::PcmI32(want));
        let mut gapless = Vec::new();
        while let Ok(e) = events.try_recv() {
            if let ZoneEvent::TrackStarted {
                gapless: g,
                signal_path,
                ..
            } = e
            {
                assert!(signal_path.is_bit_perfect());
                gapless.push(g);
            }
        }
        assert_eq!(gapless, [false, true]);
    }

    #[test]
    fn uri_to_path() {
        assert_eq!(LocalFiles::path_of("/m/a.flac"), PathBuf::from("/m/a.flac"));
        assert_eq!(
            LocalFiles::path_of("file:///m/My%20Album/01.flac"),
            PathBuf::from("/m/My Album/01.flac")
        );
        assert_eq!(LocalFiles::path_of("file:///x%2"), PathBuf::from("/x%2"));
    }
}
