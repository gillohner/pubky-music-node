//! A group of outputs playing the same stream (handoff §6.5, first cut).
//!
//! [`GroupOutput`] presents several outputs as one: its caps are the
//! intersection, so the pipeline negotiates a format every member takes
//! as-is. [`GroupSink`] fans each block out to every member sink in order;
//! the slowest member paces the zone through its blocking `write`, which
//! keeps the members within one block of each other at the source side.
//!
//! What this does and does not do yet:
//! * **Start alignment**: members with a smaller `fixed_latency_ms` get a
//!   pre-roll of silence so the first sample becomes audible together.
//! * **Drift monitoring**: each member's audible position (frames written
//!   minus frames still queued) is compared with the leader's and reported
//!   through [`AudioSink::members`]; the zone publishes it. Nothing is
//!   resampled yet; drift correction (adaptive resampling shown in the
//!   signal path) is the M8 stage this reporting feeds.
//! * Members with no clock report (`ClockSource`) are treated as free
//!   running; the group is honest about that in `sync`.

use crate::sink::{check_block, AudioSink, BufferLevel, MemberStatus, Output, OutputId, SinkError};
use async_trait::async_trait;
use pm_model::{
    AudioBlock, AudioFormat, DsdSupport, OutputCaps, QualityCeiling, SampleKind, Samples, Volume, VolumeKind,
};
use std::sync::Arc;

pub struct GroupOutput {
    name: String,
    members: Vec<Arc<dyn Output>>,
    caps: OutputCaps,
}

impl GroupOutput {
    /// Group `members` (two or more); the first is the leader whose clock
    /// and position the zone reports.
    pub fn new(name: &str, members: Vec<Arc<dyn Output>>) -> Result<Self, SinkError> {
        if members.len() < 2 {
            return Err(SinkError::Device("a group needs at least two outputs".into()));
        }
        let caps = intersect_caps(members.iter().map(|m| m.caps()));
        if caps.pcm_rates.is_empty() && caps.dsd.is_none() {
            return Err(SinkError::Device(
                "these outputs share no sample rate; they cannot play one stream".into(),
            ));
        }
        Ok(Self {
            name: name.to_string(),
            members,
            caps,
        })
    }

    pub fn members(&self) -> &[Arc<dyn Output>] {
        &self.members
    }
}

/// The caps every member satisfies.
pub fn intersect_caps<'a>(mut caps: impl Iterator<Item = &'a OutputCaps>) -> OutputCaps {
    let Some(first) = caps.next() else {
        return OutputCaps::unlimited();
    };
    let mut out = first.clone();
    for c in caps {
        out.pcm_rates.retain(|r| c.pcm_rates.contains(r));
        out.max_pcm_bits = out.max_pcm_bits.min(c.max_pcm_bits);
        out.pcm_bits = match (out.pcm_bits.is_empty(), c.pcm_bits.is_empty()) {
            (true, true) => Vec::new(),
            (true, false) => c.pcm_bits.clone(),
            (false, true) => out.pcm_bits.clone(),
            (false, false) => out
                .pcm_bits
                .iter()
                .copied()
                .filter(|b| c.pcm_bits.contains(b))
                .collect(),
        };
        out.pcm_bits.retain(|b| *b <= out.max_pcm_bits);
        out.max_channels = out.max_channels.min(c.max_channels);
        out.dsd = match (&out.dsd, &c.dsd) {
            (Some(DsdSupport::Native { max_rate: a }), Some(DsdSupport::Native { max_rate: b })) => {
                Some(DsdSupport::Native { max_rate: *a.min(b) })
            }
            // Mixed native/DoP would need two framings of one stream: not one stream.
            _ => None,
        };
        out.hw_volume = match (&out.hw_volume, &c.hw_volume) {
            (Some(VolumeKind::Db { .. }), Some(VolumeKind::Db { min, max, step })) => {
                if let Some(VolumeKind::Db {
                    min: m0,
                    max: x0,
                    step: s0,
                }) = &out.hw_volume
                {
                    Some(VolumeKind::Db {
                        min: m0.max(*min),
                        max: x0.min(*max),
                        step: s0.max(*step),
                    })
                } else {
                    None
                }
            }
            (Some(_), Some(_)) => Some(VolumeKind::Percent),
            _ => None,
        };
        out.standby = out.standby && c.standby;
        out.source_switch = false;
        out.gapless = out.gapless && c.gapless;
        out.fixed_latency_ms = match (out.fixed_latency_ms, c.fixed_latency_ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        out.quality_ceiling = lower_ceiling(&out.quality_ceiling, &c.quality_ceiling);
    }
    out.groupable = true;
    out
}

fn lower_ceiling(a: &QualityCeiling, b: &QualityCeiling) -> QualityCeiling {
    match (a, b) {
        (QualityCeiling::Lossy, _) | (_, QualityCeiling::Lossy) => QualityCeiling::Lossy,
        (
            QualityCeiling::LosslessUpTo {
                rate_hz: r1,
                bits: b1,
            },
            QualityCeiling::LosslessUpTo {
                rate_hz: r2,
                bits: b2,
            },
        ) => QualityCeiling::LosslessUpTo {
            rate_hz: *r1.min(r2),
            bits: *b1.min(b2),
        },
        (l @ QualityCeiling::LosslessUpTo { .. }, _) | (_, l @ QualityCeiling::LosslessUpTo { .. }) => *l,
        _ => QualityCeiling::BitPerfect,
    }
}

#[async_trait]
impl Output for GroupOutput {
    fn id(&self) -> OutputId {
        let ids: Vec<String> = self.members.iter().map(|m| m.id().0).collect();
        OutputId::new("group", &ids.join("+"))
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let mut sinks = Vec::with_capacity(self.members.len());
        for m in &self.members {
            sinks.push((m.id(), m.caps().fixed_latency_ms.unwrap_or(0), m.open(fmt).await?));
        }
        let max_latency = sinks.iter().map(|(_, l, _)| *l).max().unwrap_or(0);
        let mut members = Vec::with_capacity(sinks.len());
        for (id, latency, sink) in sinks {
            let preroll = ms_to_frames(fmt, (max_latency - latency) as u64);
            members.push(Member {
                id,
                sink,
                preroll_frames: preroll,
            });
        }
        let mut g = GroupSink {
            fmt,
            members,
            written: 0,
        };
        g.preroll()?;
        Ok(Box::new(g))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        if self.caps.hw_volume.is_none() {
            return Err(SinkError::NoVolume(self.id()));
        }
        for m in &self.members {
            m.set_volume(v).await?;
        }
        Ok(())
    }

    async fn standby(&self, on: bool) -> Result<(), SinkError> {
        if !self.caps.standby {
            return Err(SinkError::NoStandby(self.id()));
        }
        for m in &self.members {
            m.standby(on).await?;
        }
        Ok(())
    }
}

fn ms_to_frames(fmt: AudioFormat, ms: u64) -> u64 {
    match fmt.kind {
        SampleKind::Pcm => ms * fmt.rate_hz as u64 / 1000,
        SampleKind::Dsd => ms * fmt.rate_hz as u64 / 8000,
    }
}

fn frames_to_ms(fmt: AudioFormat, frames: i64) -> i64 {
    let rate = fmt.rate_hz.max(1) as i64;
    match fmt.kind {
        SampleKind::Pcm => frames * 1000 / rate,
        SampleKind::Dsd => frames * 8000 / rate,
    }
}

struct Member {
    id: OutputId,
    sink: Box<dyn AudioSink>,
    /// Silence written before the first block to align start with the slowest member.
    preroll_frames: u64,
}

impl Member {
    /// Audible position estimate: written minus still queued, minus pre-roll.
    fn audible(&self) -> i64 {
        let written = self.sink.frames_written() as i64;
        let queued = self.sink.buffer_level().queued_frames as i64;
        written - queued - self.preroll_frames as i64
    }
}

pub struct GroupSink {
    fmt: AudioFormat,
    members: Vec<Member>,
    written: u64,
}

impl GroupSink {
    fn preroll(&mut self) -> Result<(), SinkError> {
        for m in self.members.iter_mut() {
            if m.preroll_frames == 0 {
                continue;
            }
            let block = silence(self.fmt, m.preroll_frames as usize);
            m.sink.write(&block)?;
        }
        Ok(())
    }
}

fn silence(fmt: AudioFormat, frames: usize) -> AudioBlock {
    let n = frames * fmt.channels as usize;
    match fmt.kind {
        SampleKind::Pcm => AudioBlock::pcm_i32(fmt, vec![0; n]),
        SampleKind::Dsd => AudioBlock::dsd(fmt, vec![0x69; n]),
    }
}

impl AudioSink for GroupSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        if let Samples::PcmF64(_) = block.samples {
            return Err(SinkError::FloatNotAccepted);
        }
        for m in self.members.iter_mut() {
            m.sink.write(block)?;
        }
        self.written += block.frames() as u64;
        Ok(())
    }

    /// The leader's level: the zone paces on it, the others follow through their own writes.
    fn buffer_level(&self) -> BufferLevel {
        self.members
            .first()
            .map(|m| m.sink.buffer_level())
            .unwrap_or(BufferLevel {
                queued_frames: 0,
                capacity_frames: 0,
            })
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        let mut first_err = None;
        for m in self.members.iter_mut() {
            if let Err(e) = m.sink.flush() {
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        let mut first_err = None;
        for m in self.members.iter_mut() {
            if let Err(e) = m.sink.drain() {
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn frames_written(&self) -> u64 {
        self.written
    }

    fn members(&self) -> Vec<MemberStatus> {
        let leader = self.members.first().map(|m| m.audible()).unwrap_or(0);
        self.members
            .iter()
            .map(|m| MemberStatus {
                output: m.id.clone(),
                drift_ms: frames_to_ms(self.fmt, m.audible() - leader),
                level: m.sink.buffer_level(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::{read_wav, NullOutput, WavFileOutput};

    #[tokio::test]
    async fn fans_out_identically_and_reports_members() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        let null = Arc::new(NullOutput::new());
        let g = GroupOutput::new(
            "Everywhere",
            vec![
                Arc::new(WavFileOutput::new(&a)),
                Arc::new(WavFileOutput::new(&b)),
                null.clone(),
            ],
        )
        .unwrap();
        assert!(g.id().0.starts_with("group:wav:"));
        assert_eq!(
            g.caps().dsd,
            None,
            "WAV outputs take no DSD, so neither does the group"
        );
        let fmt = AudioFormat::pcm(48_000, 24, 2);
        let mut sink = g.open(fmt).await.unwrap();
        let block = AudioBlock::pcm_i32(fmt, (0..600).map(|i| i * 1000 - 300_000).collect());
        sink.write(&block).unwrap();
        sink.write(&block).unwrap();
        assert_eq!(sink.frames_written(), 600);
        let members = sink.members();
        assert_eq!(members.len(), 3);
        assert!(members.iter().all(|m| m.drift_ms == 0));
        sink.drain().unwrap();
        drop(sink);
        let wa = read_wav(&a).unwrap();
        let wb = read_wav(&b).unwrap();
        assert_eq!(wa.samples, wb.samples);
        assert_eq!(wa.frames(), 600);
        assert_eq!(null.frames_total(), 600);
        // Volume: null and wav have no mixer → the group has none.
        assert!(matches!(
            g.set_volume(Volume::Db(-3.0)).await,
            Err(SinkError::NoVolume(_))
        ));
    }

    #[test]
    fn caps_intersection() {
        let mut a = OutputCaps::unlimited();
        a.pcm_rates = vec![44_100, 48_000, 96_000];
        a.pcm_bits = vec![16, 32];
        a.fixed_latency_ms = Some(120);
        let mut b = OutputCaps::unlimited();
        b.pcm_rates = vec![48_000, 96_000, 192_000];
        b.max_pcm_bits = 24;
        b.dsd = None;
        b.quality_ceiling = QualityCeiling::LosslessUpTo {
            rate_hz: 96_000,
            bits: 24,
        };
        let c = intersect_caps([&a, &b].into_iter());
        assert_eq!(c.pcm_rates, vec![48_000, 96_000]);
        assert_eq!(c.max_pcm_bits, 24);
        assert_eq!(c.pcm_bits, vec![16]);
        assert_eq!(c.dsd, None);
        assert_eq!(c.fixed_latency_ms, Some(120));
        assert!(matches!(
            c.quality_ceiling,
            QualityCeiling::LosslessUpTo {
                rate_hz: 96_000,
                bits: 24
            }
        ));
        assert!(c.groupable);
        let mut d = OutputCaps::unlimited();
        d.pcm_rates = vec![22_050];
        d.dsd = None;
        assert!(GroupOutput::new(
            "x",
            vec![
                Arc::new(NullOutput::with_caps(a)),
                Arc::new(NullOutput::with_caps(d))
            ]
        )
        .is_err());
    }

    #[test]
    fn latency_preroll_math() {
        let fmt = AudioFormat::pcm(48_000, 16, 2);
        assert_eq!(ms_to_frames(fmt, 120), 5760);
        assert_eq!(frames_to_ms(fmt, 5760), 120);
        let dsd = AudioFormat::dsd(AudioFormat::DSD64_RATE, 2);
        assert_eq!(ms_to_frames(dsd, 1000), AudioFormat::DSD64_RATE as u64 / 8);
    }
}
