//! A group of outputs playing the same stream (handoff §6.5).
//!
//! [`GroupOutput`] presents several outputs as one: its caps are the
//! intersection, so the pipeline negotiates a format every member takes
//! as-is. [`GroupSink`] fans each block out to every member sink in order;
//! the slowest member paces the zone through its blocking `write`, which
//! keeps the members within one block of each other at the source side.
//!
//! * **Start alignment**: members with a smaller `fixed_latency_ms` get a
//!   pre-roll of silence so the first sample becomes audible together.
//! * **Drift monitoring**: each member's audible position (frames written
//!   minus frames still queued, minus pre-roll and slips) is compared with
//!   the leader's and reported through [`AudioSink::members`].
//! * **Drift correction** (PCM only): a member whose clock runs ahead of
//!   the leader's gets single frames duplicated at the end of a block, one
//!   behind gets single frames dropped, at most one frame per
//!   [`SLIP_DIVISOR`] frames (0.1 %) and only once the drift exceeds
//!   [`DEADBAND_MS`]. Clock differences between consumer devices are a few
//!   hundred ppm, so the slips stay rare; they are counted per member in
//!   `slipped_frames` and the zone's signal path carries a `sync` stage for
//!   grouped outputs so no bit-perfect claim is made. Adaptive resampling
//!   would be the audibly cleaner correction; it is not built. DSD groups
//!   are not corrected.
//!
//! The measurement trusts each member's `buffer_level` to reflect what its
//! hardware has consumed; a sink that reports nothing queued (the null
//! output) appears perfectly in time.

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
    drift_correction: bool,
}

/// At most one slipped frame per this many frames written.
pub const SLIP_DIVISOR: usize = 1000;
/// No correction while the measured drift stays within this.
pub const DEADBAND_MS: u64 = 2;
/// Audio written before the first correction, so start-up buffer fills settle.
const WARMUP_MS: u64 = 2000;

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
            drift_correction: true,
        })
    }

    /// Turn sample-slip drift correction off (members then only report drift).
    pub fn with_drift_correction(mut self, on: bool) -> Self {
        self.drift_correction = on;
        self
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

    /// Tight only when every member is the node's own endpoint, or every member a Slimproto
    /// player: those share a clock the group corrects against. Anything mixed is loose. So is
    /// an endpoint playing through its system mixer (`cpal`: a phone, a Mac, a Windows PC):
    /// the system adds latency the group cannot see (hundreds of ms over Bluetooth).
    fn sync(&self) -> crate::SyncClass {
        let protocols: Vec<String> = self
            .members
            .iter()
            .map(|m| m.id().protocol().to_string())
            .collect();
        let same = protocols.windows(2).all(|w| w[0] == w[1]);
        let system_mixer = self.members.iter().any(|m| m.id().0.contains("/cpal:"));
        if same
            && !system_mixer
            && matches!(protocols.first().map(String::as_str), Some("net") | Some("slim"))
        {
            crate::SyncClass::Tight
        } else {
            crate::SyncClass::Loose
        }
    }

    fn now_playing(&self, item: Option<&crate::QueueItem>) {
        for m in &self.members {
            m.now_playing(item);
        }
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
                inserted: 0,
                dropped: 0,
            });
        }
        let mut g = GroupSink {
            fmt,
            members,
            written: 0,
            correct: self.drift_correction && fmt.kind == SampleKind::Pcm,
            warmup_frames: ms_to_frames(fmt, WARMUP_MS),
            deadband_frames: ms_to_frames(fmt, DEADBAND_MS) as i64,
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
    /// Frames duplicated / dropped by drift correction.
    inserted: u64,
    dropped: u64,
}

impl Member {
    /// Audible position in stream frames: written minus still queued, minus
    /// pre-roll and inserted frames, plus dropped ones.
    fn audible(&self) -> i64 {
        let written = self.sink.frames_written() as i64;
        let queued = self.sink.buffer_level().queued_frames as i64;
        written - queued - self.preroll_frames as i64 - self.inserted as i64 + self.dropped as i64
    }

    fn slipped(&self) -> i64 {
        self.inserted as i64 - self.dropped as i64
    }
}

pub struct GroupSink {
    fmt: AudioFormat,
    members: Vec<Member>,
    written: u64,
    correct: bool,
    warmup_frames: u64,
    deadband_frames: i64,
}

/// `block` with its last frame repeated `extra` times.
fn with_repeated_tail(block: &AudioBlock, extra: usize) -> AudioBlock {
    let ch = block.format.channels as usize;
    match &block.samples {
        Samples::PcmI32(v) => {
            let mut out = Vec::with_capacity(v.len() + extra * ch);
            out.extend_from_slice(v);
            let tail = &v[v.len() - ch..];
            for _ in 0..extra {
                out.extend_from_slice(tail);
            }
            AudioBlock::pcm_i32(block.format, out)
        }
        Samples::PcmF64(v) => {
            let mut out = Vec::with_capacity(v.len() + extra * ch);
            out.extend_from_slice(v);
            let tail = &v[v.len() - ch..];
            for _ in 0..extra {
                out.extend_from_slice(tail);
            }
            AudioBlock::pcm_f64(block.format, out)
        }
        Samples::Dsd(v) => AudioBlock::dsd(block.format, v.clone()),
    }
}

/// `block` without its last `cut` frames.
fn without_tail(block: &AudioBlock, cut: usize) -> AudioBlock {
    let ch = block.format.channels as usize;
    match &block.samples {
        Samples::PcmI32(v) => AudioBlock::pcm_i32(block.format, v[..v.len() - cut * ch].to_vec()),
        Samples::PcmF64(v) => AudioBlock::pcm_f64(block.format, v[..v.len() - cut * ch].to_vec()),
        Samples::Dsd(v) => AudioBlock::dsd(block.format, v.clone()),
    }
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
        let frames = block.frames();
        let leader = self.members.first().map(|m| m.audible()).unwrap_or(0);
        let correcting = self.correct && self.written >= self.warmup_frames && frames > 0;
        for (i, m) in self.members.iter_mut().enumerate() {
            if i == 0 || !correcting {
                m.sink.write(block)?;
                continue;
            }
            let drift = m.audible() - leader;
            if drift.abs() <= self.deadband_frames {
                m.sink.write(block)?;
                continue;
            }
            let slip = frames
                .div_ceil(SLIP_DIVISOR)
                .min(drift.unsigned_abs() as usize)
                .min(frames - 1);
            if drift > 0 {
                // Ahead: its clock consumed more, so it gets more to play.
                m.sink.write(&with_repeated_tail(block, slip))?;
                m.inserted += slip as u64;
            } else {
                m.sink.write(&without_tail(block, slip))?;
                m.dropped += slip as u64;
            }
        }
        self.written += frames as u64;
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
                slipped_frames: m.slipped(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::{read_wav, NullOutput, WavFileOutput};

    /// An output that only has an id (for the sync label).
    struct Named(OutputId, OutputCaps);

    #[async_trait]
    impl Output for Named {
        fn id(&self) -> OutputId {
            self.0.clone()
        }
        fn name(&self) -> String {
            self.0 .0.clone()
        }
        fn caps(&self) -> &OutputCaps {
            &self.1
        }
        async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
            Err(SinkError::UnsupportedFormat(self.id(), fmt))
        }
    }

    #[test]
    fn the_sync_label_is_honest() {
        let o = |p: &str, id: &str| {
            Arc::new(Named(OutputId::new(p, id), OutputCaps::unlimited())) as Arc<dyn Output>
        };
        let g = |m: Vec<Arc<dyn Output>>| GroupOutput::new("g", m).unwrap().sync();
        assert_eq!(g(vec![o("net", "a"), o("net", "b")]), crate::SyncClass::Tight);
        // A phone's (or Mac's) system output: its latency is unknown to the group.
        assert_eq!(
            g(vec![
                o("net", "10.0.0.5:7790/alsa:hw:1,0"),
                o("net", "10.0.0.9:7790/cpal:Speaker")
            ]),
            crate::SyncClass::Loose
        );
        assert_eq!(g(vec![o("slim", "a"), o("slim", "b")]), crate::SyncClass::Tight);
        assert_eq!(g(vec![o("net", "a"), o("slim", "b")]), crate::SyncClass::Loose);
        assert_eq!(
            g(vec![o("upnp", "a"), o("airplay", "b")]),
            crate::SyncClass::Loose
        );
        assert_eq!(g(vec![o("cast", "a"), o("cast", "b")]), crate::SyncClass::Loose);
        assert_eq!(o("alsa", "hw").sync(), crate::SyncClass::Single);
    }

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

    /// An output whose sink consumes frames from a shared virtual clock at
    /// `ratio` times the leader's rate; nothing blocks.
    struct ClockOutput {
        id: &'static str,
        ratio: f64,
        clock: Arc<std::sync::atomic::AtomicU64>,
        written: Arc<std::sync::atomic::AtomicU64>,
        caps: OutputCaps,
    }

    impl ClockOutput {
        fn new(id: &'static str, ratio: f64, clock: &Arc<std::sync::atomic::AtomicU64>) -> Arc<Self> {
            Arc::new(Self {
                id,
                ratio,
                clock: clock.clone(),
                written: Arc::default(),
                caps: OutputCaps::unlimited(),
            })
        }
    }

    struct ClockSink {
        fmt: AudioFormat,
        ratio: f64,
        clock: Arc<std::sync::atomic::AtomicU64>,
        written: Arc<std::sync::atomic::AtomicU64>,
    }

    #[async_trait]
    impl Output for ClockOutput {
        fn id(&self) -> OutputId {
            OutputId::new("clock", self.id)
        }
        fn name(&self) -> String {
            self.id.to_string()
        }
        fn caps(&self) -> &OutputCaps {
            &self.caps
        }
        async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
            Ok(Box::new(ClockSink {
                fmt,
                ratio: self.ratio,
                clock: self.clock.clone(),
                written: self.written.clone(),
            }))
        }
    }

    impl AudioSink for ClockSink {
        fn format(&self) -> AudioFormat {
            self.fmt
        }
        fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
            self.written
                .fetch_add(block.frames() as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn buffer_level(&self) -> BufferLevel {
            let written = self.written.load(std::sync::atomic::Ordering::Relaxed);
            let consumed = (self.clock.load(std::sync::atomic::Ordering::Relaxed) as f64 * self.ratio) as u64;
            BufferLevel {
                queued_frames: written.saturating_sub(consumed),
                capacity_frames: 1 << 20,
            }
        }
        fn flush(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
        fn drain(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
        fn frames_written(&self) -> u64 {
            self.written.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// Thirty seconds at 48 kHz with members whose clocks run 500 ppm fast
    /// and slow: with correction they stay within the deadband, without it
    /// they drift 15 ms apart.
    async fn run_clocks(correct: bool) -> (Vec<MemberStatus>, u64, u64, u64) {
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let lead = ClockOutput::new("lead", 1.0, &clock);
        let fast = ClockOutput::new("fast", 1.0005, &clock);
        let slow = ClockOutput::new("slow", 0.9995, &clock);
        let g = GroupOutput::new("sync", vec![lead.clone(), fast.clone(), slow.clone()])
            .unwrap()
            .with_drift_correction(correct);
        let fmt = AudioFormat::pcm(48_000, 24, 2);
        let mut sink = g.open(fmt).await.unwrap();
        let block = AudioBlock::pcm_i32(fmt, (0..960).map(|i| i * 1000).collect());
        // The zone keeps the leader about 100 ms ahead of its clock, as a
        // real blocking write would; the others consume at their own rate.
        for n in 1..=3000u64 {
            sink.write(&block).unwrap();
            clock.store(
                (n * 480).saturating_sub(4800),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        let load = |o: &Arc<ClockOutput>| o.written.load(std::sync::atomic::Ordering::Relaxed);
        (sink.members(), load(&lead), load(&fast), load(&slow))
    }

    #[tokio::test]
    async fn drift_is_corrected_by_sample_slip() {
        let (m, lead, fast, slow) = run_clocks(true).await;
        assert_eq!(lead, 3000 * 480);
        assert_eq!(m[0].slipped_frames, 0, "the leader is never touched");
        for s in &m[1..] {
            assert!(s.drift_ms.abs() <= DEADBAND_MS as i64, "{s:?}");
        }
        // 500 ppm over 30 s is 720 frames; the deadband leaves up to 96 uncorrected.
        assert!((600..=720).contains(&m[1].slipped_frames), "{:?}", m[1]);
        assert!((-720..=-600).contains(&m[2].slipped_frames), "{:?}", m[2]);
        assert_eq!(fast as i64, lead as i64 + m[1].slipped_frames);
        assert_eq!(slow as i64, lead as i64 + m[2].slipped_frames);
    }

    #[tokio::test]
    async fn without_correction_drift_is_only_reported() {
        let (m, lead, fast, slow) = run_clocks(false).await;
        assert_eq!(fast, lead);
        assert_eq!(slow, lead);
        assert!(m.iter().all(|s| s.slipped_frames == 0));
        assert!(m[1].drift_ms >= 14, "{:?}", m[1]);
        assert!(m[2].drift_ms <= -14, "{:?}", m[2]);
    }

    #[test]
    fn tail_helpers() {
        let fmt = AudioFormat::pcm(48_000, 24, 2);
        let b = AudioBlock::pcm_i32(fmt, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            with_repeated_tail(&b, 2).samples,
            Samples::PcmI32(vec![1, 2, 3, 4, 5, 6, 5, 6, 5, 6])
        );
        assert_eq!(without_tail(&b, 1).samples, Samples::PcmI32(vec![1, 2, 3, 4]));
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
