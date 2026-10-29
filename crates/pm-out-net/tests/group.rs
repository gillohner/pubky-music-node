//! The app's players on `hw:` DACs in one node group (ADR 0009): offered as groupable, held in
//! sync like squeezelite players. Each endpoint's `Ack` is the node's only reading of that DAC's
//! clock; a member whose clock runs fast or slow is corrected by sample slip, and one whose
//! `Ack`s come later than the leader's (a slower network) is not taken for one that runs ahead.

use async_trait::async_trait;
use pm_endpoint::{Endpoint, Serving};
use pm_model::{AudioBlock, AudioFormat, OutputCaps};
use pm_out_net::{KeyCell, NetOutput};
use pm_stream::Key;
use pm_zones::outputs::{GroupOutput, NullOutput};
use pm_zones::{
    AudioSink, BufferLevel, MemberStatus, Output, OutputId, OutputRegistry, SinkError, SyncClass,
};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The group's deadband: no correction while members stay this close.
const DEADBAND_MS: i64 = 2;

/// How a test DAC plays: from a clock the test moves, at `ratio` times its rate, or in real
/// time with every write taking `late` longer once the block is in.
#[derive(Clone)]
enum Clock {
    Virtual { at: Arc<AtomicU64>, ratio: f64 },
    Real { late: Duration },
}

/// A DAC on an endpoint, with the id of an ALSA `hw:` device.
struct Dac {
    id: OutputId,
    clock: Clock,
    caps: OutputCaps,
    written: Arc<AtomicU64>,
}

impl Dac {
    fn new(device: &str, clock: Clock) -> Arc<Self> {
        let mut caps = OutputCaps::unlimited();
        caps.dsd = None;
        Arc::new(Self {
            id: OutputId::new("alsa", device),
            clock,
            caps,
            written: Arc::default(),
        })
    }
}

#[async_trait]
impl Output for Dac {
    fn id(&self) -> OutputId {
        self.id.clone()
    }
    fn name(&self) -> String {
        self.id.0.clone()
    }
    fn caps(&self) -> &OutputCaps {
        &self.caps
    }
    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        self.written.store(0, Relaxed);
        Ok(Box::new(DacSink {
            fmt,
            clock: self.clock.clone(),
            written: self.written.clone(),
            started: Mutex::new(None),
        }))
    }
}

/// Frames the device holds; a real-time DAC's write waits for room, like ALSA's.
const CAPACITY: u64 = 4800;

struct DacSink {
    fmt: AudioFormat,
    clock: Clock,
    written: Arc<AtomicU64>,
    started: Mutex<Option<Instant>>,
}

impl DacSink {
    fn played(&self) -> u64 {
        let written = self.written.load(Relaxed);
        let played = match &self.clock {
            Clock::Virtual { at, ratio } => (at.load(Relaxed) as f64 * ratio) as u64,
            Clock::Real { .. } => self.started.lock().unwrap().map_or(0, |t| {
                (t.elapsed().as_micros() * self.fmt.rate_hz as u128 / 1_000_000) as u64
            }),
        };
        played.min(written)
    }
}

impl AudioSink for DacSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }
    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        let frames = block.frames() as u64;
        if let Clock::Real { late } = self.clock {
            self.started.lock().unwrap().get_or_insert_with(Instant::now);
            while self.written.load(Relaxed) + frames - self.played() > CAPACITY {
                std::thread::sleep(Duration::from_micros(500));
            }
            self.written.fetch_add(frames, Relaxed);
            std::thread::sleep(late);
        } else {
            self.written.fetch_add(frames, Relaxed);
        }
        Ok(())
    }
    fn buffer_level(&self) -> BufferLevel {
        BufferLevel {
            queued_frames: self.written.load(Relaxed) - self.played(),
            capacity_frames: CAPACITY,
        }
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
    fn drain(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
    fn frames_written(&self) -> u64 {
        self.written.load(Relaxed)
    }
}

/// An endpoint (the app's player) offering `outputs`, and the node's outputs for them.
fn endpoint(outputs: Vec<Arc<dyn Output>>) -> (Serving, Vec<NetOutput>) {
    let registry = OutputRegistry::new();
    for o in outputs {
        registry.register(o);
    }
    let key = Key::random();
    let serving = Endpoint::new("Laptop", registry, key.clone())
        .spawn(TcpListener::bind("127.0.0.1:0").unwrap())
        .unwrap();
    let outs = NetOutput::connect(serving.addr(), &KeyCell::new(key)).unwrap();
    (serving, outs)
}

/// The leader and its members, each on an endpoint of its own; the group plays 10 ms blocks.
fn group(dacs: &[Arc<Dac>], correct: bool) -> (Vec<Serving>, Box<dyn AudioSink>, AudioBlock) {
    let mut serving = Vec::new();
    let mut members: Vec<Arc<dyn Output>> = Vec::new();
    for d in dacs {
        let (s, mut outs) = endpoint(vec![d.clone()]);
        serving.push(s);
        members.push(Arc::new(outs.remove(0)));
    }
    let g = GroupOutput::new("Everywhere", members)
        .unwrap()
        .with_drift_correction(correct);
    assert_eq!(g.sync(), SyncClass::Tight);
    let fmt = AudioFormat::pcm(48_000, 24, 2);
    let sink = pm_stream::block_on(g.open(fmt)).unwrap();
    (
        serving,
        sink,
        AudioBlock::pcm_i32(fmt, (0..960).map(|i| i * 1000).collect()),
    )
}

#[test]
fn players_on_hw_dacs_are_groupable_and_others_not() {
    let (_serving, outs) = endpoint(vec![
        Dac::new("hw:1,0", Clock::Real { late: Duration::ZERO }),
        Arc::new(NullOutput::new()),
    ]);
    let groupable = |remote: &str| {
        outs.iter()
            .find(|o| o.remote_id().protocol() == remote)
            .unwrap()
            .caps()
            .groupable
    };
    assert!(groupable("alsa"), "a hw: DAC reports its clock");
    assert!(!groupable("null"));
    // Two of them group in sync
    let (_other, more) = endpoint(vec![Dac::new("hw:0,0", Clock::Real { late: Duration::ZERO })]);
    let pair: Vec<Arc<dyn Output>> = vec![
        Arc::new(
            outs.into_iter()
                .find(|o| o.remote_id().protocol() == "alsa")
                .unwrap(),
        ),
        Arc::new(more.into_iter().next().unwrap()),
    ];
    let g = GroupOutput::new("Pair", pair).unwrap();
    assert_eq!(g.sync(), SyncClass::Tight);
    assert!(g.caps().groupable);
}

/// Thirty seconds at 48 kHz with DACs whose clocks run 500 ppm fast and slow, read through
/// their endpoints' `Ack`s.
fn run_clocks(correct: bool) -> (Vec<MemberStatus>, [u64; 3]) {
    let at = Arc::new(AtomicU64::new(0));
    let dac = |device, ratio| {
        Dac::new(
            device,
            Clock::Virtual {
                at: at.clone(),
                ratio,
            },
        )
    };
    let dacs = [dac("hw:0,0", 1.0), dac("hw:1,0", 1.0005), dac("hw:2,0", 0.9995)];
    let (_serving, mut sink, block) = group(&dacs, correct);
    // The leader stays about 100 ms ahead of the clock, as a blocking write keeps it
    for n in 1..=3000u64 {
        sink.write(&block).unwrap();
        at.store((n * 480).saturating_sub(4800), Relaxed);
    }
    let members = sink.members();
    drop(sink);
    (members, dacs.map(|d| d.written.load(Relaxed)))
}

#[test]
fn drift_between_players_is_corrected_by_sample_slip() {
    let (m, [lead, fast, slow]) = run_clocks(true);
    assert_eq!(lead, 3000 * 480);
    assert_eq!(m[0].slipped_frames, 0, "the leader is never touched");
    for s in &m[1..] {
        assert!(s.drift_ms.abs() <= DEADBAND_MS, "{s:?}");
    }
    // 500 ppm over 30 s is 720 frames; the deadband leaves up to 96 uncorrected
    assert!((550..=780).contains(&m[1].slipped_frames), "{:?}", m[1]);
    assert!((-780..=-550).contains(&m[2].slipped_frames), "{:?}", m[2]);
    assert_eq!(fast as i64, lead as i64 + m[1].slipped_frames);
    assert_eq!(slow as i64, lead as i64 + m[2].slipped_frames);
    // Without correction the same clocks end 15 ms apart, and are only reported
    let (m, written) = run_clocks(false);
    assert!(written.iter().all(|w| *w == lead));
    assert!(m[1].drift_ms >= 14 && m[2].drift_ms <= -14, "{m:?}");
}

/// Two DACs on one clock (real time), the member's `Ack`s 6 ms after the leader's: the group
/// measures both at one instant, so the member must not seem 6 ms ahead and be slipped back.
#[test]
fn a_player_whose_acks_come_late_is_not_taken_for_one_ahead() {
    let dacs = [
        Dac::new("hw:0,0", Clock::Real { late: Duration::ZERO }),
        Dac::new(
            "hw:1,0",
            Clock::Real {
                late: Duration::from_millis(6),
            },
        ),
    ];
    let (_serving, mut sink, block) = group(&dacs, true);
    // 4.5 s: correction starts after 2 s, and 6 ms (288 frames) seen as drift would be slipped
    // at a frame a block (10 ms) down to the deadband: about 190 frames by the end
    for _ in 0..450 {
        sink.write(&block).unwrap();
    }
    let m = sink.members();
    assert!(m[1].slipped_frames.abs() <= 24, "{:?}", m[1]);
    assert!(m[1].drift_ms.abs() <= DEADBAND_MS, "{:?}", m[1]);
}
