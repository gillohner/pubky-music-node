//! `/v1/events`: the WebSocket every controller listens on.
//!
//! Every event carries a sequence number `seq`, counted per node run. The
//! first message on a socket is a `snapshot` with the current `seq`, the
//! node's `boot` id (new on every start) and `library_rev` (bumped on every
//! library change). A controller that reconnects compares them with what it
//! had: a new `boot` or `library_rev` means "reload the library views", and
//! the zones in the snapshot replace its own. A gap in `seq` means it missed
//! something; it sends `{"type":"resync"}` and gets a fresh snapshot.
//!
//! A listener sees only the zones open to listeners: the snapshot leaves the others out, and an
//! event about one of them goes out as `{"seq":n,"type":"hidden"}`, so `seq` has no gaps.
//!
//! Controllers may also send `{"type":"ping","id":n}`; the node answers
//! `{"type":"pong","id":n}` at once (no `seq`), which gives the round-trip
//! time. A controller that pings is dropped after [`IDLE_TIMEOUT`] without
//! a message, so a half-open socket (a laptop that went to sleep) does not
//! linger. The node also sends a WebSocket ping every [`SERVER_PING`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::Extension;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::accounts::{may_use_zone, Caller};
use crate::state::NodeState;
use crate::types::ApiEvent;
use pm_zones::ZoneState;

/// How often the node pings each socket.
pub const SERVER_PING: Duration = Duration::from_secs(20);
/// How often a listener's socket checks its token is still good, at most (when events come).
const RECHECK: Duration = Duration::from_secs(5);
/// A pinging controller that is silent this long is gone.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// One event as it travels: its number and the event.
#[derive(Debug, Clone)]
pub struct Sequenced {
    pub seq: u64,
    pub event: ApiEvent,
}

/// The node's event fan-out. `send` numbers events in the order controllers
/// receive them.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Inner>,
}

struct Inner {
    tx: broadcast::Sender<Sequenced>,
    /// The last number handed out. Held while sending, so numbers and
    /// delivery order agree.
    seq: Mutex<u64>,
    library_rev: AtomicU64,
    boot: String,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(Inner {
                tx,
                seq: Mutex::new(0),
                library_rev: AtomicU64::new(0),
                boot: format!("{:016x}", rand::random::<u64>()),
            }),
        }
    }

    /// Number and broadcast an event. Returns its `seq`.
    pub fn send(&self, event: ApiEvent) -> u64 {
        let mut seq = self.inner.seq.lock().expect("event seq poisoned");
        *seq += 1;
        if matches!(event, ApiEvent::LibraryChanged) {
            self.inner.library_rev.fetch_add(1, Ordering::Relaxed);
        }
        // No receivers is fine: nobody is listening.
        let _ = self.inner.tx.send(Sequenced { seq: *seq, event });
        *seq
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Sequenced> {
        self.inner.tx.subscribe()
    }

    /// A receiver plus the `seq` of the last event it will *not* see.
    pub fn subscribe_at(&self) -> (broadcast::Receiver<Sequenced>, u64) {
        let seq = self.inner.seq.lock().expect("event seq poisoned");
        (self.inner.tx.subscribe(), *seq)
    }

    /// This node run's id.
    pub fn boot(&self) -> &str {
        &self.inner.boot
    }

    pub fn library_rev(&self) -> u64 {
        self.inner.library_rev.load(Ordering::Relaxed)
    }
}

/// The first message on a socket (and the answer to `resync`). Reads as
/// `ApiEvent::Snapshot` for clients that only know `zones`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotMsg {
    #[serde(rename = "type")]
    pub kind: String,
    pub seq: u64,
    pub boot: String,
    pub library_rev: u64,
    pub zones: Vec<ZoneState>,
}

#[derive(Serialize)]
struct EventMsg<'a> {
    seq: u64,
    #[serde(flatten)]
    event: &'a ApiEvent,
}

/// What a controller may send.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Ping {
        #[serde(default)]
        id: serde_json::Value,
    },
    Resync,
}

pub(crate) async fn events_ws(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let token = crate::bearer(&headers, &q);
    ws.on_upgrade(move |socket| event_stream(socket, s, c, token))
}

fn snapshot(s: &NodeState, c: &Caller, seq: u64) -> SnapshotMsg {
    SnapshotMsg {
        kind: "snapshot".into(),
        seq,
        boot: s.events.boot().to_string(),
        library_rev: s.events.library_rev(),
        zones: visible(s, c, s.zones.list()),
    }
}

/// The zones of `zones` this caller may see.
pub(crate) fn visible(s: &NodeState, c: &Caller, mut zones: Vec<ZoneState>) -> Vec<ZoneState> {
    zones.retain(|z| may_use_zone(s, c, &z.id.0));
    zones
}

/// The event as this caller may see it: `None` when it is about a zone they may not see.
fn for_caller(s: &NodeState, c: &Caller, ev: &ApiEvent) -> Option<ApiEvent> {
    match ev {
        ApiEvent::Snapshot { zones } => Some(ApiEvent::Snapshot {
            zones: visible(s, c, zones.clone()),
        }),
        ApiEvent::Zone { state } => may_use_zone(s, c, &state.id.0).then(|| ev.clone()),
        ApiEvent::ZoneEvent { zone, .. } | ApiEvent::ZoneRemoved { zone } => {
            may_use_zone(s, c, &zone.0).then(|| ev.clone())
        }
        ApiEvent::LibraryChanged => Some(ev.clone()),
        // The owner's post on the owner's homeserver
        ApiEvent::ListeningStopped { .. } => (c.role == crate::accounts::Role::Owner).then(|| ev.clone()),
    }
}

async fn event_stream(socket: WebSocket, s: Arc<NodeState>, c: Caller, token: Option<String>) {
    let (mut tx, mut rx) = socket.split();
    let (mut events, seq) = s.events.subscribe_at();
    // A listener sees file names, not where the owner keeps files (as in every JSON answer)
    let hide = c.role != crate::accounts::Role::Owner;
    if send_for(&mut tx, hide, &snapshot(&s, &c, seq)).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(SERVER_PING);
    ping.tick().await;
    // Only controllers that ping are held to the idle timeout; older ones
    // (and pm-client) never send anything.
    let mut pings = false;
    let mut last_heard = tokio::time::Instant::now();
    // The owner came in with the API token: when it is replaced, this socket ends with it
    let with_token = c.role == crate::accounts::Role::Owner && s.token().is_some();
    let mut token_changed = s.token_changed.subscribe();
    // A listener signed out or lost music access: the socket goes with the token (checked on
    // each server ping, and before an event at most every few seconds)
    let mut checked = tokio::time::Instant::now();
    let still_valid = |at: &mut tokio::time::Instant, now: bool| {
        if c.role == crate::accounts::Role::Owner || !(now || at.elapsed() >= RECHECK) {
            return true;
        }
        *at = tokio::time::Instant::now();
        crate::accounts::caller(&s, token.as_deref()).is_some_and(|now| now == c)
    };
    loop {
        let idle = tokio::time::sleep_until(last_heard + IDLE_TIMEOUT);
        tokio::select! {
            _ = token_changed.changed(), if with_token => {
                let _ = tx.send(Message::Close(None)).await;
                break;
            }
            ev = events.recv() => match ev {
                Ok(_) if !still_valid(&mut checked, false) => {
                    let _ = tx.send(Message::Close(None)).await;
                    break;
                }
                Ok(ev) => {
                    let sent = match for_caller(&s, &c, &ev.event) {
                        Some(event) => send_for(&mut tx, hide, &EventMsg { seq: ev.seq, event: &event }).await,
                        None => send_json(&mut tx, &serde_json::json!({"seq": ev.seq, "type": "hidden"})).await,
                    };
                    if sent.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "event stream lagged; resending snapshot");
                    let (fresh, seq) = s.events.subscribe_at();
                    events = fresh;
                    if send_for(&mut tx, hide, &snapshot(&s, &c, seq)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = rx.next() => {
                last_heard = tokio::time::Instant::now();
                match msg {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t) {
                        Ok(ClientMsg::Ping { id }) => {
                            pings = true;
                            let pong = serde_json::json!({"type": "pong", "id": id});
                            if send_json(&mut tx, &pong).await.is_err() {
                                break;
                            }
                        }
                        Ok(ClientMsg::Resync) => {
                            let (fresh, seq) = s.events.subscribe_at();
                            events = fresh;
                            if send_for(&mut tx, hide, &snapshot(&s, &c, seq)).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => {}
                    },
                    Some(Ok(Message::Ping(p))) => {
                        if tx.send(Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
            _ = ping.tick() => {
                if !still_valid(&mut checked, true) {
                    let _ = tx.send(Message::Close(None)).await;
                    break;
                }
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            _ = idle, if pings => {
                tracing::debug!("controller went quiet; closing its event stream");
                break;
            }
        }
    }
}

/// [`send_json`], with the owner's file paths hidden when `hide` (a listener).
async fn send_for<T: Serialize>(
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    hide: bool,
    v: &T,
) -> Result<(), axum::Error> {
    if !hide {
        return send_json(tx, v).await;
    }
    match serde_json::to_value(v) {
        Ok(mut j) => {
            crate::accounts::hide_file_paths(&mut j);
            send_json(tx, &j).await
        }
        Err(_) => send_json(tx, v).await,
    }
}

async fn send_json<T: Serialize>(
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    v: &T,
) -> Result<(), axum::Error> {
    // An event that does not serialize is a bug: log it and keep the socket, so the controller
    // still hears the zone's next state instead of losing its connection.
    let text = match serde_json::to_string(v) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("event not sent, it does not serialize: {e}");
            return Ok(());
        }
    };
    tx.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_numbered_in_order_and_library_changes_counted() {
        let bus = EventBus::new(16);
        let (mut rx, at) = bus.subscribe_at();
        assert_eq!(at, 0);
        assert_eq!(bus.send(ApiEvent::LibraryChanged), 1);
        assert_eq!(
            bus.send(ApiEvent::ZoneRemoved {
                zone: pm_zones::ZoneId("a".into())
            }),
            2
        );
        assert_eq!(rx.try_recv().unwrap().seq, 1);
        assert_eq!(rx.try_recv().unwrap().seq, 2);
        assert_eq!(bus.library_rev(), 1);
        let (_, at) = bus.subscribe_at();
        assert_eq!(at, 2);
    }

    #[test]
    fn wire_messages_keep_the_old_shape() {
        let ev = ApiEvent::ZoneRemoved {
            zone: pm_zones::ZoneId("k".into()),
        };
        let v = serde_json::to_value(EventMsg { seq: 7, event: &ev }).unwrap();
        assert_eq!(v["type"], "zone_removed");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["zone"], "k");
        // Old clients still read the snapshot as ApiEvent::Snapshot.
        let snap = SnapshotMsg {
            kind: "snapshot".into(),
            seq: 3,
            boot: "b".into(),
            library_rev: 1,
            zones: vec![],
        };
        let back: ApiEvent = serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert!(matches!(back, ApiEvent::Snapshot { .. }));
        let back: ApiEvent = serde_json::to_string(&v)
            .map(|t| serde_json::from_str(&t).unwrap())
            .unwrap();
        assert!(matches!(back, ApiEvent::ZoneRemoved { .. }));
    }

    /// One of each `ZoneEvent`. The match has no wildcard, so a new variant does not compile
    /// until it is listed here (and so tested below).
    fn every_zone_event() -> Vec<pm_zones::ZoneEvent> {
        use pm_model::{AudioFormat, SignalPath};
        use pm_zones::{QueueItem, ZoneEvent};
        let item = QueueItem::new("track:1");
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let all = vec![
            ZoneEvent::TrackStarted {
                item: item.clone(),
                index: 0,
                source_format: fmt,
                output_format: fmt,
                signal_path: SignalPath::new(),
                gapless: false,
            },
            ZoneEvent::TrackEnded {
                item: item.clone(),
                verify_ok: Some(true),
            },
            ZoneEvent::Skipped {
                item,
                reason: "unreadable".into(),
            },
            ZoneEvent::Paused { position_ms: 1 },
            ZoneEvent::Resumed,
            ZoneEvent::Seeked { position_ms: 2 },
            ZoneEvent::Stopped,
            ZoneEvent::QueueChanged,
            ZoneEvent::StreamTitle { title: "t".into() },
            ZoneEvent::Error {
                message: "device error: listener went away".into(),
            },
        ];
        for ev in &all {
            match ev {
                ZoneEvent::TrackStarted { .. }
                | ZoneEvent::TrackEnded { .. }
                | ZoneEvent::Skipped { .. }
                | ZoneEvent::Paused { .. }
                | ZoneEvent::Resumed
                | ZoneEvent::Seeked { .. }
                | ZoneEvent::Stopped
                | ZoneEvent::QueueChanged
                | ZoneEvent::StreamTitle { .. }
                | ZoneEvent::Error { .. } => {}
            }
        }
        all
    }

    #[test]
    fn every_zone_event_goes_out_on_the_socket_and_reads_back() {
        for event in every_zone_event() {
            let ev = ApiEvent::ZoneEvent {
                zone: pm_zones::ZoneId("z".into()),
                event: event.clone(),
            };
            let text = serde_json::to_string(&EventMsg { seq: 1, event: &ev })
                .unwrap_or_else(|e| panic!("{event:?} does not serialize: {e}"));
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["type"], "zone_event");
            assert!(v["event"]["type"].is_string(), "{text}");
            let back: ApiEvent = serde_json::from_str(&text).unwrap();
            match back {
                ApiEvent::ZoneEvent { event: b, .. } => assert_eq!(b, event),
                other => panic!("read back as {other:?}"),
            }
        }
        // The error's wire shape, which controllers read.
        let ev = ApiEvent::ZoneEvent {
            zone: pm_zones::ZoneId("z".into()),
            event: pm_zones::ZoneEvent::Error {
                message: "boom".into(),
            },
        };
        let v = serde_json::to_value(EventMsg { seq: 2, event: &ev }).unwrap();
        assert_eq!(
            v["event"],
            serde_json::json!({"type": "error", "message": "boom"})
        );
    }
}
