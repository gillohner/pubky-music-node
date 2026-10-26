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
}

impl FrameSource for PatternSource {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn duration_ms(&self) -> Option<u64> {
        Some(self.total * 1000 / self.fmt.rate_hz as u64)
    }

    fn next_block(&mut self) -> Result<Option<AudioBlock>, SourceError> {
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
}

impl SourceFactory for Factory {
    fn open(&self, item: &QueueItem) -> Result<Box<dyn FrameSource>, SourceError> {
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
        *self.rec.opens.lock().unwrap() += 1;
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
        Ok(())
    }
    fn drain(&mut self) -> Result<(), SinkError> {
        *self.rec.drains.lock().unwrap() += 1;
        Ok(())
    }
    fn frames_written(&self) -> u64 {
        self.written
    }
}

struct Rig {
    zone: Zone,
    gate: Arc<Gate>,
    rec: Arc<Recorder>,
    events: broadcast::Receiver<ZoneEvent>,
}

fn rig(tracks: &[(&str, u32, u64)]) -> Rig {
    let gate = Arc::new(Gate::default());
    let factory = Factory {
        tracks: tracks
            .iter()
            .map(|(u, rate, n)| (u.to_string(), (AudioFormat::pcm(*rate, 16, 1), *n)))
            .collect(),
        gate: gate.clone(),
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
            ZoneEvent::Error(e) => format!("error:{e}"),
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
