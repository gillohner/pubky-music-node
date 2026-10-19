//! ALSA `hw:` output. Draft, not yet compiled on hardware (see crate docs).
//!
//! Rules that make this bit-perfect:
//! * only `hw:` device names (no `plughw:`, no `default`);
//! * `set_rate_resample(false)` and an exact-rate check after `hw_params`;
//! * the sample format is chosen from the block's bit depth: 16 → `S16_LE`,
//!   24 → `S24_3LE` (packed) or `S32_LE` with a `pad` stage planned by the
//!   pipeline, 32 → `S32_LE`; DSD → `DSD_U8` when the device lists it, which
//!   is exactly the `pm_model` DSD byte layout, else DoP via 24-bit PCM;
//! * no software volume: `set_volume` goes to the mixer control when the
//!   device has one, else returns `NoVolume` and the pipeline handles it.

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, DsdSupport, OutputCaps, QualityCeiling, SampleKind, Samples};
use pm_zones::{check_block, AudioSink, BufferLevel, Output, OutputId, SinkError};

const CANDIDATE_RATES: [u32; 14] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000, 8_000, 16_000,
    22_050, 32_000,
];

pub struct AlsaOutput {
    device: String,
    name: String,
    caps: OutputCaps,
}

impl AlsaOutput {
    /// Probe a `hw:` device for its capabilities and wrap it as an [`Output`].
    pub fn probe(device: &str, name: &str) -> Result<Self, SinkError> {
        if !crate::is_direct_hw_name(device) {
            return Err(SinkError::Device(format!("refusing non-hw device {device}")));
        }
        let pcm = PCM::new(device, Direction::Playback, false).map_err(alsa_err)?;
        let hwp = HwParams::any(&pcm).map_err(alsa_err)?;
        let mut pcm_rates = Vec::new();
        for r in CANDIDATE_RATES {
            if hwp.test_rate(r).is_ok() {
                pcm_rates.push(r);
            }
        }
        pcm_rates.sort_unstable();
        let max_pcm_bits = if hwp.test_format(Format::S32LE).is_ok() {
            32
        } else if hwp.test_format(Format::S243LE).is_ok() || hwp.test_format(Format::S24LE).is_ok() {
            24
        } else if hwp.test_format(Format::S16LE).is_ok() {
            16
        } else {
            return Err(SinkError::Device(format!("{device}: no integer PCM format")));
        };
        let max_channels = hwp.get_channels_max().map_err(alsa_err)?.min(8) as u8;
        let dsd = if hwp.test_format(Format::DSDU8).is_ok() {
            // Native DSD rate limit is not exposed by ALSA as a single number; DSD rates are
            // tested as byte rates (bit rate / 8) when opening. Advertise DSD512 and let open()
            // refuse what the device cannot take.
            Some(DsdSupport::Native {
                max_rate: AudioFormat::DSD512_RATE,
            })
        } else if max_pcm_bits >= 24 && pcm_rates.contains(&176_400) {
            Some(DsdSupport::DoP {
                max_rate: AudioFormat::DSD128_RATE,
            })
        } else {
            None
        };
        Ok(Self {
            device: device.to_string(),
            name: name.to_string(),
            caps: OutputCaps {
                pcm_rates,
                max_pcm_bits,
                max_channels,
                dsd,
                hw_volume: None,
                standby: false,
                source_switch: false,
                gapless: true,
                groupable: false,
                fixed_latency_ms: None,
                quality_ceiling: QualityCeiling::BitPerfect,
            },
        })
    }

    /// Enumerate playback `hw:` devices on this machine.
    pub fn enumerate() -> Vec<AlsaOutput> {
        let mut out = Vec::new();
        for card in alsa::card::Iter::new().flatten() {
            let Ok(ctl) = alsa::Ctl::from_card(&card, false) else {
                continue;
            };
            let Ok(info) = ctl.card_info() else { continue };
            let card_name = info.get_name().unwrap_or("?").to_string();
            let mut dev: i32 = -1;
            loop {
                if ctl.pcm_next_device(&mut dev).is_err() || dev < 0 {
                    break;
                }
                let device = format!("hw:{},{}", card.get_index(), dev);
                let name = format!("{card_name} ({device})");
                match AlsaOutput::probe(&device, &name) {
                    Ok(o) => out.push(o),
                    Err(e) => tracing::debug!(%device, error = %e, "skipping ALSA device"),
                }
            }
        }
        out
    }
}

fn alsa_err(e: alsa::Error) -> SinkError {
    SinkError::Device(e.to_string())
}

#[async_trait]
impl Output for AlsaOutput {
    fn id(&self) -> OutputId {
        OutputId::new("alsa", &self.device)
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        fmt.validate().map_err(|e| SinkError::Device(e.to_string()))?;
        if !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let device = self.device.clone();
        let id = self.id();
        // ALSA calls block; keep them off the async executor.
        tokio_free_blocking(move || AlsaSink::open(&device, id, fmt))
            .map(|s| Box::new(s) as Box<dyn AudioSink>)
    }
}

/// Run a blocking closure without depending on a specific runtime.
fn tokio_free_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(f).join().expect("alsa open thread panicked")
}

struct AlsaSink {
    id: OutputId,
    pcm: PCM,
    fmt: AudioFormat,
    alsa_format: Format,
    buffer_frames: u64,
    written: u64,
}

impl AlsaSink {
    fn open(device: &str, id: OutputId, fmt: AudioFormat) -> Result<Self, SinkError> {
        let pcm = PCM::new(device, Direction::Playback, false).map_err(alsa_err)?;
        let (alsa_format, channels, rate) = match fmt.kind {
            SampleKind::Pcm => {
                let f = match fmt.bits {
                    16 => Format::S16LE,
                    24 => Format::S243LE,
                    32 => Format::S32LE,
                    b => return Err(SinkError::Device(format!("unsupported bit depth {b}"))),
                };
                (f, fmt.channels as u32, fmt.rate_hz)
            }
            // DSD_U8: one byte per channel per frame, so the frame rate is the bit rate / 8.
            SampleKind::Dsd => (Format::DSDU8, fmt.channels as u32, fmt.rate_hz / 8),
        };
        let buffer_frames = {
            let hwp = HwParams::any(&pcm).map_err(alsa_err)?;
            hwp.set_rate_resample(false).map_err(alsa_err)?;
            hwp.set_access(Access::RWInterleaved).map_err(alsa_err)?;
            hwp.set_channels(channels).map_err(alsa_err)?;
            hwp.set_format(alsa_format).map_err(alsa_err)?;
            hwp.set_rate(rate, ValueOr::Nearest).map_err(alsa_err)?;
            let got = hwp.get_rate().map_err(alsa_err)?;
            if got != rate {
                return Err(SinkError::UnsupportedFormat(id, fmt));
            }
            // ~0.5 s of buffer: generous, the endpoint owns the clock (§6.1).
            hwp.set_buffer_size_near((rate / 2) as i64).map_err(alsa_err)?;
            pcm.hw_params(&hwp).map_err(alsa_err)?;
            hwp.get_buffer_size().map_err(alsa_err)? as u64
        };
        Ok(Self {
            id,
            pcm,
            fmt,
            alsa_format,
            buffer_frames,
            written: 0,
        })
    }

    fn pack(&self, block: &AudioBlock) -> Result<Vec<u8>, SinkError> {
        let mut out = Vec::with_capacity(block.frames() * self.fmt.channels as usize * 4);
        match (&block.samples, self.alsa_format) {
            (Samples::PcmI32(s), Format::S16LE) => {
                for &v in s {
                    out.extend_from_slice(&(v as i16).to_le_bytes());
                }
            }
            (Samples::PcmI32(s), Format::S243LE) => {
                for &v in s {
                    out.extend_from_slice(&v.to_le_bytes()[..3]);
                }
            }
            (Samples::PcmI32(s), Format::S32LE) => {
                for &v in s {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            (Samples::Dsd(b), Format::DSDU8) => out.extend_from_slice(b),
            _ => {
                return Err(SinkError::Device(
                    "block layout does not match the ALSA format".into(),
                ))
            }
        }
        Ok(out)
    }
}

impl AudioSink for AlsaSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        let bytes = self.pack(block)?;
        let io = self.pcm.io_bytes();
        let mut offset = 0;
        let frame_bytes = bytes.len() / block.frames().max(1);
        while offset < bytes.len() {
            match io.writei(&bytes[offset..]) {
                Ok(frames) => offset += frames * frame_bytes,
                Err(e) => {
                    // Underrun or suspend: recover once and keep going; the signal path is
                    // unaffected, but the gap is audible, so log it.
                    tracing::warn!(output = %self.id, error = %e, "alsa write error, recovering");
                    self.pcm.try_recover(e, true).map_err(alsa_err)?;
                }
            }
        }
        self.written += block.frames() as u64;
        Ok(())
    }

    fn buffer_level(&self) -> BufferLevel {
        let avail = self.pcm.avail_update().unwrap_or(0).max(0) as u64;
        BufferLevel {
            queued_frames: self.buffer_frames.saturating_sub(avail),
            capacity_frames: self.buffer_frames,
        }
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        if self.pcm.state() == State::Running {
            self.pcm.drop().map_err(alsa_err)?;
        }
        self.pcm.prepare().map_err(alsa_err)
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        self.pcm.drain().map_err(alsa_err)
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}
