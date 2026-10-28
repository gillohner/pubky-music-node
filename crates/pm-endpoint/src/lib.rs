//! The endpoint daemon ("our RoonBridge", handoff M2): exposes this
//! machine's outputs over the `pm-stream` protocol. One TCP session per
//! open sink; the session thread blocks on the local sink's `write`, so a
//! full DAC buffer holds back the node through the `Ack` handshake.
//!
//! The endpoint never decodes or converts: it receives the negotiated
//! format and packs it exactly as a local output would. Discovery is mDNS
//! (`pm_stream::SERVICE_TYPE`), done by the binary; this library is the
//! session logic, so tests can run it on a loopback listener.

use pm_model::AudioFormat;
use pm_stream::{
    block_on, decode_samples, expect_control, read_frame, write_frame, Frame, Incoming, StreamError,
};
use pm_zones::{AudioSink, Output, OutputId, OutputRegistry};
use std::collections::HashSet;
use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Sessions at once: a zone per output plus the node's short control sessions fit many times.
pub const MAX_SESSIONS: usize = 16;
/// How long a new connection has to say `Hello`.
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
    /// Outputs with an open session; a second `Open` is refused.
    busy: Mutex<HashSet<OutputId>>,
    sessions: Arc<AtomicUsize>,
    limits: Limits,
}

/// How many sessions at once and how long a peer may be silent.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_sessions: usize,
    pub hello: Duration,
    pub idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sessions: MAX_SESSIONS,
            hello: HELLO_TIMEOUT,
            idle: IDLE_TIMEOUT,
        }
    }
}

/// Counts a session while it lives.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Endpoint {
    pub fn new(name: &str, outputs: OutputRegistry) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            outputs,
            busy: Mutex::new(HashSet::new()),
            sessions: Arc::new(AtomicUsize::new(0)),
            limits: Limits::default(),
        })
    }

    /// The same endpoint with other limits (tests).
    pub fn with_limits(name: &str, outputs: OutputRegistry, limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            outputs,
            busy: Mutex::new(HashSet::new()),
            sessions: Arc::new(AtomicUsize::new(0)),
            limits,
        })
    }

    /// Accept connections forever, one thread per session, at most `max_sessions` at once.
    pub fn serve(self: &Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    continue;
                }
            };
            let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            let max = self.limits.max_sessions;
            if self.sessions.fetch_add(1, Ordering::SeqCst) >= max {
                self.sessions.fetch_sub(1, Ordering::SeqCst);
                tracing::warn!(%peer, "refused: {max} sessions already");
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                let _ = write_frame(
                    &mut BufWriter::new(&stream),
                    &Frame::Error {
                        message: "too many sessions".into(),
                    },
                );
                continue;
            }
            let slot = Slot(self.sessions.clone());
            let ep = self.clone();
            std::thread::Builder::new()
                .name(format!("pm-endpoint {peer}"))
                .spawn(move || {
                    let _slot = slot;
                    if let Err(e) = ep.session(stream) {
                        tracing::info!(%peer, "session ended: {e}");
                    }
                })?;
        }
        Ok(())
    }

    fn welcome(&self) -> Frame {
        Frame::Welcome {
            name: self.name.clone(),
            version: pm_stream::PROTOCOL_VERSION,
            outputs: self.outputs.list(),
        }
    }

    fn session(&self, stream: TcpStream) -> Result<(), StreamError> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(self.limits.hello))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let _ = socket2::SockRef::from(&stream).set_tcp_keepalive(
            &socket2::TcpKeepalive::new()
                .with_time(KEEPALIVE)
                .with_interval(Duration::from_secs(10)),
        );
        let mut r = BufReader::new(stream.try_clone()?);
        let mut w = BufWriter::new(stream);
        match expect_control(&mut r)? {
            Frame::Hello { client, version } => {
                tracing::debug!(%client, version, "hello");
                write_frame(&mut w, &self.welcome())?;
            }
            other => {
                write_frame(
                    &mut w,
                    &Frame::Error {
                        message: "expected Hello".into(),
                    },
                )?;
                return Err(StreamError::Unexpected(format!("{other:?}")));
            }
        }
        r.get_ref().set_read_timeout(Some(self.limits.idle))?;
        loop {
            let frame = match read_frame(&mut r) {
                Ok(Incoming::Control(f)) => f,
                Ok(Incoming::Audio(_)) => {
                    write_frame(
                        &mut w,
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
                Frame::Hello { .. } => write_frame(&mut w, &self.welcome())?,
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
                    write_frame(&mut w, &reply)?;
                }
                Frame::Open { output, format } => {
                    let id = OutputId(output.clone());
                    let Some(out) = self.outputs.get(&id) else {
                        write_frame(
                            &mut w,
                            &Frame::Error {
                                message: format!("no output {output}"),
                            },
                        )?;
                        continue;
                    };
                    if !self.busy.lock().expect("busy").insert(id.clone()) {
                        write_frame(
                            &mut w,
                            &Frame::Error {
                                message: format!("{output} is in use"),
                            },
                        )?;
                        continue;
                    }
                    // A paused zone keeps its output open without a word for as long as it likes
                    r.get_ref().set_read_timeout(None)?;
                    let result = self.run_open(&mut r, &mut w, out, format);
                    self.busy.lock().expect("busy").remove(&id);
                    match result {
                        Ok(()) => return Ok(()),
                        Err(e) => {
                            let _ = write_frame(
                                &mut w,
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
                        &mut w,
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
