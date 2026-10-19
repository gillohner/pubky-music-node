//! PCM decoding through symphonia 0.6.
//!
//! Every container symphonia hands us is turned into [`AudioBlock`]s in the
//! layout fixed by `pm_model::block`: interleaved `i32`, right-justified,
//! sign-extended, `format.bits` = the source's real bit depth.
//!
//! What symphonia 0.6 actually produces (verified against its source):
//! * FLAC (`symphonia-bundle-flac`) and ALAC (`symphonia-codec-alac`) emit
//!   `S32` buffers **left-shifted** to 32 bits (`sample << (32 - bps)`).
//! * The PCM codec emits `S16` / `S24` / `S32` buffers holding the sample in
//!   its own width (`i24` wraps a 24-bit value).
//! * `copy_to_vec_interleaved::<i32>()` converts every integer width to a
//!   **32-bit left-justified** `i32` (`i16 << 16`, `i24 << 8`, `i32` as-is).
//!
//! So the one rule here is: take the interleaved `i32` view and arithmetic
//! right-shift by `32 - bits`. FLAC's STREAMINFO MD5 check (enabled through
//! `verify`) runs inside the decoder on the un-shifted samples and is exposed
//! through [`Decoder::finalize`].

use crate::error::AudioError;
use pm_model::{AudioBlock, AudioFormat};
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::sample::SampleFormat;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_ALAC, CODEC_ID_FLAC};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, Timestamp};

pub struct Decoder {
    format: Box<dyn FormatReader + 'static>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    audio_format: AudioFormat,
    container: &'static str,
    duration_ms: Option<u64>,
    /// Ticks of the track time base per second (frames for audio).
    ticks_per_second: f64,
    /// After a seek: drop frames before this timestamp.
    skip_until: Option<Timestamp>,
    finished: bool,
}

impl Decoder {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AudioError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| AudioError::Open { path: path.to_path_buf(), source })?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| match e {
                SymError::Unsupported(m) => AudioError::Unsupported(format!("{}: {m}", path.display())),
                other => AudioError::from(other),
            })?;
        Self::from_reader(format)
    }

    fn from_reader(format: Box<dyn FormatReader + 'static>) -> Result<Self, AudioError> {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| AudioError::Unsupported("no audio track".into()))?;
        let params = match &track.codec_params {
            Some(CodecParameters::Audio(p)) => p.clone(),
            _ => return Err(AudioError::Unsupported("no audio codec parameters".into())),
        };
        let track_id = track.id;

        let sample_rate = params.sample_rate.ok_or_else(|| AudioError::Malformed("missing sample rate".into()))?;
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count())
            .ok_or_else(|| AudioError::Malformed("missing channel layout".into()))?;
        if channels == 0 || channels > u8::MAX as usize {
            return Err(AudioError::Malformed(format!("bad channel count {channels}")));
        }

        let container: &'static str = if params.codec == CODEC_ID_FLAC {
            "flac"
        } else if params.codec == CODEC_ID_ALAC {
            "alac"
        } else {
            match format.format_info().short_name {
                "wav" | "wave" => "wav",
                "aiff" => "aiff",
                other => Box::leak(other.to_string().into_boxed_str()),
            }
        };

        let bits = resolve_bits(&params).ok_or_else(|| AudioError::Unsupported("cannot determine bits per sample".into()))?;
        if !matches!(bits, 8 | 16 | 20 | 24 | 32) {
            return Err(AudioError::Unsupported(format!("{bits}-bit samples")));
        }
        let audio_format = AudioFormat::pcm(sample_rate, bits, channels as u8);

        let ticks_per_second = track
            .time_base
            .map(|tb| tb.denom.get() as f64 / tb.numer.get() as f64)
            .unwrap_or(sample_rate as f64);
        let duration_ms = match (track.num_frames, track.time_base, track.duration) {
            (Some(n), _, _) => Some(n * 1000 / sample_rate as u64),
            (None, Some(tb), Some(d)) => tb.calc_duration(d).map(|t| t.as_millis() as u64),
            _ => None,
        };

        let opts = AudioDecoderOptions::default().verify(true);
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &opts)
            .map_err(|e| AudioError::Unsupported(format!("codec: {e}")))?;

        Ok(Self {
            format,
            decoder,
            track_id,
            audio_format,
            container,
            duration_ms,
            ticks_per_second,
            skip_until: None,
            finished: false,
        })
    }

    pub fn format(&self) -> AudioFormat {
        self.audio_format
    }

    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    pub fn container(&self) -> &str {
        self.container
    }

    /// Decode the next packet. `Ok(None)` at end of stream. Packets of other
    /// tracks and undecodable packets are skipped.
    pub fn next_block(&mut self) -> Result<Option<AudioBlock>, AudioError> {
        if self.finished {
            return Ok(None);
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.finished = true;
                    return Ok(None);
                }
                Err(SymError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.finished = true;
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let pts = packet.pts;
            let decoded = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                Err(SymError::DecodeError(m)) => {
                    tracing::warn!(error = m, "skipping undecodable packet");
                    continue;
                }
                Err(SymError::IoError(e)) => {
                    tracing::warn!(error = %e, "skipping packet with io error");
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let frames = decoded.frames();
            if frames == 0 {
                continue;
            }
            let mut v: Vec<i32> = Vec::with_capacity(frames * self.audio_format.channels as usize);
            decoded.copy_to_vec_interleaved::<i32>(&mut v);
            let shift = 32 - self.audio_format.bits as u32;
            if shift > 0 {
                for s in &mut v {
                    *s >>= shift;
                }
            }
            if let Some(until) = self.skip_until.take() {
                let delta = until.get() - pts.get();
                if delta > 0 {
                    let skip_frames = ((delta as f64) * self.audio_format.rate_hz as f64 / self.ticks_per_second).round() as usize;
                    if skip_frames >= frames {
                        self.skip_until = Some(until);
                        continue;
                    }
                    v.drain(..skip_frames * self.audio_format.channels as usize);
                }
            }
            return Ok(Some(AudioBlock::pcm_i32(self.audio_format, v)));
        }
    }

    /// Accurate seek: the next block starts at `ms` (frames before it are
    /// dropped).
    pub fn seek_ms(&mut self, ms: u64) -> Result<(), AudioError> {
        let seeked = self
            .format
            .seek(SeekMode::Accurate, SeekTo::Time { time: Time::from_millis_u64(ms), track_id: Some(self.track_id) })?;
        self.decoder.reset();
        self.finished = false;
        self.skip_until = Some(seeked.required_ts);
        Ok(())
    }

    /// Result of the codec's own verification (FLAC: the STREAMINFO MD5 over
    /// all decoded samples). `None` when the codec has none. Call after
    /// `next_block` returned `None`.
    pub fn finalize(&mut self) -> Option<bool> {
        self.decoder.finalize().verify_ok
    }

    /// Decode everything that is left into one block.
    pub fn decode_all(&mut self) -> Result<AudioBlock, AudioError> {
        let mut out = AudioBlock::pcm_i32(self.audio_format, Vec::new());
        while let Some(b) = self.next_block()? {
            out.append(&b).map_err(|e| AudioError::Decode(e.to_string()))?;
        }
        Ok(out)
    }
}

/// The source's real bit depth.
fn resolve_bits(params: &symphonia::core::codecs::audio::AudioCodecParameters) -> Option<u8> {
    if params.codec == CODEC_ID_ALAC {
        // The container does not set bits_per_sample for ALAC; it is byte 5 of
        // the magic cookie (optionally wrapped in frma/alac atoms).
        let mut cookie: &[u8] = params.extra_data.as_deref()?;
        if cookie.len() >= 12 && &cookie[4..8] == b"frma" {
            cookie = &cookie[12..];
        }
        if cookie.len() >= 12 && &cookie[4..8] == b"alac" {
            cookie = &cookie[12..];
        }
        return cookie.get(5).copied();
    }
    if let Some(b) = params.bits_per_sample {
        return u8::try_from(b).ok();
    }
    params.sample_format.map(|f| match f {
        SampleFormat::U8 | SampleFormat::S8 => 8,
        SampleFormat::U16 | SampleFormat::S16 => 16,
        SampleFormat::U24 | SampleFormat::S24 => 24,
        SampleFormat::U32 | SampleFormat::S32 | SampleFormat::F32 => 32,
        SampleFormat::F64 => 32,
    })
}
