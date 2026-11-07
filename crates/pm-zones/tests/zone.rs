//! Zone engine behaviour with synthetic sources: gapless joins, format
//! changes, skipping, pause/seek positions, transport commands.
//!
//! Sources are gated: `next_block` waits for a permit, so a test controls
//! exactly how many blocks the engine has produced before it sends a
//! command. Samples carry their frame index, so the recorded output shows
//! every seek and repeat.

use pm_model::{AudioBlock, AudioFormat, OutputCaps, PathStage, Quality, SignalPath, Volume};
use pm_zones::outputs::NullOutput;
use pm_zones::{
    AudioSink, BufferLevel, FrameSource, Insert, Output, OutputId, Processor, QueueItem, Repeat, SinkError,
    SourceError, SourceFactory, Transport, Zone, ZoneEvent, ZoneId, Zones,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const BLOCK: u64 = 100;
const RATE: u32 = 10_000; // 100 frames = 10 ms

#[derive(Default)]
struct Gate {
    permits: Mutex<u64>,
    cv: Condvar,
    /// Engine threads currently blocked in `take`.
    waiting: AtomicU64,
}

impl Gate {
    fn allow(&self, n: u64) {
        *self.permits.lock().unwrap() += n;
        self.cv.notify_all();
    }

    fn take(&self) {
        let mut p = self.permits.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        self.waiting.fetch_add(1, Ordering::SeqCst);
        while *p == 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "gate starved: the test forgot a permit");
            p = self.cv.wait_timeout(p, left).unwrap().0;
        }
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        *p -= 1;
    }

    /// Wait until the engine is blocked inside `next_block`, so that a command
    /// sent now is handled right after the next permitted block lands.
    async fn blocked(&self) {
        wait_for(|| self.waiting.load(Ordering::SeqCst) == 1).await;
    }
}

struct PatternSource {
    fmt: AudioFormat,
    total: u64,
    pos: u64,
    gate: Arc<Gate>,
    /// An in-stream title announced after the second block (radio).
    title: Option<String>,
    /// Streams MP3 (a uri ending in `.mp3`), else FLAC.
    mp3: bool,
    /// How a seek goes (from the uri): `brokenseek` takes it and then fails to read (TIDAL's
    /// segmented streams before TASK-seek-segmented-streams-1104), `noseek` refuses it (radio).
    seek: SeekKind,
    /// A broken seek happened: the next read fails.
    broken: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SeekKind {
    Works,
    Broken,
    Refused,
}

impl FrameSource for PatternSource {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn duration_ms(&self) -> Option<u64> {
        Some(self.total * 1000 / self.fmt.rate_hz as u64)
    }

    fn next_block(&mut self) -> Result<Option<AudioBlock>, SourceError> {
        if self.broken {
            return Err(SourceError::Decode(
                "isomp4: out-of-bounds seek for a non-seekable stream".into(),
            ));
        }
        if self.pos >= self.total {
            return Ok(None);
        }
        self.gate.take();
        let n = BLOCK.min(self.total - self.pos);
        let ch = self.fmt.channels as u64;
        let samples = (self.pos..self.pos + n)
            .flat_map(|f| (0..ch).map(move |_| f as i32))
            .collect();
        self.pos += n;
        Ok(Some(AudioBlock::pcm_i32(self.fmt, samples)))
    }

    fn seek_ms(&mut self, ms: u64) -> Result<(), SourceError> {
        match self.seek {
            SeekKind::Works => {}
            SeekKind::Broken => {
                self.broken = true;
                return Ok(());
            }
            SeekKind::Refused => return Err(SourceError::Seek("a live stream".into())),
        }
        self.pos = (ms * self.fmt.rate_hz as u64 / 1000).min(self.total);
        Ok(())
    }

    fn finalize(&mut self) -> Option<bool> {
        Some(true)
    }

    fn stream_info(&self) -> Option<pm_zones::StreamInfo> {
        Some(pm_zones::StreamInfo {
            codec: if self.mp3 { "MP3" } else { "FLAC" }.into(),
            lossless: !self.mp3,
            bitrate_kbps: self.mp3.then_some(320),
            ..Default::default()
        })
    }
    fn stream_title(&mut self) -> Option<String> {
        if self.pos >= 2 * BLOCK {
            return self.title.take();
        }
        None
    }
}

struct Identity(AudioFormat, SignalPath);

impl Processor for Identity {
    fn source_format(&self) -> AudioFormat {
        self.0
    }
    fn output_format(&self) -> AudioFormat {
        self.0
    }
    fn signal_path(&self) -> &SignalPath {
        &self.1
    }
    fn process(&mut self, block: AudioBlock) -> Result<AudioBlock, SourceError> {
        Ok(block)
    }
}

struct Factory {
    tracks: BTreeMap<String, (AudioFormat, u64)>,
    gate: Arc<Gate>,
    /// Opens of `hang:` items wait for a permit here (a stream slow to answer), then fail.
    hang: Arc<Gate>,
}

impl SourceFactory for Factory {
    fn open(&self, item: &QueueItem) -> Result<Box<dyn FrameSource>, SourceError> {
        if item.uri.starts_with("hang:") {
            self.hang.take();
            return Err(SourceError::Open {
                uri: item.uri.clone(),
                reason: "no answer".into(),
            });
        }
        let (fmt, total) = self.tracks.get(&item.uri).ok_or_else(|| SourceError::Open {
            uri: item.uri.clone(),
            reason: "no such track".into(),
        })?;
        Ok(Box::new(PatternSource {
            fmt: *fmt,
            total: *total,
            pos: 0,
            gate: self.gate.clone(),
            title: item.uri.contains("icy").then(|| "Nils Frahm - Says".to_string()),
            mp3: item.uri.ends_with(".mp3"),
            seek: if item.uri.contains("brokenseek") {
                SeekKind::Broken
            } else if item.uri.contains("noseek") {
                SeekKind::Refused
            } else {
                SeekKind::Works
            },
            broken: false,
        }))
    }

    fn plan(
        &self,
        src: &dyn FrameSource,
        caps: &OutputCaps,
        _output: &OutputId,
    ) -> Result<Box<dyn Processor>, SourceError> {
        let source = src.format();
        if !caps.accepts(&source) {
            return Err(SourceError::Negotiate {
                format: source,
                reason: "not accepted".into(),
            });
        }
        let path = SignalPath {
            stages: vec![PathStage {
                name: "decode".into(),
                in_format: source,
                out_format: source,
                quality: Quality::BitPerfect,
                note: None,
            }],
        };
        Ok(Box::new(Identity(source, path)))
    }
}

/// Records every sample written, counts opens/flushes/drains.
#[derive(Default)]
struct Recorder {
    samples: Mutex<Vec<i32>>,
    opens: Mutex<u64>,
    flushes: Mutex<u64>,
    drains: Mutex<u64>,
    /// The sink pauses its device itself (UPnP, BluOS) instead of discarding its queue.
    device_pause: std::sync::atomic::AtomicBool,
    /// The next resume fails (the device dropped the stream during the pause).
    fail_resume: std::sync::atomic::AtomicBool,
    pauses: Mutex<u64>,
    resumes: Mutex<u64>,
    /// Each sink's stream carries this many frames at most (a renderer's stream of fixed size).
    room: Mutex<Option<u64>>,
    /// The device goes away once this many frames were written (a USB DAC unplugged).
    gone_after: Mutex<Option<u64>>,
    /// The device goes away while the sink plays out (at a song boundary).
    drain_gone: std::sync::atomic::AtomicBool,
    /// The output's settings changed: sinks open before this ask to be reopened.
    reopen: std::sync::atomic::AtomicBool,
    /// Opens from this one on find the device gone (unplugged while the output opens).
    open_gone_from: Mutex<Option<u64>>,
    /// The next write must go to a fresh sink (a phone's DAC that would go back to full level).
    reopen_now: std::sync::atomic::AtomicBool,
    /// The next resume finds the device gone (unplugged during the pause).
    resume_gone: std::sync::atomic::AtomicBool,
    /// Flushes find the device gone (unplugged while the zone waited)
    flush_gone: std::sync::atomic::AtomicBool,
    /// Gones the zone said it heard
    gone_reported: Mutex<u64>,
}

struct RecOutput {
    caps: OutputCaps,
    rec: Arc<Recorder>,
}

#[async_trait::async_trait]
impl Output for RecOutput {
    fn id(&self) -> OutputId {
        OutputId::new("rec", "")
    }
    fn name(&self) -> String {
        "recorder".into()
    }
    fn caps(&self) -> &OutputCaps {
        &self.caps
    }
    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        let n = {
            let mut o = self.rec.opens.lock().unwrap();
            *o += 1;
            *o
        };
        if self
            .rec
            .open_gone_from
            .lock()
            .unwrap()
            .is_some_and(|from| n >= from)
        {
            return Err(SinkError::Gone(
                OutputId::new("rec", ""),
                "FiiO KA17 is not where Android plays now".into(),
            ));
        }
        Ok(Box::new(RecSink {
            fmt,
            rec: self.rec.clone(),
            written: 0,
        }))
    }
    async fn set_volume(&self, _v: Volume) -> Result<(), SinkError> {
        Ok(())
    }
}

struct RecSink {
    fmt: AudioFormat,
    rec: Arc<Recorder>,
    written: u64,
}

impl AudioSink for RecSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }
    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        pm_zones::sink::check_block(self.fmt, block)?;
        if let Some(n) = *self.rec.gone_after.lock().unwrap() {
            if self.written + block.frames() as u64 > n {
                return Err(SinkError::Gone(
                    OutputId::new("rec", ""),
                    "FiiO KA17 was unplugged".into(),
                ));
            }
        }
        // A stream of fixed size takes nothing past its end (the HTTP answer would cut it off)
        if let Some(room) = *self.rec.room.lock().unwrap() {
            if self.written + block.frames() as u64 > room {
                return Err(SinkError::Device("past the end of its stream".into()));
            }
        }
        if let pm_model::Samples::PcmI32(s) = &block.samples {
            self.rec.samples.lock().unwrap().extend_from_slice(s);
        }
        self.written += block.frames() as u64;
        Ok(())
    }
    fn buffer_level(&self) -> BufferLevel {
        BufferLevel {
            queued_frames: 0,
            capacity_frames: 0,
        }
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        *self.rec.flushes.lock().unwrap() += 1;
        if self.rec.flush_gone.load(Ordering::SeqCst) {
            return Err(SinkError::Gone(
                OutputId::new("rec", ""),
                "FiiO KA17 was unplugged".into(),
            ));
        }
        Ok(())
    }
    fn gone_reported(&mut self) {
        *self.rec.gone_reported.lock().unwrap() += 1;
    }
    fn drain(&mut self) -> Result<(), SinkError> {
        *self.rec.drains.lock().unwrap() += 1;
        if self.rec.drain_gone.load(Ordering::SeqCst) {
            return Err(SinkError::Gone(
                OutputId::new("rec", ""),
                "FiiO KA17 was unplugged".into(),
            ));
        }
        Ok(())
    }
    fn reopen_at_track(&self) -> bool {
        self.rec.reopen.load(Ordering::SeqCst)
    }
    fn reopen_now(&self) -> bool {
        self.rec.reopen_now.swap(false, Ordering::SeqCst)
    }
    fn pause(&mut self) -> Result<bool, SinkError> {
        if !self.rec.device_pause.load(Ordering::SeqCst) {
            return self.flush().map(|()| false);
        }
        *self.rec.pauses.lock().unwrap() += 1;
        Ok(true)
    }
    fn resume(&mut self) -> Result<(), SinkError> {
        *self.rec.resumes.lock().unwrap() += 1;
        if self.rec.resume_gone.swap(false, Ordering::SeqCst) {
            return Err(SinkError::Gone(
                OutputId::new("rec", ""),
                "FiiO KA17 was unplugged".into(),
            ));
        }
        if self.rec.fail_resume.swap(false, Ordering::SeqCst) {
            return Err(SinkError::Device("listener went away during the pause".into()));
        }
        Ok(())
    }
    fn frames_written(&self) -> u64 {
        self.written
    }
    fn room_frames(&self) -> Option<u64> {
        self.rec
            .room
            .lock()
            .unwrap()
            .map(|r| r.saturating_sub(self.written))
    }
}

struct Rig {
    zone: Zone,
    gate: Arc<Gate>,
    hang: Arc<Gate>,
    rec: Arc<Recorder>,
    events: broadcast::Receiver<ZoneEvent>,
}

fn rig(tracks: &[(&str, u32, u64)]) -> Rig {
    let gate = Arc::new(Gate::default());
    let hang = Arc::new(Gate::default());
    let factory = Factory {
        tracks: tracks
            .iter()
            .map(|(u, rate, n)| (u.to_string(), (AudioFormat::pcm(*rate, 16, 1), *n)))
            .collect(),
        gate: gate.clone(),
        hang: hang.clone(),
    };
    let rec = Arc::new(Recorder::default());
    let output = Arc::new(RecOutput {
        caps: OutputCaps {
            pcm_rates: vec![RATE, 2 * RATE],
            ..OutputCaps::unlimited()
        },
        rec: rec.clone(),
    });
    let zone = Zone::new(ZoneId::new("t"), "Test", output, Arc::new(factory));
    let events = zone.subscribe();
    Rig {
        zone,
        gate,
        hang,
        rec,
        events,
    }
}

fn frames(from: u64, to: u64) -> Vec<i32> {
    (from..to).map(|f| f as i32).collect()
}

async fn wait_for(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn drain_events(rx: &mut broadcast::Receiver<ZoneEvent>) -> Vec<ZoneEvent> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

fn kinds(evs: &[ZoneEvent]) -> Vec<String> {
    evs.iter()
        .map(|e| match e {
            ZoneEvent::TrackStarted { item, gapless, .. } => format!("start:{}:{}", item.uri, gapless),
            ZoneEvent::TrackEnded { item, .. } => format!("end:{}", item.uri),
            ZoneEvent::Skipped { item, .. } => format!("skip:{}", item.uri),
            ZoneEvent::Paused { position_ms } => format!("pause:{position_ms}"),
            ZoneEvent::Resumed => "resume".into(),
            ZoneEvent::Seeked { position_ms } => format!("seek:{position_ms}"),
            ZoneEvent::Stopped => "stopped".into(),
            ZoneEvent::QueueChanged => "queue".into(),
            ZoneEvent::StreamTitle { title } => format!("title:{title}"),
            ZoneEvent::Error { message } => format!("error:{message}"),
        })
        .filter(|k| k != "queue")
        .collect()
}

#[tokio::test]
async fn gapless_join_keeps_one_sink_open() {
    let mut r = rig(&[("a", RATE, 250), ("b", RATE, 150)]);
    r.gate.allow(1000);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.transport, Transport::Stopped);
    assert_eq!(st.error, None);
    assert_eq!(st.queue_index, None, "a finished queue starts over on play");
    assert_eq!(*r.rec.opens.lock().unwrap(), 1);
    assert_eq!(
        *r.rec.drains.lock().unwrap(),
        1,
        "drained once, at the end of the queue"
    );
    let mut want = frames(0, 250);
    want.extend(frames(0, 150));
    assert_eq!(*r.rec.samples.lock().unwrap(), want);
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        ["start:a:false", "end:a", "start:b:true", "end:b", "stopped"]
    );
}

/// A sink of fixed size (a Linn's stream offered as a file) takes the next track gaplessly only
/// when the whole track fits in what is left (with a margin of 2% and 10 s); else the track
/// starts on a fresh sink after the old one played out. Every sample still plays, in order.
#[tokio::test]
async fn a_track_that_does_not_fit_the_sized_stream_starts_on_a_fresh_one() {
    // 150 frames + 3 (2%) + 10 s at RATE
    let need = 150 + 3 + 10 * RATE as u64;
    for (room, gapless, opens) in [(250 + need, true, 1), (250 + need - 1, false, 2)] {
        let mut r = rig(&[("a", RATE, 250), ("b", RATE, 150)]);
        *r.rec.room.lock().unwrap() = Some(room);
        r.gate.allow(1000);
        r.zone
            .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
        r.zone.play();
        let st = r.zone.wait_stopped().await;
        assert_eq!(st.error, None);
        assert_eq!(*r.rec.opens.lock().unwrap(), opens, "room {room}");
        let mut want = frames(0, 250);
        want.extend(frames(0, 150));
        assert_eq!(*r.rec.samples.lock().unwrap(), want);
        assert_eq!(
            kinds(&drain_events(&mut r.events)),
            [
                "start:a:false".to_string(),
                "end:a".into(),
                format!("start:b:{gapless}"),
                "end:b".into(),
                "stopped".into()
            ]
        );
    }
}

/// A track longer than it said fills its sized stream: the zone plays it out and goes on on a
/// fresh one from the same place, without an error or a lost or repeated sample.
#[tokio::test]
async fn a_full_sized_stream_goes_on_on_a_fresh_sink() {
    let mut r = rig(&[("a", RATE, 250)]);
    *r.rec.room.lock().unwrap() = Some(120);
    r.gate.allow(1000);
    r.zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    r.zone.play();
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    // 100 + 100 does not fit 120, nor 100 + 50: three streams
    assert_eq!(*r.rec.opens.lock().unwrap(), 3);
    assert_eq!(*r.rec.samples.lock().unwrap(), frames(0, 250));
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        ["start:a:false", "end:a", "stopped"]
    );
}

#[tokio::test]
async fn format_change_drains_and_reopens() {
    let mut r = rig(&[("a", RATE, 100), ("b", 2 * RATE, 100)]);
    r.gate.allow(1000);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    r.zone.wait_stopped().await;
    assert_eq!(*r.rec.opens.lock().unwrap(), 2);
    assert_eq!(*r.rec.drains.lock().unwrap(), 2);
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        ["start:a:false", "end:a", "start:b:false", "end:b", "stopped"]
    );
}

#[tokio::test]
async fn unopenable_items_are_skipped() {
    let mut r = rig(&[("a", RATE, 100)]);
    r.gate.allow(1000);
    r.zone.enqueue(
        vec![
            QueueItem::new("missing"),
            QueueItem::new("a"),
            QueueItem::new("gone"),
        ],
        Insert::End,
    );
    r.zone.play();
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    assert_eq!(*r.rec.samples.lock().unwrap(), frames(0, 100));
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        ["skip:missing", "start:a:false", "end:a", "skip:gone", "stopped"]
    );

    // Nothing playable at all: an error, stopped.
    let r2 = rig(&[]);
    r2.zone.enqueue(vec![QueueItem::new("x")], Insert::End);
    r2.zone.play();
    let st = r2.zone.wait_stopped().await;
    assert!(st.error.as_deref().unwrap_or("").contains("nothing"), "{st:?}");
}

/// Items that are slow to fail (streams whose server never answers) are skipped one after
/// another; a Stop that arrives meanwhile is taken at once, not after the whole pass.
#[tokio::test]
async fn a_stop_during_a_pass_over_dead_items_is_taken_at_once() {
    let mut r = rig(&[("a", RATE, 100)]);
    r.gate.allow(1000);
    r.zone.enqueue(
        (0..8)
            .map(|i| QueueItem::new(format!("hang:{i}")))
            .chain([QueueItem::new("a")])
            .collect(),
        Insert::End,
    );
    r.zone.play();
    // The first two opens end; the third is in progress when Stop arrives
    r.hang.allow(2);
    wait_for(|| r.hang.waiting.load(Ordering::SeqCst) == 1).await;
    r.zone.stop();
    r.hang.allow(1);
    // The Stop is taken (acknowledged) as soon as that open fails, not after the other five
    tokio::time::timeout(Duration::from_secs(2), r.zone.wait_stopped())
        .await
        .expect("Stop is taken between two opens");
    // No more items were tried, and nothing played
    assert_eq!(r.hang.waiting.load(Ordering::SeqCst), 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        *r.hang.permits.lock().unwrap(),
        0,
        "the pass went on past the Stop"
    );
    assert!(r.rec.samples.lock().unwrap().is_empty());
    let k = kinds(&drain_events(&mut r.events));
    assert_eq!(k.iter().filter(|k| k.starts_with("skip:")).count(), 3, "{k:?}");
    assert!(r.zone.state().error.is_none(), "{:?}", r.zone.state());
    // Play afterwards runs the pass from the start again (the remaining items fail at once)
    r.hang.allow(100);
    r.zone.play();
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    assert_eq!(*r.rec.samples.lock().unwrap(), frames(0, 100));
}

/// A track that ends on its own, followed by items that are slow to fail: the pass to the next
/// item takes Stop while the next item (opened ahead) is still opening, and while a later one is
/// opened on the audio thread.
#[tokio::test]
async fn a_stop_after_a_natural_end_into_dead_items_is_taken_at_once() {
    for later in [false, true] {
        let r = rig(&[("a", RATE, 100)]);
        r.gate.allow(1000);
        r.zone.enqueue(
            std::iter::once(QueueItem::new("a"))
                .chain((0..5).map(|i| QueueItem::new(format!("hang:{i}"))))
                .collect(),
            Insert::End,
        );
        r.zone.play();
        // The next item, opened ahead, waits; with `later` it fails and the one after it waits
        // on the audio thread
        wait_for(|| r.hang.waiting.load(Ordering::SeqCst) == 1).await;
        if later {
            r.hang.allow(1);
            wait_for(|| *r.hang.permits.lock().unwrap() == 0).await;
            wait_for(|| r.hang.waiting.load(Ordering::SeqCst) == 1).await;
        }
        // Let a's one block end the track first
        wait_for(|| r.rec.samples.lock().unwrap().len() == 100).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        r.zone.stop();
        if later {
            r.hang.allow(1);
        }
        tokio::time::timeout(Duration::from_secs(2), r.zone.wait_stopped())
            .await
            .unwrap_or_else(|_| panic!("Stop is taken during the pass (later: {later})"));
        // Nothing more was tried
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            r.hang.waiting.load(Ordering::SeqCst),
            u64::from(!later),
            "later: {later}"
        );
        r.hang.allow(10);
    }
}

/// Pause, the toggle, Previous and Play that arrive while a pass skips items slow to fail don't
/// end the pass: they are served once the next playable track has started, as before.
#[tokio::test]
async fn other_commands_during_a_pass_are_served_after_it() {
    for cmd in ["pause", "toggle", "previous", "play"] {
        let r = rig(&[("a", RATE, 300)]);
        r.zone.enqueue(
            vec![
                QueueItem::new("hang:0"),
                QueueItem::new("hang:1"),
                QueueItem::new("a"),
            ],
            Insert::End,
        );
        r.zone.play();
        r.hang.allow(1);
        wait_for(|| *r.hang.permits.lock().unwrap() == 0).await;
        wait_for(|| r.hang.waiting.load(Ordering::SeqCst) == 1).await;
        match cmd {
            "pause" => r.zone.pause(),
            "toggle" => r.zone.toggle_pause(),
            "previous" => r.zone.previous(),
            _ => r.zone.play(),
        }
        r.hang.allow(1);
        r.gate.allow(1);
        let mut w = r.zone.watch();
        let st = tokio::time::timeout(
            Duration::from_secs(3),
            w.wait_for(|s| s.now_playing.is_some() && s.queue_index == Some(2)),
        )
        .await
        .unwrap_or_else(|_| panic!("{cmd}: the pass went on to the playable track"))
        .unwrap()
        .clone();
        match cmd {
            "pause" | "toggle" => {
                let st = tokio::time::timeout(
                    Duration::from_secs(2),
                    w.wait_for(|s| s.transport == Transport::Paused),
                )
                .await
                .unwrap_or_else(|_| panic!("{cmd}: paused on the track"))
                .unwrap()
                .clone();
                assert!(st.now_playing.is_some(), "{cmd}: {st:?}");
                // ...and Play resumes it
                r.gate.allow(1000);
                r.zone.play();
                tokio::time::timeout(
                    Duration::from_secs(2),
                    w.wait_for(|s| s.transport == Transport::Playing),
                )
                .await
                .unwrap_or_else(|_| panic!("{cmd}: Play resumes"))
                .unwrap();
            }
            _ => assert_eq!(st.transport, Transport::Playing, "{cmd}: {st:?}"),
        }
        r.gate.allow(1000);
        r.zone.stop();
        r.zone.wait_stopped().await;
    }
}

/// The next item is opened ahead on its own thread: a next item whose open doesn't return
/// never holds the audio thread, so Stop is taken while it is still opening.
#[tokio::test]
async fn a_next_item_slow_to_open_does_not_hold_the_zone() {
    let r = rig(&[("a", RATE, 300)]);
    r.zone.enqueue(
        vec![QueueItem::new("a"), QueueItem::new("hang:next")],
        Insert::End,
    );
    r.zone.play();
    r.gate.allow(1);
    // The prefetch of the next item has started and is waiting
    wait_for(|| r.hang.waiting.load(Ordering::SeqCst) == 1).await;
    r.gate.blocked().await;
    r.zone.stop();
    r.gate.allow(1);
    let mut w = r.zone.watch();
    tokio::time::timeout(
        Duration::from_secs(2),
        w.wait_for(|s| s.transport == Transport::Stopped),
    )
    .await
    .expect("Stop is taken while the next item is still opening")
    .unwrap();
    assert_eq!(
        r.hang.waiting.load(Ordering::SeqCst),
        1,
        "the open is still in progress"
    );
    r.hang.allow(1);
    wait_for(|| r.hang.waiting.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn pause_resume_seek_positions() {
    let mut r = rig(&[("a", RATE, 1000)]);
    r.zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    r.zone.play();
    // Three blocks out, the engine now waits for a permit inside next_block.
    r.gate.allow(3);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 300).await;
    // Pause is handled after the in-flight block (300..400) lands.
    r.gate.blocked().await;
    r.zone.pause();
    r.gate.allow(1);
    let mut w = r.zone.watch();
    w.wait_for(|s| s.transport == Transport::Paused).await.unwrap();
    let np = r.zone.state().now_playing.unwrap();
    assert_eq!(np.position_ms, 40);
    assert_eq!(np.duration_ms, Some(100));
    assert_eq!(*r.rec.flushes.lock().unwrap(), 1);

    // Resume seeks the source back to the audible position (40 ms = frame 400).
    r.zone.play();
    w.wait_for(|s| s.transport == Transport::Playing).await.unwrap();
    // Seek is handled after the in-flight block (400..500).
    r.gate.blocked().await;
    r.zone.seek_ms(60);
    r.gate.allow(1);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 500).await;
    r.gate.allow(1000);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    let mut want = frames(0, 500);
    want.extend(frames(600, 1000));
    assert_eq!(*r.rec.samples.lock().unwrap(), want);
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        [
            "start:a:false",
            "pause:40",
            "resume",
            "seek:60",
            "end:a",
            "stopped"
        ]
    );
}

/// A stream that takes a seek and then can't read (TIDAL's DASH before the segmented seek) goes
/// on from where it was: the track doesn't end and the queue doesn't move on, and the caller
/// hears that this stream cannot seek. One that refuses the seek (radio) keeps playing as it was.
#[tokio::test]
async fn a_seek_the_stream_cannot_follow_keeps_the_track_playing() {
    for (uri, reopened) in [("a-brokenseek", true), ("a-noseek", false)] {
        let mut r = rig(&[(uri, RATE, 1000), ("b", RATE, 300)]);
        r.zone
            .enqueue(vec![QueueItem::new(uri), QueueItem::new("b")], Insert::End);
        r.zone.play();
        r.gate.allow(3);
        wait_for(|| r.rec.samples.lock().unwrap().len() == 300).await;
        // The seek is handled after the in-flight block (300..400): from 40 ms
        r.gate.blocked().await;
        r.zone.seek_ms(70);
        r.gate.allow(1);
        wait_for(|| r.rec.samples.lock().unwrap().len() == 400).await;
        r.gate.allow(1000);
        let st = r.zone.wait_stopped().await;
        assert_eq!(st.error, None, "{uri}");
        let mut want = frames(0, 1000);
        want.extend(frames(0, 300));
        assert_eq!(
            *r.rec.samples.lock().unwrap(),
            want,
            "{uri}: on from 40 ms, then the next track"
        );
        let evs = kinds(&drain_events(&mut r.events));
        let mut expect = vec![format!("start:{uri}:false")];
        if reopened {
            expect.extend([
                "seek:70".to_string(),
                "seek:40".into(),
                "error:this stream cannot seek (decode error: isomp4: out-of-bounds seek for a non-seekable stream)".into(),
            ]);
        } else {
            expect.push("error:this stream cannot seek (seek failed: a live stream)".into());
        }
        expect.extend([
            format!("end:{uri}"),
            "start:b:true".into(),
            "end:b".into(),
            "stopped".into(),
        ]);
        assert_eq!(evs, expect, "{uri}");
    }
}

/// A sink whose device buffers (UPnP, BluOS) is told to pause, not flushed: the renderer stops
/// at once instead of playing out seconds of buffer (NUC, 2026-10-30). A resume it cannot carry
/// on (the device hung up during the pause) opens the output again at the paused position.
#[tokio::test]
async fn pause_pauses_the_device_and_a_failed_resume_reopens_the_output() {
    let mut r = rig(&[("a", RATE, 1000)]);
    r.rec.device_pause.store(true, Ordering::SeqCst);
    r.zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    r.zone.play();
    r.gate.allow(3);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 300).await;
    r.gate.blocked().await;
    r.zone.pause();
    r.gate.allow(1);
    let mut w = r.zone.watch();
    w.wait_for(|s| s.transport == Transport::Paused).await.unwrap();
    assert_eq!(*r.rec.pauses.lock().unwrap(), 1);
    assert_eq!(*r.rec.flushes.lock().unwrap(), 0, "the device keeps its buffer");

    // Resume: the device carries on, same sink
    r.zone.play();
    w.wait_for(|s| s.transport == Transport::Playing).await.unwrap();
    assert_eq!(*r.rec.resumes.lock().unwrap(), 1);
    r.gate.blocked().await;
    r.zone.pause();
    r.gate.allow(1);
    w.wait_for(|s| s.transport == Transport::Paused).await.unwrap();
    assert_eq!(r.zone.state().now_playing.unwrap().position_ms, 50);

    // The device dropped the stream meanwhile: a fresh sink, from the paused position
    r.rec.fail_resume.store(true, Ordering::SeqCst);
    r.zone.play();
    w.wait_for(|s| s.transport == Transport::Playing).await.unwrap();
    assert_eq!(*r.rec.opens.lock().unwrap(), 2, "the output opened again");
    r.gate.allow(1000);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    assert_eq!(
        *r.rec.samples.lock().unwrap(),
        frames(0, 1000),
        "nothing lost, nothing twice"
    );
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        [
            "start:a:false",
            "pause:40",
            "resume",
            "pause:50",
            "resume",
            "end:a",
            "stopped"
        ]
    );
}

/// Next (or a queue item) or a seek while paused: the paused device holds audio from before,
/// so its sink closes and the next play starts on a fresh one, never resuming the old buffer.
#[tokio::test]
async fn a_track_or_seek_while_paused_starts_on_a_fresh_sink() {
    let r = rig(&[("a", RATE, 1000), ("b", RATE, 300)]);
    r.rec.device_pause.store(true, Ordering::SeqCst);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    r.gate.allow(2);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 200).await;
    r.gate.blocked().await;
    r.zone.pause();
    r.gate.allow(1);
    let mut w = r.zone.watch();
    w.wait_for(|s| s.transport == Transport::Paused).await.unwrap();
    // Seek while paused, then play: a fresh sink from the new position
    r.zone.seek_ms(70);
    r.zone.play();
    w.wait_for(|s| s.transport == Transport::Playing).await.unwrap();
    assert_eq!(*r.rec.opens.lock().unwrap(), 2, "a fresh sink after the seek");
    r.gate.allow(1);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 400).await;
    r.gate.blocked().await;
    r.zone.pause();
    r.gate.allow(1);
    w.wait_for(|s| s.transport == Transport::Paused).await.unwrap();
    // Next while paused: the next track on a fresh sink, playing
    r.zone.next();
    w.wait_for(|s| s.transport == Transport::Playing && s.queue_index == Some(1))
        .await
        .unwrap();
    assert_eq!(*r.rec.opens.lock().unwrap(), 3, "a fresh sink for the next track");
    assert_eq!(*r.rec.resumes.lock().unwrap(), 0, "no stale buffer resumed");
    r.gate.allow(1000);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    let mut want = frames(0, 300);
    want.extend(frames(700, 900));
    want.extend(frames(0, 300));
    assert_eq!(*r.rec.samples.lock().unwrap(), want);
}

#[tokio::test]
async fn next_previous_stop_and_restart() {
    // Three blocks per track, so no track can end while a command is in flight.
    let mut r = rig(&[("a", RATE, 300), ("b", RATE, 300), ("c", RATE, 300)]);
    r.zone.enqueue(
        vec![QueueItem::new("a"), QueueItem::new("b"), QueueItem::new("c")],
        Insert::End,
    );
    r.zone.play();
    // Next → b; whether a's first block lands before it depends on timing, and either way
    // exactly one block is out after the first permit.
    r.zone.next();
    r.gate.allow(1);
    let mut w = r.zone.watch();
    w.wait_for(|s| s.queue_index == Some(1)).await.unwrap();
    wait_for(|| r.rec.samples.lock().unwrap().len() == 100).await;
    // Previous within 3 s → back to a (handled after b's in-flight block lands).
    r.gate.blocked().await;
    r.zone.previous();
    r.gate.allow(1);
    w.wait_for(|s| s.queue_index == Some(0) && s.now_playing.is_some())
        .await
        .unwrap();
    assert_eq!(r.rec.samples.lock().unwrap().len(), 200);
    // Stop keeps the index; play restarts a from the top.
    r.gate.blocked().await;
    r.zone.stop();
    r.gate.allow(1);
    w.wait_for(|s| s.transport == Transport::Stopped).await.unwrap();
    assert_eq!(r.zone.state().queue_index, Some(0));
    r.gate.allow(1000);
    r.zone.play();
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    let starts: Vec<_> = kinds(&drain_events(&mut r.events))
        .into_iter()
        .filter(|k| k.starts_with("start:"))
        .collect();
    assert_eq!(
        starts,
        [
            "start:a:false",
            "start:b:false",
            "start:a:false",
            "start:a:false",
            "start:b:true",
            "start:c:true"
        ]
    );
    // Stop discarded buffers (flush), never drained mid-queue.
    assert_eq!(*r.rec.drains.lock().unwrap(), 1);
}

#[tokio::test]
async fn repeat_one_and_queue_edits_while_playing() {
    let mut r = rig(&[("a", RATE, 100), ("b", RATE, 100)]);
    r.zone.set_repeat(Repeat::One);
    r.zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    r.zone.play();
    r.gate.allow(1); // a plays once...
    r.gate.allow(1); // ...and again (repeat one)
    wait_for(|| r.rec.samples.lock().unwrap().len() == 200).await;
    // Switch off repeat and append b: b follows, then the queue ends.
    r.zone.set_repeat(Repeat::Off);
    let ids = r.zone.enqueue(vec![QueueItem::new("b")], Insert::End);
    assert_eq!(ids.len(), 1);
    r.gate.allow(1000);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    let starts: Vec<_> = kinds(&drain_events(&mut r.events))
        .into_iter()
        .filter(|k| k.starts_with("start:"))
        .collect();
    assert_eq!(starts[0], "start:a:false");
    assert_eq!(starts[1], "start:a:true");
    assert_eq!(*starts.last().unwrap(), "start:b:true");
    assert_eq!(*r.rec.opens.lock().unwrap(), 1);
}

#[tokio::test]
async fn stream_titles_reach_state_and_events() {
    let mut r = rig(&[("icy-radio", RATE, 5 * BLOCK)]);
    r.zone.enqueue(vec![QueueItem::new("icy-radio")], Insert::End);
    r.zone.play_index(0);
    // Let three blocks through: the title is announced after the second.
    for _ in 0..3 {
        r.gate.blocked().await;
        r.gate.allow(1);
    }
    wait_for(|| {
        r.zone
            .state()
            .now_playing
            .as_ref()
            .and_then(|n| n.stream_title.clone())
            .is_some()
    })
    .await;
    let np = r.zone.state().now_playing.unwrap();
    assert_eq!(np.stream_title.as_deref(), Some("Nils Frahm - Says"));
    assert_eq!(np.item.uri, "icy-radio", "the queue item itself is untouched");
    let evs = kinds(&drain_events(&mut r.events));
    assert!(evs.contains(&"title:Nils Frahm - Says".to_string()), "{evs:?}");
    r.gate.allow(100);
    r.zone.wait_stopped().await;
}

#[tokio::test]
async fn zones_manager_ids_and_lookup() {
    let gate = Arc::new(Gate::default());
    let zones = Zones::new(Arc::new(Factory {
        tracks: BTreeMap::new(),
        gate,
        hang: Arc::new(Gate::default()),
    }));
    let out: Arc<dyn Output> = Arc::new(NullOutput::new());
    let z1 = zones.create("Living Room", out.clone());
    let z2 = zones.create("Living Room", out.clone());
    assert_eq!(z1.id().0, "living-room");
    assert_eq!(z2.id().0, "living-room-2");
    assert_eq!(zones.list().len(), 2);
    assert!(zones.for_output(&out.id()).is_some());
    assert!(zones.get(&ZoneId::new("living-room")).is_some());
    assert!(zones.remove(z2.id()).is_some());
    assert_eq!(zones.list().len(), 1);
    // Volume delegates to the output; the null output has none.
    assert!(matches!(
        z1.set_volume(Volume::Db(-10.0)).await,
        Err(SinkError::NoVolume(_))
    ));
}

#[tokio::test]
async fn crossfade_mixes_the_next_track_over_the_end() {
    // 100 ms tracks, 30 ms crossfade; blocks are 10 ms. The fade starts with the block that
    // reaches the last 30 ms (frames 600..700 end 30 ms before the end), so it covers a's last
    // 400 frames and b's first 400.
    let mut r = rig(&[("a", RATE, 1000), ("b", RATE, 1000)]);
    r.gate.allow(1000);
    r.zone.set_crossfade_ms(30);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    assert_eq!(st.crossfade_ms, 30);
    let got = r.rec.samples.lock().unwrap().clone();
    assert_eq!(got.len(), 1600, "the overlap is played once");
    assert_eq!(got[..600], frames(0, 600)[..], "a alone until the fade");
    assert_eq!(
        got[1000..],
        frames(400, 1000)[..],
        "b alone after it, from where the mix left it"
    );
    for i in 0..400u64 {
        let t = i as f64 / 400.0 * std::f64::consts::FRAC_PI_2;
        let want = ((600 + i) as f64 * t.cos() + i as f64 * t.sin()).round() as i32;
        assert_eq!(got[600 + i as usize], want, "equal-power mix at {i}");
    }
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        ["start:a:false", "end:a", "start:b:false", "end:b", "stopped"]
    );
    assert_eq!(*r.rec.opens.lock().unwrap(), 1, "one sink throughout");
}

/// A crossfade whose end doesn't fit what is left of a sized stream: the mix and the incoming
/// track's buffered rest go on on a fresh stream, nothing written past the end and nothing lost.
#[tokio::test]
async fn a_crossfade_into_a_full_sized_stream_loses_nothing() {
    let reference = {
        let r = rig(&[("a", RATE, 1050), ("b", RATE, 1000)]);
        r.gate.allow(1000);
        r.zone.set_crossfade_ms(30);
        r.zone
            .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
        r.zone.play();
        r.zone.wait_stopped().await;
        let got = r.rec.samples.lock().unwrap().clone();
        got
    };
    for room in [650, 1050, 1350] {
        let r = rig(&[("a", RATE, 1050), ("b", RATE, 1000)]);
        *r.rec.room.lock().unwrap() = Some(room);
        r.gate.allow(1000);
        r.zone.set_crossfade_ms(30);
        r.zone
            .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
        r.zone.play();
        let st = r.zone.wait_stopped().await;
        assert_eq!(st.error, None, "room {room}");
        assert!(*r.rec.opens.lock().unwrap() >= 2, "room {room}: a fresh stream");
        assert_eq!(*r.rec.samples.lock().unwrap(), reference, "room {room}");
    }
}

#[tokio::test]
async fn crossfade_leaves_an_album_gapless() {
    let mut r = rig(&[("a", RATE, 1000), ("b", RATE, 1000)]);
    r.gate.allow(1000);
    r.zone.set_crossfade_ms(30);
    let mut a = QueueItem::new("a");
    let mut b = QueueItem::new("b");
    a.album = Some("Kind of Blue".into());
    b.album = Some("Kind of Blue".into());
    r.zone.enqueue(vec![a, b], Insert::End);
    r.zone.play();
    r.zone.wait_stopped().await;
    let mut want = frames(0, 1000);
    want.extend(frames(0, 1000));
    assert_eq!(*r.rec.samples.lock().unwrap(), want);
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        ["start:a:false", "end:a", "start:b:true", "end:b", "stopped"]
    );
}

#[tokio::test]
async fn crossfade_waits_for_a_track_long_enough_and_a_matching_format() {
    // b changes format: no fade, the sink drains and reopens as without crossfade
    let r = rig(&[("a", RATE, 1000), ("b", 2 * RATE, 1000)]);
    r.gate.allow(1000);
    r.zone.set_crossfade_ms(30);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    r.zone.wait_stopped().await;
    assert_eq!(r.rec.samples.lock().unwrap().len(), 2000);
    assert_eq!(*r.rec.opens.lock().unwrap(), 2);
    // A track shorter than twice the fade plays whole
    let r = rig(&[("a", RATE, 500), ("b", RATE, 500)]);
    r.gate.allow(1000);
    r.zone.set_crossfade_ms(30);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    r.zone.wait_stopped().await;
    assert_eq!(r.rec.samples.lock().unwrap().len(), 1000);
}

/// Let blocks through one at a time until the fade from a into b has begun (a's block 600..700
/// is written), then hold the engine there.
async fn into_the_fade(r: &Rig) {
    loop {
        r.gate.blocked().await;
        if r.rec.samples.lock().unwrap().len() >= 700 {
            return;
        }
        r.gate.allow(1);
        wait_for(|| *r.gate.permits.lock().unwrap() == 0).await;
    }
}

#[tokio::test]
async fn crossfade_gives_way_to_play_next_during_the_fade() {
    let mut r = rig(&[("a", RATE, 1000), ("b", RATE, 1000), ("c", RATE, 1000)]);
    r.zone.set_crossfade_ms(30);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    into_the_fade(&r).await;
    // "Play next" while a fades into b: c comes first, b after it
    r.zone.enqueue(vec![QueueItem::new("c")], Insert::Next);
    r.gate.allow(1000);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        [
            "start:a:false",
            "end:a",
            "start:c:true",
            "end:c",
            "start:b:false",
            "end:b",
            "stopped"
        ]
    );
}

#[tokio::test]
async fn crossfade_stops_when_its_track_is_removed_during_the_fade() {
    let mut r = rig(&[("a", RATE, 1000), ("b", RATE, 1000), ("c", RATE, 1000)]);
    r.zone.set_crossfade_ms(30);
    let ids = r.zone.enqueue(
        vec![QueueItem::new("a"), QueueItem::new("b"), QueueItem::new("c")],
        Insert::End,
    );
    r.zone.play();
    into_the_fade(&r).await;
    assert!(r.zone.remove(ids[1]));
    r.gate.allow(1000);
    r.zone.wait_stopped().await;
    assert_eq!(
        kinds(&drain_events(&mut r.events)),
        ["start:a:false", "end:a", "start:c:true", "end:c", "stopped"]
    );
    assert_eq!(r.zone.queue().len(), 2);
}

#[tokio::test]
async fn now_playing_says_what_is_arriving() {
    let r = rig(&[("tidal:track:7", RATE, 1000)]);
    r.zone.enqueue(vec![QueueItem::new("tidal:track:7")], Insert::End);
    r.zone.play();
    wait_for(|| r.zone.state().now_playing.is_some()).await;
    let f = r.zone.state().now_playing.unwrap().format.expect("format");
    assert_eq!(f.codec, "FLAC");
    assert!(f.lossless);
    assert_eq!(
        f.service, "tidal",
        "the engine fills the service from the queue item"
    );
    r.gate.allow(1000);
    r.zone.wait_stopped().await;
}

/// What is streaming follows each track: a gapless join and a crossfade into another codec
/// both show the new track's format (worked out once when the track starts).
#[tokio::test]
async fn the_stream_format_follows_each_track() {
    for crossfade in [0, 30] {
        let r = rig(&[("a", RATE, 1000), ("b.mp3", RATE, 3000)]);
        r.zone.set_crossfade_ms(crossfade);
        r.zone
            .enqueue(vec![QueueItem::new("a"), QueueItem::new("b.mp3")], Insert::End);
        // all of a and a few blocks of b, then the engine waits on the gate
        r.gate.allow(16);
        r.zone.play();
        wait_for(|| r.zone.state().now_playing.is_some_and(|n| n.item.uri == "b.mp3")).await;
        let np = r.zone.state().now_playing.unwrap();
        let f = np.format.expect("a format for b");
        assert_eq!(f.codec, "MP3", "crossfade {crossfade} ms");
        assert!(!f.lossless);
        assert_eq!(f.bitrate_kbps, Some(320));
        assert_eq!(f.service, "library", "filled from a uri without a scheme");
        r.gate.allow(1000);
        r.zone.wait_stopped().await;
    }
}

/// An output with a bug: `open` panics the first time. Later opens use the runtime the way the
/// BluOS and MPD outputs do (`spawn_blocking`), which needs the zone's thread inside it.
struct PanicsOnce {
    inner: RecOutput,
    panicked: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl Output for PanicsOnce {
    fn id(&self) -> OutputId {
        self.inner.id()
    }
    fn name(&self) -> String {
        self.inner.name()
    }
    fn caps(&self) -> &OutputCaps {
        self.inner.caps()
    }
    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if !self.panicked.swap(true, Ordering::SeqCst) {
            panic!("a bug in the output");
        }
        let answer = tokio::task::spawn_blocking(|| 42).await.expect("blocking call");
        assert_eq!(answer, 42);
        self.inner.open(fmt).await
    }
    async fn set_volume(&self, _v: Volume) -> Result<(), SinkError> {
        Ok(())
    }
}

#[tokio::test]
async fn a_panic_stops_the_zone_with_an_error_and_the_zone_plays_again() {
    let gate = Arc::new(Gate::default());
    gate.allow(1000);
    let factory = Factory {
        tracks: [("a".to_string(), (AudioFormat::pcm(RATE, 16, 1), 100))]
            .into_iter()
            .collect(),
        gate: gate.clone(),
        hang: Arc::new(Gate::default()),
    };
    let rec = Arc::new(Recorder::default());
    let output = Arc::new(PanicsOnce {
        inner: RecOutput {
            caps: OutputCaps {
                pcm_rates: vec![RATE],
                ..OutputCaps::unlimited()
            },
            rec: rec.clone(),
        },
        panicked: Default::default(),
    });
    let zone = Zone::new(ZoneId::new("p"), "Panics", output, Arc::new(factory));
    let mut events = zone.subscribe();
    zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    zone.play();
    wait_for(|| zone.state().error.is_some()).await;
    let st = zone.state();
    assert_eq!(st.transport, Transport::Stopped);
    let err = st.error.unwrap();
    assert!(err.contains("a bug in the output"), "{err}");
    assert!(
        kinds(&drain_events(&mut events))
            .iter()
            .any(|k| k.starts_with("error:") && k.contains("a bug in the output")),
        "the error went out as an event"
    );
    // The thread is alive: the next play works, from the top, through the runtime.
    zone.play();
    let st = zone.wait_stopped().await;
    assert_eq!(st.error, None, "{st:?}");
    assert_eq!(*rec.samples.lock().unwrap(), frames(0, 100));
    // The queue is usable from outside after the panic (its lock is not poisoned).
    zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    wait_for(|| zone.state().queue.len() == 2).await;
}

/// The device goes away mid-track (a phone's USB DAC unplugged): the zone pauses where the listener
/// was, with the reason, writes nothing anywhere else, and Play opens the output again from there.
#[tokio::test]
async fn a_device_gone_mid_track_pauses_the_zone() {
    let mut r = rig(&[("a", RATE, 1000)]);
    *r.rec.gone_after.lock().unwrap() = Some(300);
    r.zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    r.zone.play();
    r.gate.allow(4);
    let mut w = r.zone.watch();
    w.wait_for(|s| s.transport == Transport::Paused).await.unwrap();
    let st = r.zone.state();
    assert_eq!(st.error.as_deref(), Some("FiiO KA17 was unplugged: paused"));
    assert_eq!(
        st.now_playing.as_ref().unwrap().position_ms,
        30,
        "where the listener was"
    );
    assert_eq!(*r.rec.opens.lock().unwrap(), 1, "no other device opened");
    assert_eq!(r.rec.samples.lock().unwrap().len(), 300);
    // Plugged in again; Play carries on from there on a fresh sink
    *r.rec.gone_after.lock().unwrap() = None;
    r.zone.play();
    w.wait_for(|s| s.transport == Transport::Playing).await.unwrap();
    assert_eq!(r.zone.state().error, None, "playing again: the reason is gone");
    assert_eq!(*r.rec.opens.lock().unwrap(), 2);
    r.gate.allow(1000);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    assert_eq!(
        *r.rec.samples.lock().unwrap(),
        frames(0, 1000),
        "nothing lost, nothing twice"
    );
    let k = kinds(&drain_events(&mut r.events));
    assert!(k.contains(&"pause:30".to_string()), "{k:?}");
    assert!(k.iter().any(|e| e.starts_with("error")), "{k:?}");
}

/// The device goes away while the old song plays out before a format change: the next song starts
/// paused, and nothing opens on another device (the phone's speaker); Play opens it on the DAC.
#[tokio::test]
async fn a_device_gone_at_a_song_boundary_pauses_the_next_song() {
    let mut r = rig(&[("a", RATE, 200), ("b", 2 * RATE, 300)]);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    r.rec.drain_gone.store(true, Ordering::SeqCst);
    r.gate.allow(3);
    let mut w = r.zone.watch();
    tokio::time::timeout(
        Duration::from_secs(5),
        w.wait_for(|s| s.transport == Transport::Paused),
    )
    .await
    .expect("the zone pauses (it must not play b elsewhere)")
    .unwrap();
    let st = r.zone.state();
    assert_eq!(st.error.as_deref(), Some("FiiO KA17 was unplugged: paused"));
    assert_eq!(
        st.now_playing.as_ref().unwrap().item.uri,
        "b",
        "paused at the next song"
    );
    assert_eq!(*r.rec.opens.lock().unwrap(), 1, "nothing opened for b");
    r.rec.drain_gone.store(false, Ordering::SeqCst);
    r.zone.play();
    w.wait_for(|s| s.transport == Transport::Playing).await.unwrap();
    r.gate.allow(100);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    assert_eq!(*r.rec.opens.lock().unwrap(), 2, "b on a fresh sink");
    let mut want = frames(0, 200);
    want.extend(frames(0, 300));
    assert_eq!(
        *r.rec.samples.lock().unwrap(),
        want,
        "nothing lost, nothing twice"
    );
    drop(drain_events(&mut r.events));
}

/// Codex M4 (round 3): the device is not there when the next song's sink opens (the route went
/// between the drain and the open): the same pause as a drain that finds it gone, never a stopped
/// zone, and nothing opens elsewhere.
#[tokio::test]
async fn a_device_gone_while_the_next_song_opens_pauses_not_stops() {
    let mut r = rig(&[("a", RATE, 200), ("b", 2 * RATE, 300)]);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    *r.rec.open_gone_from.lock().unwrap() = Some(2);
    r.zone.play();
    r.gate.allow(3);
    let mut w = r.zone.watch();
    tokio::time::timeout(
        Duration::from_secs(5),
        w.wait_for(|s| s.transport == Transport::Paused),
    )
    .await
    .expect("the zone pauses, it does not stop")
    .unwrap();
    let st = r.zone.state();
    assert_eq!(
        st.error.as_deref(),
        Some("FiiO KA17 is not where Android plays now: paused")
    );
    assert_eq!(st.now_playing.as_ref().unwrap().item.uri, "b");
    // Back: Play opens b there
    *r.rec.open_gone_from.lock().unwrap() = None;
    r.zone.play();
    w.wait_for(|s| s.transport == Transport::Playing).await.unwrap();
    r.gate.allow(100);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    let mut want = frames(0, 200);
    want.extend(frames(0, 300));
    assert_eq!(
        *r.rec.samples.lock().unwrap(),
        want,
        "nothing lost, nothing twice"
    );
    drop(drain_events(&mut r.events));
}

/// Unplugged during a pause the device held: Play pauses there with the reason; it doesn't open
/// the output again (that would be the phone's speaker).
#[tokio::test]
async fn a_device_gone_during_a_pause_stays_paused_on_play() {
    let mut r = rig(&[("a", RATE, 1000)]);
    r.rec.device_pause.store(true, Ordering::SeqCst);
    r.zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    r.zone.play();
    r.gate.allow(3);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 300).await;
    r.gate.blocked().await;
    r.zone.pause();
    r.gate.allow(1);
    let mut w = r.zone.watch();
    w.wait_for(|s| s.transport == Transport::Paused).await.unwrap();
    r.rec.resume_gone.store(true, Ordering::SeqCst);
    r.zone.play();
    wait_for(|| r.zone.state().error.is_some()).await;
    let st = r.zone.state();
    assert_eq!(st.transport, Transport::Paused);
    assert_eq!(st.error.as_deref(), Some("FiiO KA17 was unplugged: paused"));
    assert_eq!(*r.rec.opens.lock().unwrap(), 1, "nothing opened again");
    drop(drain_events(&mut r.events));
}

/// Claude L1 (round 5): the listener pauses just after the device went (the sink's flush says so):
/// the zone pauses with the reason, it doesn't stop, and the sink hears that the zone heard it.
#[tokio::test]
async fn a_pause_that_finds_the_device_gone_pauses_with_the_reason() {
    let mut r = rig(&[("a", RATE, 1000)]);
    r.zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    r.zone.play();
    r.gate.allow(3);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 300).await;
    r.gate.blocked().await;
    r.rec.flush_gone.store(true, Ordering::SeqCst);
    r.zone.pause();
    r.gate.allow(1);
    wait_for(|| r.zone.state().error.is_some()).await;
    let st = r.zone.state();
    assert_eq!(st.transport, Transport::Paused, "{st:?}");
    assert_eq!(st.error.as_deref(), Some("FiiO KA17 was unplugged: paused"));
    assert_eq!(*r.rec.gone_reported.lock().unwrap(), 1);
    drop(drain_events(&mut r.events));
}

/// A sink that may not go on as it is (a phone's DAC paused in bit-perfect whose volume went
/// down since): the next write goes to a fresh sink, from where it was, nothing lost.
#[tokio::test]
async fn a_sink_that_may_not_go_on_is_replaced_before_the_write() {
    let mut r = rig(&[("a", RATE, 500)]);
    r.zone.enqueue(vec![QueueItem::new("a")], Insert::End);
    r.zone.play();
    r.gate.allow(2);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 200).await;
    r.gate.blocked().await;
    let drains = *r.rec.drains.lock().unwrap();
    r.rec.reopen_now.store(true, Ordering::SeqCst);
    r.gate.allow(1);
    wait_for(|| *r.rec.opens.lock().unwrap() == 2).await;
    assert_eq!(
        *r.rec.drains.lock().unwrap(),
        drains,
        "what may not play is discarded, not played out"
    );
    assert!(*r.rec.flushes.lock().unwrap() >= 1);
    r.gate.allow(100);
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    assert_eq!(
        *r.rec.opens.lock().unwrap(),
        2,
        "a fresh sink before the next write"
    );
    assert_eq!(
        *r.rec.samples.lock().unwrap(),
        frames(0, 500),
        "nothing lost, nothing twice"
    );
    drop(drain_events(&mut r.events));
}

/// The output's settings change while it plays (a phone's DAC from shared to bit-perfect): the
/// next song opens afresh even in the same format, so the change applies there.
#[tokio::test]
async fn a_settings_change_applies_at_the_next_song() {
    let mut r = rig(&[("a", RATE, 200), ("b", RATE, 200), ("c", RATE, 200)]);
    r.zone.enqueue(
        vec![QueueItem::new("a"), QueueItem::new("b"), QueueItem::new("c")],
        Insert::End,
    );
    r.zone.play();
    r.gate.allow(1);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 100).await;
    r.rec.reopen.store(true, Ordering::SeqCst);
    r.gate.allow(2);
    wait_for(|| r.rec.samples.lock().unwrap().len() == 300).await;
    r.gate.blocked().await;
    r.rec.reopen.store(false, Ordering::SeqCst);
    r.gate.allow(100);
    r.zone.wait_stopped().await;
    assert_eq!(
        *r.rec.opens.lock().unwrap(),
        2,
        "a, then b afresh; c joins b's sink"
    );
    let k = kinds(&drain_events(&mut r.events));
    assert!(
        k.contains(&"start:b:false".to_string()),
        "b is not gapless: {k:?}"
    );
    assert!(k.contains(&"start:c:true".to_string()), "c is: {k:?}");
}

/// The output's settings changed before a crossfade (a phone's DAC switched mode): no crossfade
/// carries the next song on the old sink; it opens afresh, so the change applies there.
#[tokio::test]
async fn a_settings_change_skips_the_crossfade_and_reopens() {
    let mut r = rig(&[("a", RATE, 1000), ("b", RATE, 1000)]);
    r.rec.reopen.store(true, Ordering::SeqCst);
    r.gate.allow(1000);
    r.zone.set_crossfade_ms(30);
    r.zone
        .enqueue(vec![QueueItem::new("a"), QueueItem::new("b")], Insert::End);
    r.zone.play();
    let st = r.zone.wait_stopped().await;
    assert_eq!(st.error, None);
    let mut want = frames(0, 1000);
    want.extend(frames(0, 1000));
    assert_eq!(
        *r.rec.samples.lock().unwrap(),
        want,
        "no mix: a whole, then b whole"
    );
    assert_eq!(*r.rec.opens.lock().unwrap(), 2, "b on a fresh sink");
    drop(drain_events(&mut r.events));
}

/// A renderer whose set can be held open, and whose read after it fails: the shape of a device
/// event arriving while a set waits on the device (#265 with #256's one volume).
struct HeldSet {
    inner: NullOutput,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    reads_fail: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl Output for HeldSet {
    fn id(&self) -> OutputId {
        self.inner.id()
    }
    fn name(&self) -> String {
        self.inner.name()
    }
    fn caps(&self) -> &OutputCaps {
        self.inner.caps()
    }
    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        self.inner.open(fmt).await
    }
    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        self.entered.notify_one();
        self.release.notified().await;
        self.inner.set_volume(v).await
    }
    async fn volume(&self) -> Result<Option<pm_model::ZoneVolume>, SinkError> {
        if self.reads_fail.load(Ordering::SeqCst) {
            return Err(SinkError::Device("no answer".into()));
        }
        self.inner.volume().await
    }
}

fn pct(p: u8) -> pm_model::ZoneVolume {
    pm_model::ZoneVolume {
        level: pm_model::VolumeLevel::Percent(p),
        muted: false,
    }
}

#[tokio::test]
async fn a_level_the_device_announces_during_a_set_stands() {
    let zones = Zones::new(Arc::new(Factory {
        tracks: BTreeMap::new(),
        gate: Arc::new(Gate::default()),
        hang: Arc::new(Gate::default()),
    }));
    let out = Arc::new(HeldSet {
        inner: NullOutput::with_volume(pm_model::VolumeKind::Percent),
        entered: Default::default(),
        release: Default::default(),
        reads_fail: Default::default(),
    });
    let zone = zones.create("Linn", out.clone());
    zone.announce_volume(pct(10));
    wait_for(|| zone.state().volume == Some(pct(10))).await;
    // A set waits on the device; meanwhile the device announces the level it took, and the
    // read after the set fails: the announced level stays, not the one before it
    out.reads_fail.store(true, Ordering::SeqCst);
    let z = zone.clone();
    let set = tokio::spawn(async move { z.set_volume(Volume::Percent(20)).await });
    out.entered.notified().await;
    zone.announce_volume(pct(20));
    out.release.notify_one();
    set.await.expect("the set ran").expect("the set worked");
    zone.wait_acked().await;
    assert_eq!(
        zone.state().volume,
        Some(pct(20)),
        "the device's own announcement stands"
    );
    // A poll that reads the announced level sends nothing new; one that reads another sends it
    out.reads_fail.store(false, Ordering::SeqCst);
    out.inner.pretend_level(pct(20));
    assert_eq!(zone.refresh_volume().await, Some(pct(20)));
    zone.wait_acked().await;
    assert_eq!(zone.state().volume, Some(pct(20)));
    out.inner.pretend_level(pct(35));
    assert_eq!(zone.refresh_volume().await, Some(pct(35)));
    zone.wait_acked().await;
    assert_eq!(
        zone.state().volume,
        Some(pct(35)),
        "a change read after the announcement shows"
    );
}
