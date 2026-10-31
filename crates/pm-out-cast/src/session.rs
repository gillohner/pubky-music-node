//! A Cast session while it plays. After LOAD the session owns the link:
//! - it answers the device's heartbeat PINGs and sends its own, so the device keeps the sender
//!   connected and STOP and volume still reach it;
//! - it resumes after the connection drops (Wi-Fi blip, device busy): reconnect, find the session
//!   still running and join it, or launch and load again;
//! - it steps aside when someone else takes the device (another app cast to the TV): it never
//!   stops another sender's app;
//! - it reloads the stream when the receiver reports a media error;
//! - it pauses the media for the zone's pause, or stops it when the receiver plays on (or can't
//!   pause the live stream), so the device falls silent at once instead of playing out its buffer.

use crate::link::{CastLink, NS_HEARTBEAT, NS_MEDIA, NS_RECEIVER};
use crate::proto::{decode, frame, CastMessage};
use crate::{CastError, Meta};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot};

const SENDER: &str = "sender-pubky-music";
const RECEIVER_0: &str = "receiver-0";
/// How often the session pings the device.
pub const PING_EVERY: Duration = Duration::from_secs(5);
/// Reconnect attempts after the link drops, with doubling waits from 1 s.
const RESUME_TRIES: u32 = 5;
/// How long a PAUSE waits for the receiver to report PAUSED before it is stopped instead.
pub const PAUSE_CONFIRM: Duration = Duration::from_secs(1);
/// `supportedMediaCommands` bit for PAUSE.
const CAN_PAUSE: u64 = 1;

/// What the session has seen, for the API and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStatus {
    /// Times the link dropped and the session came back.
    pub resumed: u32,
    /// Another sender took the device; this session ended without stopping it.
    pub taken_over: bool,
    /// The link dropped and did not come back.
    pub lost: bool,
    /// The receiver's last `playerState` (PLAYING, BUFFERING, IDLE, …).
    pub player_state: Option<String>,
}

enum Cmd {
    Volume(Option<f64>, Option<bool>),
    Stop(oneshot::Sender<()>),
    Pause(oneshot::Sender<Paused>),
    /// Play after a pause; the answer is false when there is no media to play on.
    Play(oneshot::Sender<bool>),
}

/// How a pause went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paused {
    /// The receiver paused, holding its buffer: PLAY carries on.
    Held,
    /// The media was stopped instead (the receiver played on, or can't pause the live stream):
    /// the resume loads it again.
    Stopped,
    /// Nothing was sent (no media session yet, or the link is down): it plays its buffer out.
    Refused,
}

type Reconnect<S> =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<CastLink<S>, CastError>> + Send>> + Send + Sync>;

/// What a session plays, to load it again after a drop.
#[derive(Debug, Clone)]
pub struct Media {
    pub url: String,
    pub content_type: String,
    pub meta: Meta,
}

pub struct Session {
    tx: mpsc::UnboundedSender<Cmd>,
    status: Arc<Mutex<SessionStatus>>,
}

impl Session {
    /// Take over a launched and loaded `link`. `reconnect` opens a fresh link to the same device.
    pub fn spawn<S, F, Fut>(link: CastLink<S>, media: Media, ping_every: Duration, reconnect: F) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CastLink<S>, CastError>> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(SessionStatus::default()));
        let reconnect: Reconnect<S> = Arc::new(move || Box::pin(reconnect()));
        let st = status.clone();
        crate::link::runtime().spawn(run(link, media, ping_every, reconnect, rx, st));
        Self { tx, status }
    }

    pub fn status(&self) -> SessionStatus {
        self.status.lock().expect("cast status").clone()
    }

    /// Set the device volume (0.0..=1.0) or mute, through the live link.
    pub fn set_volume(&self, level: Option<f64>, muted: Option<bool>) {
        let _ = self.tx.send(Cmd::Volume(level, muted));
    }

    /// Pause the media. The answer comes once the receiver has said (within [`PAUSE_CONFIRM`]);
    /// a dropped answer means the session ended.
    pub fn pause(&self) -> oneshot::Receiver<Paused> {
        let (ack, rx) = oneshot::channel();
        let _ = self.tx.send(Cmd::Pause(ack));
        rx
    }

    /// Play the paused media again.
    pub fn play(&self) -> oneshot::Receiver<bool> {
        let (ack, rx) = oneshot::channel();
        let _ = self.tx.send(Cmd::Play(ack));
        rx
    }

    /// Stop the receiver app (unless someone else took the device) and end the session.
    pub async fn stop(self) {
        let (ack, done) = oneshot::channel();
        if self.tx.send(Cmd::Stop(ack)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(3), done).await;
        }
    }
}

/// The write half with request numbering.
struct Writer<S> {
    w: WriteHalf<S>,
    request_id: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Writer<S> {
    async fn send(
        &mut self,
        destination: &str,
        namespace: &str,
        mut payload: Value,
    ) -> Result<(), CastError> {
        if let Value::Object(m) = &mut payload {
            m.insert("requestId".into(), json!(self.request_id));
        }
        self.request_id += 1;
        let msg = CastMessage::text(SENDER, destination, namespace, &payload.to_string());
        self.w.write_all(&frame(&msg)).await?;
        self.w.flush().await?;
        Ok(())
    }
}

async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<CastMessage, CastError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 1 << 20 {
        return Err(CastError::Protocol(format!("frame of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    decode(&body)
}

/// A link split into a reader task (messages arrive on a channel, so `select!` never cancels a
/// half-read frame) and a writer.
struct Live<S> {
    writer: Writer<S>,
    incoming: mpsc::UnboundedReceiver<CastMessage>,
    reader: tokio::task::JoinHandle<()>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> Live<S> {
    fn new(link: CastLink<S>) -> Self {
        let (stream, request_id) = link.into_parts();
        let (mut r, w) = tokio::io::split(stream);
        let (tx, incoming) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            while let Ok(m) = read_message::<ReadHalf<S>>(&mut r).await {
                if tx.send(m).is_err() {
                    break;
                }
            }
        });
        Self {
            writer: Writer { w, request_id },
            incoming,
            reader,
        }
    }
}

impl<S> Drop for Live<S> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

struct Ids {
    session_id: Option<String>,
    transport_id: Option<String>,
    media_session_id: Option<u64>,
    /// The receiver's `supportedMediaCommands`, once it has said.
    commands: Option<u64>,
}

impl Ids {
    /// A media command for the loaded media: its transport and payload.
    fn media(&self, kind: &str) -> Option<(String, Value)> {
        let t = self.transport_id.clone()?;
        let m = self.media_session_id?;
        Some((t, json!({"type": kind, "mediaSessionId": m})))
    }
}

async fn run<S>(
    link: CastLink<S>,
    media: Media,
    ping_every: Duration,
    reconnect: Reconnect<S>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    status: Arc<Mutex<SessionStatus>>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut ids = Ids {
        session_id: link.session_id.clone(),
        transport_id: link.transport_id.clone(),
        media_session_id: link.media_session_id,
        commands: None,
    };
    // A PAUSE waiting for the receiver to say PAUSED, and until when
    let mut pausing: Option<(oneshot::Sender<Paused>, tokio::time::Instant)> = None;
    // The receiver played on after a PAUSE: later pauses stop it at once
    let mut pause_stops = false;
    let mut live = Some(Live::new(link));
    let mut ping = tokio::time::interval(ping_every);
    ping.tick().await;
    let mut last_reload = tokio::time::Instant::now();
    loop {
        let Some(l) = live.as_mut() else {
            // Lost for good: acknowledge the zone's stop without sending anything.
            finish(&mut rx).await;
            return;
        };
        let pause_due = pausing.as_ref().map(|(_, at)| *at);
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Cmd::Volume(level, muted)) => {
                    let mut vol = serde_json::Map::new();
                    if let Some(v) = level { vol.insert("level".into(), json!(v.clamp(0.0, 1.0))); }
                    if let Some(m) = muted { vol.insert("muted".into(), json!(m)); }
                    let _ = l.writer.send(RECEIVER_0, NS_RECEIVER, json!({"type": "SET_VOLUME", "volume": Value::Object(vol)})).await;
                }
                Some(Cmd::Pause(ack)) => {
                    if let Some((earlier, _)) = pausing.take() {
                        let _ = earlier.send(Paused::Held);
                    }
                    let can_pause = ids.commands.is_none_or(|c| c & CAN_PAUSE != 0);
                    let kind = if pause_stops || !can_pause { "STOP" } else { "PAUSE" };
                    let sent = match ids.media(kind) {
                        Some((t, payload)) => l.writer.send(&t, NS_MEDIA, payload).await.is_ok(),
                        None => false,
                    };
                    if !sent {
                        let _ = ack.send(Paused::Refused);
                    } else if kind == "STOP" {
                        let _ = ack.send(Paused::Stopped);
                    } else {
                        pausing = Some((ack, tokio::time::Instant::now() + PAUSE_CONFIRM));
                    }
                }
                Some(Cmd::Play(ack)) => {
                    let idle = status.lock().expect("cast status").player_state.as_deref() == Some("IDLE");
                    let sent = match ids.media("PLAY") {
                        Some((t, payload)) if !idle => l.writer.send(&t, NS_MEDIA, payload).await.is_ok(),
                        _ => false,
                    };
                    let _ = ack.send(sent);
                }
                Some(Cmd::Stop(ack)) => {
                    if let Some(sid) = &ids.session_id {
                        let _ = l.writer.send(RECEIVER_0, NS_RECEIVER, json!({"type": "STOP", "sessionId": sid})).await;
                    }
                    let _ = ack.send(());
                    return;
                }
                None => return,
            },
            msg = l.incoming.recv() => match msg {
                Some(m) => {
                    let v: Value = m.payload_utf8.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
                    let t = v.get("type").and_then(Value::as_str).unwrap_or("");
                    match (m.namespace.as_str(), t) {
                        (NS_HEARTBEAT, "PING") => {
                            let _ = l.writer.send(&m.source_id, NS_HEARTBEAT, json!({"type": "PONG"})).await;
                        }
                        (NS_RECEIVER, "RECEIVER_STATUS") => {
                            if let Some(apps) = v.pointer("/status/applications").and_then(Value::as_array) {
                                let ours = apps.iter().any(|a| a.get("sessionId").and_then(Value::as_str) == ids.session_id.as_deref());
                                if !ours {
                                    tracing::info!("another sender took the Cast device; stepping aside");
                                    status.lock().expect("cast status").taken_over = true;
                                    finish(&mut rx).await;
                                    return;
                                }
                            }
                        }
                        (NS_MEDIA, "MEDIA_STATUS") => {
                            let s0 = v.pointer("/status/0");
                            let state = s0.and_then(|s| s.get("playerState")).and_then(Value::as_str).map(str::to_string);
                            let idle_error = s0.and_then(|s| s.get("idleReason")).and_then(Value::as_str) == Some("ERROR");
                            if let Some(m) = s0.and_then(|s| s.get("mediaSessionId")).and_then(Value::as_u64) {
                                ids.media_session_id = Some(m);
                            }
                            if let Some(c) = s0.and_then(|s| s.get("supportedMediaCommands")).and_then(Value::as_u64) {
                                ids.commands = Some(c);
                            }
                            let outcome = match state.as_deref() {
                                Some("PAUSED") => Some(Paused::Held),
                                Some("IDLE") => Some(Paused::Stopped),
                                _ => None,
                            };
                            if let Some(how) = outcome {
                                if let Some((ack, _)) = pausing.take() {
                                    let _ = ack.send(how);
                                }
                            }
                            status.lock().expect("cast status").player_state = state;
                            if idle_error && last_reload.elapsed() > Duration::from_secs(10) {
                                last_reload = tokio::time::Instant::now();
                                tracing::warn!("the Cast receiver reported a media error; loading the stream again");
                                if let Some(t) = ids.transport_id.clone() {
                                    let _ = l.writer.send(&t, NS_MEDIA, load_payload(&media)).await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                None => {
                    // The link dropped: come back to the same session if it still runs.
                    live = None;
                    match resume(&reconnect, &mut ids, &media).await {
                        Some(link) => {
                            status.lock().expect("cast status").resumed += 1;
                            live = Some(Live::new(link));
                        }
                        None => {
                            tracing::warn!("lost the Cast device");
                            status.lock().expect("cast status").lost = true;
                        }
                    }
                }
            },
            _ = async { tokio::time::sleep_until(pause_due.expect("pausing")).await }, if pause_due.is_some() => {
                let Some((ack, _)) = pausing.take() else { continue };
                let state = status.lock().expect("cast status").player_state.clone();
                // Only a receiver last seen playing on is stopped; one that didn't say may well have paused
                if !matches!(state.as_deref(), Some("PLAYING" | "BUFFERING")) {
                    let _ = ack.send(Paused::Held);
                    continue;
                }
                tracing::info!("the Cast receiver plays on after PAUSE; stopping it for pauses from now on");
                pause_stops = true;
                let stopped = match ids.media("STOP") {
                    Some((t, payload)) => l.writer.send(&t, NS_MEDIA, payload).await.is_ok(),
                    None => false,
                };
                let _ = ack.send(if stopped { Paused::Stopped } else { Paused::Refused });
            }
            _ = ping.tick() => {
                let _ = l.writer.send(RECEIVER_0, NS_HEARTBEAT, json!({"type": "PING"})).await;
            }
        }
    }
}

/// Wait for the zone's stop (or the session handle to go) and acknowledge it without sending STOP.
async fn finish(rx: &mut mpsc::UnboundedReceiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        if let Cmd::Stop(ack) = cmd {
            let _ = ack.send(());
            return;
        }
    }
}

fn load_payload(media: &Media) -> Value {
    json!({
        "type": "LOAD",
        "autoplay": true,
        "currentTime": 0,
        "media": {
            "contentId": media.url,
            "contentType": media.content_type,
            "streamType": "LIVE",
            "metadata": crate::meta_json(&media.meta, "Pubky Music"),
        }
    })
}

/// Reconnect; join our session if the receiver still runs it, else launch and load again.
async fn resume<S>(reconnect: &Reconnect<S>, ids: &mut Ids, media: &Media) -> Option<CastLink<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut wait = Duration::from_secs(1);
    for attempt in 1..=RESUME_TRIES {
        tokio::time::sleep(wait).await;
        wait *= 2;
        let mut link = match reconnect().await {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!(attempt, "Cast reconnect failed: {e}");
                continue;
            }
        };
        match link.rejoin(ids.session_id.as_deref()).await {
            Ok(true) => {
                tracing::info!("Cast session resumed");
                return Some(link);
            }
            Ok(false) => {
                // The session ended on the device (it was idle too long, or rebooted): start again.
                if link.launch().await.is_ok()
                    && link
                        .load(&media.url, &media.content_type, &media.meta)
                        .await
                        .is_ok()
                {
                    ids.session_id = link.session_id.clone();
                    ids.transport_id = link.transport_id.clone();
                    ids.media_session_id = link.media_session_id;
                    tracing::info!("Cast session started again");
                    return Some(link);
                }
            }
            Err(e) => tracing::debug!(attempt, "Cast rejoin failed: {e}"),
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::link::NS_CONNECTION;
    use crate::DEFAULT_MEDIA_RECEIVER;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::broadcast;

    /// What the test makes the fake device do.
    #[derive(Clone, Debug)]
    pub(crate) enum Ctl {
        /// Close every open connection (a Wi-Fi blip).
        Drop,
        /// Another sender launched something else.
        TakeOver,
        Ping,
    }

    #[derive(Default)]
    pub(crate) struct Fake {
        log: Mutex<Vec<String>>,
        connections: Mutex<u32>,
        /// Answers PAUSE but plays on (as a Linn does on the live stream).
        pub(crate) ignores_pause: std::sync::atomic::AtomicBool,
    }

    /// A default media receiver over plain TCP that keeps its session across connections.
    async fn fake_device(listener: TcpListener, fake: Arc<Fake>, ctl: broadcast::Sender<Ctl>) {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let n = {
                let mut c = fake.connections.lock().unwrap();
                *c += 1;
                *c
            };
            let fake = fake.clone();
            let mut ctl = ctl.subscribe();
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(sock);
                let (tx, mut rx) = mpsc::unbounded_channel();
                tokio::spawn(async move {
                    while let Ok(m) = read_message(&mut r).await {
                        if tx.send(m).is_err() {
                            break;
                        }
                    }
                });
                let app =
                    json!({"appId": DEFAULT_MEDIA_RECEIVER, "transportId": "web-7", "sessionId": "sess-1"});
                loop {
                    tokio::select! {
                        m = rx.recv() => {
                            let Some(m) = m else { return };
                            let v: Value = serde_json::from_str(m.payload_utf8.as_deref().unwrap_or("{}")).unwrap();
                            let t = v["type"].as_str().unwrap_or("").to_string();
                            let ns = m.namespace.rsplit('.').next().unwrap().to_string();
                            let media_session = v["mediaSessionId"].as_u64().map(|m| format!(" {m}")).unwrap_or_default();
                            fake.log.lock().unwrap().push(format!("{n} {ns} {t}{media_session}"));
                            let media_status = |state: &str| json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 9, "playerState": state, "supportedMediaCommands": 15}]});
                            match (ns.as_str(), t.as_str()) {
                                (_, "LAUNCH" | "GET_STATUS") => {
                                    let _ = send(&mut w, NS_RECEIVER, json!({"type": "RECEIVER_STATUS", "status": {"applications": [app]}})).await;
                                }
                                (_, "LOAD") => {
                                    let _ = send(&mut w, NS_MEDIA, json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 9, "playerState": "PLAYING"}]})).await;
                                }
                                ("media", "PAUSE") => {
                                    let plays_on = fake.ignores_pause.load(std::sync::atomic::Ordering::SeqCst);
                                    let _ = send(&mut w, NS_MEDIA, media_status(if plays_on { "PLAYING" } else { "PAUSED" })).await;
                                }
                                ("media", "PLAY") => {
                                    let _ = send(&mut w, NS_MEDIA, media_status("PLAYING")).await;
                                }
                                ("media", "STOP") => {
                                    let _ = send(&mut w, NS_MEDIA, media_status("IDLE")).await;
                                }
                                _ => {}
                            }
                        }
                        c = ctl.recv() => match c {
                            Ok(Ctl::Drop) => return,
                            Ok(Ctl::TakeOver) => {
                                let other = json!({"appId": "233637DE", "transportId": "web-9", "sessionId": "yt"});
                                let _ = send(&mut w, NS_RECEIVER, json!({"type": "RECEIVER_STATUS", "status": {"applications": [other]}})).await;
                            }
                            Ok(Ctl::Ping) => {
                                let _ = send(&mut w, NS_HEARTBEAT, json!({"type": "PING"})).await;
                            }
                            Err(_) => return,
                        }
                    }
                }
            });
        }
    }

    async fn send(w: &mut WriteHalf<TcpStream>, ns: &'static str, v: Value) -> std::io::Result<()> {
        let msg = CastMessage::text("receiver-0", SENDER, ns, &v.to_string());
        w.write_all(&frame(&msg)).await
    }

    pub(crate) async fn setup() -> (Session, Arc<Fake>, broadcast::Sender<Ctl>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = Arc::new(Fake::default());
        let (ctl, _) = broadcast::channel(8);
        crate::link::runtime().spawn(fake_device(listener, fake.clone(), ctl.clone()));
        let connect =
            move || async move { Ok::<_, CastError>(CastLink::new(TcpStream::connect(addr).await?)) };
        let media = Media {
            url: "http://node/stream.wav?k=1".into(),
            content_type: "audio/wav".into(),
            meta: Meta {
                title: Some("Bohemian Rhapsody".into()),
                artist: Some("Queen".into()),
                album: Some("A Night at the Opera".into()),
                image: Some("http://node/v1/art/7?k=2".into()),
            },
        };
        let m = media.clone();
        let session = crate::link::runtime()
            .spawn(async move {
                let mut link = connect().await.unwrap();
                link.launch().await.unwrap();
                link.load(&m.url, &m.content_type, &m.meta).await.unwrap();
                Session::spawn(link, m, Duration::from_millis(200), connect)
            })
            .await
            .unwrap();
        (session, fake, ctl)
    }

    pub(crate) fn log(f: &Fake) -> Vec<String> {
        f.log.lock().unwrap().clone()
    }

    pub(crate) async fn until(f: &Fake, line: &str) {
        for _ in 0..200 {
            if log(f).iter().any(|l| l == line) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("never saw {line:?} in {:?}", log(f));
    }

    #[tokio::test]
    async fn the_session_keeps_the_link_alive_and_carries_commands() {
        let (session, fake, ctl) = setup().await;
        ctl.send(Ctl::Ping).unwrap();
        until(&fake, "1 heartbeat PONG").await;
        until(&fake, "1 heartbeat PING").await; // its own keepalive
        session.set_volume(Some(0.3), None);
        until(&fake, "1 receiver SET_VOLUME").await;
        assert_eq!(session.status().player_state, None);
        session.stop().await;
        until(&fake, "1 receiver STOP").await;
    }

    #[tokio::test]
    async fn the_load_carries_title_artist_album_and_art() {
        let m = Meta {
            title: Some("T".into()),
            artist: Some("A".into()),
            album: Some("L".into()),
            image: Some("http://x/a.jpg".into()),
        };
        let v = crate::meta_json(&m, "fallback");
        assert_eq!(v["metadataType"], 3);
        assert_eq!(v["title"], "T");
        assert_eq!(v["artist"], "A");
        assert_eq!(v["albumName"], "L");
        assert_eq!(v["images"][0]["url"], "http://x/a.jpg");
        assert_eq!(
            crate::meta_json(&Meta::default(), "Pubky Music")["title"],
            "Pubky Music"
        );
    }

    #[tokio::test]
    async fn a_dropped_link_resumes_the_same_session() {
        let (session, fake, ctl) = setup().await;
        ctl.send(Ctl::Drop).unwrap();
        until(&fake, "2 receiver GET_STATUS").await;
        until(&fake, "2 connection CONNECT").await;
        for _ in 0..100 {
            if session.status().resumed == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(session.status().resumed, 1);
        // Joined, not relaunched.
        assert!(
            !log(&fake).iter().any(|l| l == "2 receiver LAUNCH"),
            "{:?}",
            log(&fake)
        );
        session.stop().await;
        until(&fake, "2 receiver STOP").await;
    }

    #[tokio::test]
    async fn another_sender_taking_the_device_is_left_alone() {
        let (session, fake, ctl) = setup().await;
        ctl.send(Ctl::TakeOver).unwrap();
        for _ in 0..100 {
            if session.status().taken_over {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(session.status().taken_over);
        session.stop().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !log(&fake).iter().any(|l| l.ends_with("receiver STOP")),
            "{:?}",
            log(&fake)
        );
        let _ = NS_CONNECTION;
    }

    /// Pause is the receiver's own media PAUSE for the loaded media (it played out seconds of
    /// buffer before), and the resume its PLAY.
    #[tokio::test]
    async fn pause_and_play_are_the_receivers_own_media_commands() {
        let (session, fake, _ctl) = setup().await;
        assert_eq!(session.pause().await.unwrap(), Paused::Held);
        assert!(session.play().await.unwrap());
        until(&fake, "1 media PLAY 9").await;
        let media: Vec<String> = log(&fake).into_iter().filter(|l| l.contains(" media ")).collect();
        assert_eq!(media, ["1 media LOAD", "1 media PAUSE 9", "1 media PLAY 9"]);
        session.stop().await;
    }

    /// A receiver that answers PAUSE but plays on is stopped instead, at once from then on.
    #[tokio::test]
    async fn a_receiver_that_plays_on_after_pause_is_stopped() {
        let (session, fake, _ctl) = setup().await;
        fake.ignores_pause
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let t = std::time::Instant::now();
        assert_eq!(session.pause().await.unwrap(), Paused::Stopped);
        assert!(t.elapsed() >= PAUSE_CONFIRM, "{:?}", t.elapsed());
        until(&fake, "1 media STOP 9").await;
        // Learned: the next pause stops without trying PAUSE
        let t = std::time::Instant::now();
        assert_eq!(session.pause().await.unwrap(), Paused::Stopped);
        assert!(t.elapsed() < PAUSE_CONFIRM, "{:?}", t.elapsed());
        for _ in 0..200 {
            if log(&fake).iter().filter(|l| *l == "1 media STOP 9").count() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let media: Vec<String> = log(&fake).into_iter().filter(|l| l.contains(" media ")).collect();
        assert_eq!(
            media,
            [
                "1 media LOAD",
                "1 media PAUSE 9",
                "1 media STOP 9",
                "1 media STOP 9"
            ]
        );
        session.stop().await;
    }
}
