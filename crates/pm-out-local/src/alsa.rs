//! ALSA `hw:` output. Tested on a FiiO KA17 (see `docs/device-db/fiio-ka17.yaml`).
//!
//! Rules that make this bit-perfect:
//! * only `hw:` device names (no `plughw:`, no `default`);
//! * `set_rate_resample(false)` and an exact-rate check after `hw_params`;
//! * the sample format is chosen from the block's bit depth: 16 → `S16_LE`,
//!   24 → `S24_3LE` or `S24_LE` (both hold the 24 bits unchanged), 32 →
//!   `S32_LE`. A device with no 24-bit format (most UAC2 DACs) lists
//!   `pcm_bits = [16, 32]` in its caps so the pipeline plans a `pad` stage;
//! * DSD → `DSD_U8`, which is exactly the `pm_model` DSD byte layout, else
//!   `DSD_U32_BE` (what snd-usb-audio exposes for ESS/XMOS DACs: four bytes of
//!   one channel per word, oldest byte first), else DoP via 24-bit PCM. Both
//!   native formats are byte re-orderings, no bit is changed;
//! * no software volume: `set_volume` goes to the card's mixer control when
//!   it has one, else returns `NoVolume` and the pipeline handles it.

use alsa::mixer::{MilliBel, Mixer, Selem, SelemChannelId};
use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, Round, ValueOr};
use async_trait::async_trait;
use pm_model::{
    AudioBlock, AudioFormat, DsdSupport, OutputCaps, QualityCeiling, SampleKind, Samples, Volume, VolumeKind,
};
use pm_zones::sink::check_block;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, SinkError};

const CANDIDATE_RATES: [u32; 14] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000, 8_000, 16_000,
    22_050, 32_000,
];

/// DSD rates from DSD512 down; the probe advertises the highest one the device opens.
const DSD_RATES: [u32; 4] = [
    AudioFormat::DSD512_RATE,
    AudioFormat::DSD256_RATE,
    AudioFormat::DSD128_RATE,
    AudioFormat::DSD64_RATE,
];

/// DSD idle pattern: fills the last `DSD_U32_BE` word and the end-of-stream pad.
const DSD_SILENCE: u8 = 0x69;

pub struct AlsaOutput {
    device: String,
    name: String,
    caps: OutputCaps,
}

/// The ALSA formats that carry `bits`-wide PCM words unchanged, in order of preference.
fn pcm_formats(bits: u8) -> &'static [Format] {
    match bits {
        16 => &[Format::S16LE],
        24 => &[Format::S243LE, Format::S24LE],
        32 => &[Format::S32LE],
        _ => &[],
    }
}

/// Native DSD formats and how many DSD bytes per channel one ALSA frame holds.
const DSD_FORMATS: [(Format, u32); 2] = [(Format::DSDU8, 1), (Format::DSDU32BE, 4)];

/// The first format in `formats` the device takes, if any.
fn first_supported(hwp: &HwParams, formats: &[Format]) -> Option<Format> {
    formats.iter().copied().find(|f| hwp.test_format(*f).is_ok())
}

/// Does the device open `format` at `rate` frames per second?
fn takes_rate(pcm: &PCM, format: Format, rate: u32) -> bool {
    let Ok(hwp) = HwParams::any(pcm) else { return false };
    hwp.set_rate_resample(false).is_ok() && hwp.set_format(format).is_ok() && hwp.test_rate(rate).is_ok()
}

impl AlsaOutput {
    /// Probe a `hw:` device for its capabilities and wrap it as an [`Output`].
    pub fn probe(device: &str, name: &str) -> Result<Self, SinkError> {
        if !crate::is_direct_hw_name(device) {
            return Err(SinkError::Device(format!("refusing non-hw device {device}")));
        }
        let pcm = PCM::new(device, Direction::Playback, false).map_err(alsa_err)?;
        let hwp = HwParams::any(&pcm).map_err(alsa_err)?;
        hwp.set_rate_resample(false).map_err(alsa_err)?;
        let mut pcm_rates = Vec::new();
        for r in CANDIDATE_RATES {
            if hwp.test_rate(r).is_ok() {
                pcm_rates.push(r);
            }
        }
        pcm_rates.sort_unstable();
        let pcm_bits: Vec<u8> = [16u8, 24, 32]
            .into_iter()
            .filter(|b| first_supported(&hwp, pcm_formats(*b)).is_some())
            .collect();
        let Some(&max_pcm_bits) = pcm_bits.last() else {
            return Err(SinkError::Device(format!("{device}: no integer PCM format")));
        };
        let max_channels = hwp.get_channels_max().map_err(alsa_err)?.min(8) as u8;
        let native = DSD_FORMATS
            .iter()
            .find(|(f, _)| hwp.test_format(*f).is_ok())
            .and_then(|&(f, bytes)| {
                DSD_RATES
                    .into_iter()
                    .find(|r| takes_rate(&pcm, f, r / (8 * bytes)))
                    .map(|max_rate| DsdSupport::Native { max_rate })
            });
        // ALSA cannot tell whether a DAC decodes DoP; a 24-bit word at 176.4 kHz is necessary,
        // not sufficient (HDMI sinks pass this test too).
        let dsd = native.or_else(|| {
            (pcm_bits.contains(&24) && pcm_rates.contains(&176_400)).then_some(DsdSupport::DoP {
                max_rate: AudioFormat::DSD128_RATE,
            })
        });
        drop(hwp);
        drop(pcm);
        let hw_volume = card_mixer(device)
            .ok()
            .and_then(|m| volume_selem(&m).map(|s| volume_kind(&s)));
        Ok(Self {
            device: device.to_string(),
            name: name.to_string(),
            caps: OutputCaps {
                pcm_rates,
                max_pcm_bits,
                pcm_bits,
                max_channels,
                dsd,
                hw_volume,
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
            for dev in alsa::ctl::DeviceIter::new(&ctl) {
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

/// The control device of the card behind a PCM name: `hw:1,0` → `hw:1`.
fn card_mixer(device: &str) -> Result<Mixer, SinkError> {
    let card = device.split(',').next().unwrap_or(device);
    Mixer::new(card, false).map_err(alsa_err)
}

/// The playback volume control to drive: `Master`, then `PCM`, then a mono
/// control (the UAC2 feature unit's master channel, which snd-usb-audio
/// names after the card, e.g. `'FIIO KA17 ',1`), then the first one.
fn volume_selem(mixer: &Mixer) -> Option<Selem<'_>> {
    let controls: Vec<Selem> = mixer
        .iter()
        .filter_map(Selem::new)
        .filter(|s| s.has_playback_volume())
        .collect();
    let named = |n: &str| {
        controls
            .iter()
            .position(|s| s.get_id().get_name().is_ok_and(|x| x == n))
    };
    let pick = named("Master")
        .or_else(|| named("PCM"))
        .or_else(|| controls.iter().position(|s| s.is_playback_mono()))
        .or(if controls.is_empty() { None } else { Some(0) })?;
    controls.into_iter().nth(pick)
}

fn volume_kind(s: &Selem) -> VolumeKind {
    let (lo, hi) = s.get_playback_db_range();
    let (vmin, vmax) = s.get_playback_volume_range();
    if hi.0 > lo.0 && vmax > vmin {
        VolumeKind::Db {
            min: lo.to_db(),
            max: hi.to_db(),
            step: (hi.to_db() - lo.to_db()) / (vmax - vmin) as f32,
        }
    } else {
        VolumeKind::Percent
    }
}

fn set_mixer_volume(device: &str, v: Volume) -> Result<(), SinkError> {
    let mixer = card_mixer(device)?;
    let s = volume_selem(&mixer).ok_or_else(|| SinkError::Device(format!("{device}: no volume control")))?;
    let (vmin, vmax) = s.get_playback_volume_range();
    let ch = SelemChannelId::mono();
    match v {
        Volume::Db(db) => s.set_playback_db_all(MilliBel::from_db(db), Round::Floor),
        Volume::Percent(p) => s.set_playback_volume_all(vmin + (vmax - vmin) * i64::from(p.min(100)) / 100),
        Volume::Up | Volume::Down => {
            let cur = s.get_playback_volume(ch).map_err(alsa_err)?;
            let next = if matches!(v, Volume::Up) { cur + 1 } else { cur - 1 };
            s.set_playback_volume_all(next.clamp(vmin, vmax))
        }
        Volume::Mute(m) if s.has_playback_switch() => s.set_playback_switch_all(i32::from(!m)),
        Volume::Mute(true) => s.set_playback_volume_all(vmin),
        Volume::Mute(false) => return Err(SinkError::Device(format!("{device}: no mute switch to clear"))),
    }
    .map_err(alsa_err)
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
        run_blocking(move || AlsaSink::open(&device, id, fmt)).map(|s| Box::new(s) as Box<dyn AudioSink>)
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        if self.caps.hw_volume.is_none() {
            return Err(SinkError::NoVolume(self.id()));
        }
        let device = self.device.clone();
        run_blocking(move || set_mixer_volume(&device, v))
    }
}

/// Run a blocking closure without depending on a specific runtime.
fn run_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(f).join().expect("alsa thread panicked")
}

/// Move whole `DSD_U32_BE` frames out of `pending` (interleaved, one byte per
/// channel): per channel, four consecutive DSD bytes, oldest in the most
/// significant (first) byte. A partial frame stays in `pending`.
fn pack_dsd_u32(pending: &mut Vec<u8>, channels: usize, out: &mut Vec<u8>) {
    let groups = pending.len() / (4 * channels);
    for g in 0..groups {
        let base = g * 4 * channels;
        for c in 0..channels {
            for k in 0..4 {
                out.push(pending[base + k * channels + c]);
            }
        }
    }
    pending.drain(..groups * 4 * channels);
}

struct AlsaSink {
    id: OutputId,
    pcm: PCM,
    fmt: AudioFormat,
    alsa_format: Format,
    /// Bytes of one ALSA frame (all channels).
    frame_bytes: usize,
    buffer_frames: u64,
    period_frames: u64,
    written: u64,
    /// Frames handed to ALSA, in device frames (differs from `written` for `DSD_U32_BE`).
    device_frames: u64,
    /// `DSD_U32_BE` only: interleaved DSD bytes not yet filling a 4-byte word.
    dsd_pending: Vec<u8>,
}

impl AlsaSink {
    fn open(device: &str, id: OutputId, fmt: AudioFormat) -> Result<Self, SinkError> {
        let pcm = PCM::new(device, Direction::Playback, false).map_err(alsa_err)?;
        let unsupported = || SinkError::UnsupportedFormat(id.clone(), fmt);
        let channels = fmt.channels as u32;
        let (buffer_frames, period_frames, alsa_format) = {
            let hwp = HwParams::any(&pcm).map_err(alsa_err)?;
            hwp.set_rate_resample(false).map_err(alsa_err)?;
            let (alsa_format, rate) = match fmt.kind {
                SampleKind::Pcm => (
                    first_supported(&hwp, pcm_formats(fmt.bits)).ok_or_else(unsupported)?,
                    fmt.rate_hz,
                ),
                // One ALSA frame holds `bytes` DSD bytes per channel: frame rate = bit rate / 8 / bytes.
                SampleKind::Dsd => {
                    let (f, bytes) = DSD_FORMATS
                        .iter()
                        .copied()
                        .find(|(f, _)| hwp.test_format(*f).is_ok())
                        .ok_or_else(unsupported)?;
                    (f, fmt.rate_hz / (8 * bytes))
                }
            };
            if hwp.test_channels(channels).is_err() {
                return Err(unsupported());
            }
            hwp.set_access(Access::RWInterleaved).map_err(alsa_err)?;
            hwp.set_channels(channels).map_err(alsa_err)?;
            hwp.set_format(alsa_format).map_err(alsa_err)?;
            hwp.set_rate(rate, ValueOr::Nearest).map_err(|_| unsupported())?;
            if hwp.get_rate().map_err(alsa_err)? != rate {
                return Err(unsupported());
            }
            // ~0.5 s of buffer: generous, the endpoint owns the clock (§6.1).
            hwp.set_buffer_size_near((rate / 2) as i64).map_err(alsa_err)?;
            pcm.hw_params(&hwp).map_err(alsa_err)?;
            (
                hwp.get_buffer_size().map_err(alsa_err)? as u64,
                hwp.get_period_size().map_err(alsa_err)? as u64,
                alsa_format,
            )
        };
        let frame_bytes = pcm.frames_to_bytes(1) as usize;
        tracing::debug!(%id, %fmt, format = %alsa_format, buffer_frames, "alsa open");
        Ok(Self {
            id,
            pcm,
            fmt,
            alsa_format,
            frame_bytes,
            buffer_frames,
            period_frames,
            written: 0,
            device_frames: 0,
            dsd_pending: Vec::new(),
        })
    }

    fn pack(&mut self, block: &AudioBlock) -> Result<Vec<u8>, SinkError> {
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
            // S24_LE is 24 bits right-justified and sign-extended in 32: the block layout itself.
            (Samples::PcmI32(s), Format::S24LE | Format::S32LE) => {
                for &v in s {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            (Samples::Dsd(b), Format::DSDU8) => out.extend_from_slice(b),
            (Samples::Dsd(b), Format::DSDU32BE) => {
                self.dsd_pending.extend_from_slice(b);
                pack_dsd_u32(&mut self.dsd_pending, self.fmt.channels as usize, &mut out);
            }
            _ => {
                return Err(SinkError::Device(
                    "block layout does not match the ALSA format".into(),
                ))
            }
        }
        Ok(out)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), SinkError> {
        let io = self.pcm.io_bytes();
        let mut offset = 0;
        while offset < bytes.len() {
            match io.writei(&bytes[offset..]) {
                Ok(frames) => {
                    offset += frames * self.frame_bytes;
                    self.device_frames += frames as u64;
                }
                Err(e) => {
                    // Underrun or suspend: recover once and keep going; the signal path is
                    // unaffected, but the gap is audible, so log it.
                    tracing::warn!(output = %self.id, error = %e, "alsa write error, recovering");
                    self.pcm.try_recover(e, true).map_err(alsa_err)?;
                }
            }
        }
        Ok(())
    }
}

impl AudioSink for AlsaSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        let bytes = self.pack(block)?;
        self.write_bytes(&bytes)?;
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
        self.dsd_pending.clear();
        if self.pcm.state() == State::Running {
            self.pcm.drop().map_err(alsa_err)?;
        }
        self.pcm.prepare().map_err(alsa_err)
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        if !self.dsd_pending.is_empty() {
            // A stream that is not a multiple of 4 bytes per channel: complete the last
            // word with DSD silence (at most 3 bytes, ~8.5 µs at DSD64).
            let ch = self.fmt.channels as usize;
            let fill = 4 * ch - self.dsd_pending.len() % (4 * ch);
            self.dsd_pending
                .resize(self.dsd_pending.len() + fill, DSD_SILENCE);
            let mut out = Vec::new();
            pack_dsd_u32(&mut self.dsd_pending, ch, &mut out);
            self.write_bytes(&out)?;
        }
        // While draining, the device runs past the last written frame (to the period end,
        // plus DMA/URB read-ahead) into whatever the ring still holds from one buffer ago:
        // an audible burst of old audio at the end of every track, found by the M1 loopback
        // test. The crate does not wrap ALSA's silence-fill sw_params, so pad instead: to
        // the period boundary, then one whole buffer of silence, so every frame the
        // hardware can overshoot into is silence we wrote. Costs ~0.5 s at end of stream.
        let p = self.period_frames.max(1);
        let pad = (p - self.device_frames % p) % p + self.buffer_frames;
        let silence = if self.fmt.kind == SampleKind::Dsd {
            DSD_SILENCE
        } else {
            0
        };
        self.write_bytes(&vec![silence; pad as usize * self.frame_bytes])?;
        self.pcm.drain().map_err(alsa_err)
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_formats_never_convert() {
        assert_eq!(pcm_formats(16), &[Format::S16LE]);
        assert_eq!(pcm_formats(24), &[Format::S243LE, Format::S24LE]);
        assert_eq!(pcm_formats(32), &[Format::S32LE]);
        assert!(pcm_formats(20).is_empty());
    }

    #[test]
    fn dsd_u32_be_groups_four_bytes_per_channel() {
        // L0 R0 L1 R1 ... L4 R4: one whole word per channel, then a partial one.
        let mut pending = vec![0x10, 0x20, 0x11, 0x21, 0x12, 0x22, 0x13, 0x23, 0x14, 0x24];
        let mut out = Vec::new();
        pack_dsd_u32(&mut pending, 2, &mut out);
        assert_eq!(out, [0x10, 0x11, 0x12, 0x13, 0x20, 0x21, 0x22, 0x23]);
        assert_eq!(pending, [0x14, 0x24]);
    }
}
