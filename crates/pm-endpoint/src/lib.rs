//! The desktop app's built-in player: this machine's outputs over the `pm-stream` protocol, for
//! the one node the app is connected to. One TCP session per open sink; the session thread blocks
//! on the local sink's `write`, so a full DAC buffer holds back the node through the `Ack`
//! handshake.
//!
//! The endpoint never decodes or converts: it receives the negotiated format and packs it exactly
//! as a local output would. It answers only a client that shows its [`Key`] in `Hello`: the app
//! makes one each time it starts the player and registers it with its node, which stores it with
//! the address. A connection without the key is closed within the `Hello` deadline and never holds
//! a session slot.

use pm_model::AudioFormat;
use pm_stream::{block_on, decode_samples, read_frame, write_frame, Frame, Incoming, Key, StreamError};
/// The outputs an endpoint offers (for the app, which has no other use for `pm-zones`).
pub use pm_zones::OutputRegistry;
use pm_zones::{AudioSink, Output, OutputId};
use std::collections::HashSet;
use std::io::{BufReader, BufWriter, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Sessions at once: a zone per output plus the node's short control sessions fit many times.
pub const MAX_SESSIONS: usize = 16;
/// Connections at once that have not shown the key yet; each is closed by the `Hello` deadline.
pub const MAX_PENDING: usize = 8;
/// How long a new connection has to say `Hello` with the key, all of it.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a session without an open output may be silent (the node's control sessions are short).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a write may block on a peer that does not read.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// An open output may be paused for hours, so it has no read timeout; TCP keepalive notices a peer
/// that is gone.
const KEEPALIVE: Duration = Duration::from_secs(60);

pub struct Endpoint {
    pub name: String,
    pub outputs: OutputRegistry,
    key: Key,
    /// Outputs with an open session; a second `Open` is refused.
    busy: Mutex<HashSet<OutputId>>,
    sessions: Arc<AtomicUsize>,
    pending: Arc<AtomicUsize>,
    limits: Limits,
}

/// How many connections at once and how long a peer may be silent.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Sessions of clients that showed the key.
    pub max_sessions: usize,
    /// Connections that have not shown it yet.
    pub max_pending: usize,
    pub hello: Duration,
    pub idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sessions: MAX_SESSIONS,
            max_pending: MAX_PENDING,
            hello: HELLO_TIMEOUT,
            idle: IDLE_TIMEOUT,
        }
    }
}

/// Counts a connection while it lives.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(count: &Arc<AtomicUsize>, max: usize) -> Option<Slot> {
        if count.fetch_add(1, Ordering::SeqCst) >= max {
            count.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot(count.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Reads that all end by one deadline: a peer sending a byte now and then cannot stretch it.
struct Deadline<'a> {
    stream: &'a TcpStream,
    until: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no Hello in time",
            ));
        }
        self.stream.set_read_timeout(Some(left))?;
        (&mut &*self.stream).read(buf)
    }
}

/// A listener served on its own thread; dropping it stops taking connections (sessions already
/// open carry on), so the app can listen elsewhere when its node changes.
pub struct Serving {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Serving {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept so the thread sees the flag and the port is free again
        let mut wake = self.addr;
        if wake.ip().is_unspecified() {
            wake.set_ip(match wake {
                SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            });
        }
        if TcpStream::connect_timeout(&wake, Duration::from_secs(1)).is_ok() {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}

impl Endpoint {
    /// An endpoint that answers only clients showing `key`.
    pub fn new(name: &str, outputs: OutputRegistry, key: Key) -> Arc<Self> {
        Self::with_limits(name, outputs, key, Limits::default())
    }

    /// The same endpoint with other limits (tests).
    pub fn with_limits(name: &str, outputs: OutputRegistry, key: Key, limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            outputs,
            key,
            busy: Mutex::new(HashSet::new()),
            sessions: Arc::new(AtomicUsize::new(0)),
            pending: Arc::new(AtomicUsize::new(0)),
            limits,
        })
    }

    /// Accept connections forever, one thread each.
    pub fn serve(self: &Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        self.serve_until(listener, &AtomicBool::new(false));
        Ok(())
    }

    /// Serve `listener` on a thread of its own until the returned handle is dropped.
    pub fn spawn(self: &Arc<Self>, listener: TcpListener) -> std::io::Result<Serving> {
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (ep, flag) = (self.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("pm-endpoint {addr}"))
            .spawn(move || ep.serve_until(listener, &flag))?;
        Ok(Serving {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    fn serve_until(self: &Arc<Self>, listener: TcpListener, stop: &AtomicBool) {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    continue;
                }
            };
            let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            let Some(pending) = Slot::take(&self.pending, self.limits.max_pending) else {
                tracing::warn!(%peer, "refused: {} connections already waiting for Hello", self.limits.max_pending);
                refuse(&stream, "too many connections");
                continue;
            };
            let ep = self.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("pm-endpoint {peer}"))
                .spawn(move || {
                    if let Err(e) = ep.connection(stream, pending) {
                        tracing::info!(%peer, "session ended: {e}");
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!("no thread for a connection: {e}");
            }
        }
    }

    fn welcome(&self) -> Frame {
        Frame::Welcome {
            name: self.name.clone(),
            version: pm_stream::PROTOCOL_VERSION,
            outputs: self.outputs.list(),
        }
    }

    /// A new connection: `Hello` with the key by the deadline, then a session slot.
    fn connection(&self, stream: TcpStream, pending: Slot) -> Result<(), StreamError> {
        stream.set_nodelay(true)?;
        // Until the key is shown nothing may hold the thread past the Hello deadline
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        let hello = read_frame(&mut Deadline {
            stream: &stream,
            until: Instant::now() + self.limits.hello,
        });
        let client = match hello {
            Ok(Incoming::Control(Frame::Hello { client, version, key })) => {
                if version != pm_stream::PROTOCOL_VERSION {
                    let update = if version < pm_stream::PROTOCOL_VERSION {
                        "update the node"
                    } else {
                        "update the Pubky Music app"
                    };
                    let msg = format!(
                        "endpoint speaks protocol {}, this node {version}; {update}",
                        pm_stream::PROTOCOL_VERSION
                    );
                    refuse(&stream, &msg);
                    return Err(StreamError::Remote(msg));
                }
                if !self.key.matches(&key) {
                    let msg = if key.is_empty() {
                        "no key: this player answers only the node it registered with"
                    } else {
                        "wrong key: this player answers only the node it registered with"
                    };
                    refuse(&stream, msg);
                    return Err(StreamError::Remote(msg.into()));
                }
                client
            }
            Ok(_) => {
                refuse(&stream, "expected Hello");
                return Err(StreamError::Unexpected("not Hello".into()));
            }
            Err(e) => return Err(e),
        };
        let max = self.limits.max_sessions;
        let Some(_slot) = Slot::take(&self.sessions, max) else {
            refuse(&stream, "too many sessions");
            return Err(StreamError::Remote(format!("refused: {max} sessions already")));
        };
        drop(pending);
        tracing::debug!(%client, "hello");
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let _ = socket2::SockRef::from(&stream).set_tcp_keepalive(
            &socket2::TcpKeepalive::new()
                .with_time(KEEPALIVE)
                .with_interval(Duration::from_secs(10)),
        );
        let mut r = BufReader::new(stream.try_clone()?);
        let mut w = BufWriter::new(stream);
        write_frame(&mut w, &self.welcome())?;
        self.session(&mut r, &mut w)
    }

    /// A client that showed the key: control frames until `Open`, then audio.
    fn session(&self, r: &mut BufReader<TcpStream>, w: &mut BufWriter<TcpStream>) -> Result<(), StreamError> {
        r.get_ref().set_read_timeout(Some(self.limits.idle))?;
        loop {
            let frame = match read_frame(r) {
                Ok(Incoming::Control(f)) => f,
                Ok(Incoming::Audio(_)) => {
                    write_frame(
                        w,
                        &Frame::Error {
                            message: "no output open".into(),
                        },
                    )?;
                    continue;
                }
                Err(StreamError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };
            match frame {
                Frame::Close => return Ok(()),
                Frame::Hello { .. } => write_frame(w, &self.welcome())?,
                Frame::SetVolume { output, volume } => {
                    let reply = match self.outputs.get(&OutputId(output.clone())) {
                        Some(o) => match block_on(o.set_volume(volume)) {
                            Ok(()) => Frame::Done,
                            Err(e) => Frame::Error {
                                message: e.to_string(),
                            },
                        },
                        None => Frame::Error {
                            message: format!("no output {output}"),
                        },
                    };
                    write_frame(w, &reply)?;
                }
                Frame::Open { output, format } => {
                    let id = OutputId(output.clone());
                    let Some(out) = self.outputs.get(&id) else {
                        write_frame(
                            w,
                            &Frame::Error {
                                message: format!("no output {output}"),
                            },
                        )?;
                        continue;
                    };
                    if !self.busy.lock().expect("busy").insert(id.clone()) {
                        write_frame(
                            w,
                            &Frame::Error {
                                message: format!("{output} is in use"),
                            },
                        )?;
                        continue;
                    }
                    // A paused zone keeps its output open without a word for as long as it likes
                    r.get_ref().set_read_timeout(None)?;
                    let result = self.run_open(r, w, out, format);
                    self.busy.lock().expect("busy").remove(&id);
                    match result {
                        Ok(()) => return Ok(()),
                        Err(e) => {
                            let _ = write_frame(
                                w,
                                &Frame::Error {
                                    message: e.to_string(),
                                },
                            );
                            return Err(e);
                        }
                    }
                }
                other => {
                    write_frame(
                        w,
                        &Frame::Error {
                            message: format!("unexpected {other:?}"),
                        },
                    )?;
                }
            }
        }
    }

    /// The audio phase: open the sink, then stream until Close or EOF.
    fn run_open(
        &self,
        r: &mut BufReader<TcpStream>,
        w: &mut BufWriter<TcpStream>,
        out: Arc<dyn Output>,
        format: AudioFormat,
    ) -> Result<(), StreamError> {
        let mut sink: Box<dyn AudioSink> = match block_on(out.open(format)) {
            Ok(s) => s,
            Err(e) => {
                write_frame(
                    w,
                    &Frame::Error {
                        message: e.to_string(),
                    },
                )?;
                return Ok(());
            }
        };
        write_frame(
            w,
            &Frame::Opened {
                buffer_frames: sink.buffer_level().capacity_frames,
            },
        )?;
        loop {
            match read_frame(r) {
                Ok(Incoming::Audio(bytes)) => {
                    let block = decode_samples(format, &bytes)?;
                    sink.write(&block)
                        .map_err(|e| StreamError::Remote(e.to_string()))?;
                    write_frame(
                        w,
                        &Frame::Ack {
                            level: sink.buffer_level(),
                            frames_written: sink.frames_written(),
                        },
                    )?;
                }
                Ok(Incoming::Control(Frame::Flush)) => {
                    sink.flush().map_err(|e| StreamError::Remote(e.to_string()))?;
                    write_frame(w, &Frame::Flushed)?;
                }
                Ok(Incoming::Control(Frame::Drain)) => {
                    sink.drain().map_err(|e| StreamError::Remote(e.to_string()))?;
                    write_frame(w, &Frame::Drained)?;
                }
                Ok(Incoming::Control(Frame::Close)) => {
                    let _ = sink.flush();
                    return Ok(());
                }
                Ok(Incoming::Control(other)) => {
                    write_frame(
                        w,
                        &Frame::Error {
                            message: format!("unexpected {other:?} while open"),
                        },
                    )?;
                }
                Err(StreamError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Connection dropped: stop immediately rather than play out stale audio.
                    let _ = sink.flush();
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Tell a peer why it is turned away (best effort: it may not read).
fn refuse(stream: &TcpStream, message: &str) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = write_frame(
        &mut BufWriter::new(stream),
        &Frame::Error {
            message: message.into(),
        },
    );
}

/// The outputs this machine offers: the null output always, ALSA `hw:`
/// devices with the `alsa` feature, the system's shared-mode outputs with
/// `cpal`.
pub fn local_outputs() -> OutputRegistry {
    let registry = device_outputs();
    registry.register(Arc::new(pm_zones::outputs::NullOutput::from_env()));
    registry
}

/// This machine's audio devices only, without the null output (the app's
/// player: a "Null output" zone per phone or laptop would only be clutter).
pub fn device_outputs() -> OutputRegistry {
    let registry = OutputRegistry::new();
    #[cfg(feature = "alsa")]
    for out in pm_out_local::alsa::AlsaOutput::enumerate() {
        registry.register(Arc::new(out));
    }
    #[cfg(feature = "cpal")]
    for out in pm_out_local::cpal_out::CpalOutput::enumerate() {
        registry.register(Arc::new(out));
    }
    registry
}

/// This machine's name, for the node's output list: `$HOSTNAME`, then
/// `/etc/hostname` (on Android the phone's model), else a generic name.
pub fn machine_name() -> String {
    #[cfg(target_os = "android")]
    if let Some(model) = android_model() {
        return model;
    }
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "Pubky Music endpoint".into())
}

/// "Google Pixel 8", from the system properties every app may read.
#[cfg(target_os = "android")]
fn android_model() -> Option<String> {
    let prop = |name: &str| {
        std::process::Command::new("/system/bin/getprop")
            .arg(name)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let model = prop("ro.product.model")?;
    Some(match prop("ro.product.manufacturer") {
        // "Google" + "Pixel 8"; Samsung models already read "SM-S911B".
        Some(maker) if !model.to_lowercase().starts_with(&maker.to_lowercase()) => {
            let mut m = maker.chars();
            let maker: String = m
                .next()
                .map(|c| c.to_uppercase().chain(m).collect())
                .unwrap_or_default();
            format!("{maker} {model}")
        }
        _ => model,
    })
}
