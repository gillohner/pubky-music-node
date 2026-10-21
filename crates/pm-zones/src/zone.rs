//! A zone: one output (groups come with clock sync, §6.5), a queue and a
//! transport, driven by a dedicated audio thread (the private `engine` module).
//!
//! [`Zone`] is the handle the API and the CLI hold: it mutates the queue
//! directly, sends transport commands to the thread, and exposes the
//! latest [`ZoneState`] through a `watch` channel and [`ZoneEvent`]s through
//! a `broadcast` channel.
//!
//! Gapless rule: when a track ends and the next one negotiates the same
//! output format, the sink stays open and the next track's first block
//! follows the last one with nothing in between. A format change drains
//! the sink (the tail plays out) and reopens it.
//!
//! Pause and seek: the [`AudioSink`](crate::AudioSink) trait has no pause,
//! so pausing flushes the sink's buffer and remembers the audible position;
//! resuming seeks the source there. Nothing is resampled or faded.

use crate::engine::{self, Command};
use crate::queue::{Insert, Queue, QueueItem, Repeat};
use crate::sink::{Output, OutputId, SinkError};
use crate::source::SourceFactory;
use pm_model::{AudioFormat, SignalPath, Volume};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use tokio::sync::{broadcast, watch};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ZoneId(pub String);

impl ZoneId {
    pub fn new(s: impl Into<String>) -> Self {
        ZoneId(s.into())
    }
}

impl fmt::Display for ZoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NowPlaying {
    pub item: QueueItem,
    pub index: usize,
    /// Audible position: frames handed to the sink minus what it still buffers.
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    pub source_format: AudioFormat,
    pub output_format: AudioFormat,
    pub signal_path: SignalPath,
    /// Grouped outputs and their drift; empty for a single output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<crate::sink::MemberStatus>,
    /// The latest in-stream title (internet radio), when the stream sends one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneState {
    pub id: ZoneId,
    pub name: String,
    pub output: OutputId,
    pub transport: Transport,
    pub now_playing: Option<NowPlaying>,
    pub queue: Vec<QueueItem>,
    pub queue_index: Option<usize>,
    pub queue_version: u64,
    pub repeat: Repeat,
    /// Keep playing similar music after the last item (implemented by the node).
    #[serde(default)]
    pub radio: bool,
    /// Last error that stopped playback; cleared on the next successful start.
    pub error: Option<String>,
    /// Sequence number of the last command the engine handled (see [`Zone::wait_stopped`]).
    pub acked: u64,
    /// How closely the zone's speakers stay together.
    #[serde(default)]
    pub sync: crate::SyncClass,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ZoneEvent {
    TrackStarted {
        item: QueueItem,
        index: usize,
        source_format: AudioFormat,
        output_format: AudioFormat,
        signal_path: SignalPath,
        /// The sink stayed open from the previous track (gapless join).
        gapless: bool,
    },
    TrackEnded {
        item: QueueItem,
        /// Codec verification (FLAC MD5) when the codec provides one.
        verify_ok: Option<bool>,
    },
    /// An item could not be opened or planned; the zone moved on.
    Skipped {
        item: QueueItem,
        reason: String,
    },
    Paused {
        position_ms: u64,
    },
    Resumed,
    Seeked {
        position_ms: u64,
    },
    Stopped,
    QueueChanged,
    /// The stream announced a new title (internet radio).
    StreamTitle {
        title: String,
    },
    /// Playback stopped because of this.
    Error(String),
}

/// Handle to a running zone. Cloneable through `Arc`; dropping the last
/// handle stops the audio thread.
pub struct Zone {
    id: ZoneId,
    name: String,
    output: Arc<dyn Output>,
    queue: Arc<Mutex<Queue>>,
    cmds: mpsc::Sender<(u64, Command)>,
    seq: AtomicU64,
    state: watch::Receiver<ZoneState>,
    events: broadcast::Sender<ZoneEvent>,
}

impl Zone {
    /// Create a zone on `output` and start its audio thread.
    pub fn new(id: ZoneId, name: &str, output: Arc<dyn Output>, factory: Arc<dyn SourceFactory>) -> Self {
        let queue = Arc::new(Mutex::new(Queue::default()));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (events, _) = broadcast::channel(256);
        let initial = ZoneState {
            id: id.clone(),
            name: name.to_string(),
            output: output.id(),
            transport: Transport::Stopped,
            now_playing: None,
            queue: Vec::new(),
            queue_index: None,
            queue_version: 0,
            repeat: Repeat::Off,
            radio: false,
            error: None,
            acked: 0,
            sync: output.sync(),
        };
        let (state_tx, state) = watch::channel(initial);
        engine::spawn(
            id.clone(),
            output.clone(),
            factory,
            queue.clone(),
            cmd_rx,
            state_tx,
            events.clone(),
        );
        Self {
            id,
            name: name.to_string(),
            output,
            queue,
            cmds: cmd_tx,
            seq: AtomicU64::new(0),
            state,
            events,
        }
    }

    pub fn id(&self) -> &ZoneId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn output(&self) -> &Arc<dyn Output> {
        &self.output
    }

    /// The latest published state.
    pub fn state(&self) -> ZoneState {
        self.state.borrow().clone()
    }

    /// A receiver that wakes on every state change.
    pub fn watch(&self) -> watch::Receiver<ZoneState> {
        self.state.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ZoneEvent> {
        self.events.subscribe()
    }

    fn send(&self, cmd: Command) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        // A dead engine (panicked audio thread) shows up as a stale state; nothing to do here.
        let _ = self.cmds.send((seq, cmd));
        seq
    }

    fn with_queue<T>(&self, f: impl FnOnce(&mut Queue) -> T) -> T {
        let r = f(&mut self.queue.lock().expect("queue poisoned"));
        self.send(Command::QueueChanged);
        r
    }

    // ---- queue ----

    pub fn enqueue(&self, items: Vec<QueueItem>, at: Insert) -> Vec<u64> {
        self.with_queue(|q| q.add(items, at))
    }

    pub fn remove(&self, id: u64) -> bool {
        self.with_queue(|q| q.remove(id))
    }

    pub fn move_item(&self, id: u64, to_index: usize) -> bool {
        self.with_queue(|q| q.move_item(id, to_index))
    }

    pub fn clear(&self) {
        self.with_queue(|q| q.clear())
    }

    /// Shuffle the upcoming items; the current one keeps playing.
    pub fn shuffle_upcoming(&self) -> bool {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5eed);
        self.with_queue(|q| q.shuffle_upcoming(seed))
    }

    pub fn set_repeat(&self, r: Repeat) {
        self.with_queue(|q| q.set_repeat(r))
    }

    pub fn set_radio(&self, on: bool) {
        self.with_queue(|q| q.set_radio(on))
    }

    pub fn radio(&self) -> bool {
        self.queue.lock().expect("queue poisoned").radio()
    }

    pub fn queue(&self) -> Vec<QueueItem> {
        self.queue.lock().expect("queue poisoned").items().to_vec()
    }

    // ---- transport ----

    /// Play: resume when paused, else start the current item (or the first).
    pub fn play(&self) {
        self.send(Command::Play(None));
    }

    /// Start the queue item at `index` from its beginning.
    pub fn play_index(&self, index: usize) {
        self.send(Command::Play(Some(index)));
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    pub fn toggle_pause(&self) {
        self.send(Command::TogglePause);
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    pub fn next(&self) {
        self.send(Command::Next);
    }

    /// Previous track, or restart the current one when more than 3 s in.
    pub fn previous(&self) {
        self.send(Command::Previous);
    }

    pub fn seek_ms(&self, ms: u64) {
        self.send(Command::Seek(ms));
    }

    /// Hardware volume on the output; the audio path is untouched.
    pub async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        self.output.set_volume(v).await
    }

    /// Wait until every command sent so far was handled, whatever the
    /// transport ended up as; returns the state published after that.
    pub async fn wait_acked(&self) -> ZoneState {
        let want = self.seq.load(Ordering::SeqCst);
        let mut rx = self.state.clone();
        loop {
            {
                let s = rx.borrow_and_update();
                if s.acked >= want {
                    return s.clone();
                }
            }
            if rx.changed().await.is_err() {
                return self.state();
            }
        }
    }

    /// Wait until every command sent so far was handled and the zone is
    /// stopped (end of queue, explicit stop, or error).
    pub async fn wait_stopped(&self) -> ZoneState {
        let want = self.seq.load(Ordering::SeqCst);
        let mut rx = self.state.clone();
        loop {
            {
                let s = rx.borrow_and_update();
                if s.acked >= want && s.transport == Transport::Stopped {
                    return s.clone();
                }
            }
            if rx.changed().await.is_err() {
                return self.state();
            }
        }
    }
}

impl Drop for Zone {
    fn drop(&mut self) {
        let _ = self.cmds.send((u64::MAX, Command::Shutdown));
    }
}

impl fmt::Debug for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Zone")
            .field("id", &self.id)
            .field("output", &self.output.id())
            .finish()
    }
}

/// All zones of a node.
pub struct Zones {
    factory: Arc<dyn SourceFactory>,
    inner: RwLock<BTreeMap<ZoneId, Arc<Zone>>>,
}

impl Zones {
    pub fn new(factory: Arc<dyn SourceFactory>) -> Self {
        Self {
            factory,
            inner: RwLock::new(BTreeMap::new()),
        }
    }

    /// Create a zone named `name` on `output`. The id is derived from the
    /// name (`Living Room` → `living-room`), made unique with a suffix.
    pub fn create(&self, name: &str, output: Arc<dyn Output>) -> Arc<Zone> {
        let mut map = self.inner.write().expect("zones poisoned");
        let base = slug(name);
        let mut id = ZoneId(base.clone());
        let mut n = 2;
        while map.contains_key(&id) {
            id = ZoneId(format!("{base}-{n}"));
            n += 1;
        }
        let zone = Arc::new(Zone::new(id.clone(), name, output, self.factory.clone()));
        map.insert(id, zone.clone());
        zone
    }

    pub fn get(&self, id: &ZoneId) -> Option<Arc<Zone>> {
        self.inner.read().expect("zones poisoned").get(id).cloned()
    }

    pub fn for_output(&self, output: &OutputId) -> Option<Arc<Zone>> {
        self.inner
            .read()
            .expect("zones poisoned")
            .values()
            .find(|z| &z.output().id() == output)
            .cloned()
    }

    pub fn remove(&self, id: &ZoneId) -> Option<Arc<Zone>> {
        self.inner.write().expect("zones poisoned").remove(id)
    }

    pub fn list(&self) -> Vec<ZoneState> {
        self.inner
            .read()
            .expect("zones poisoned")
            .values()
            .map(|z| z.state())
            .collect()
    }
}

fn slug(name: &str) -> String {
    let mut s = String::new();
    let mut dash = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            s.push('-');
            dash = true;
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() {
        "zone".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Living Room"), "living-room");
        assert_eq!(slug("  Küche / DAC #2 "), "k-che-dac-2");
        assert_eq!(slug("***"), "zone");
    }
}
