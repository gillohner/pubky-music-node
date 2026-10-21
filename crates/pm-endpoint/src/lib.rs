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
use std::sync::{Arc, Mutex};

pub struct Endpoint {
    pub name: String,
    pub outputs: OutputRegistry,
    /// Outputs with an open session; a second `Open` is refused.
    busy: Mutex<HashSet<OutputId>>,
}

impl Endpoint {
    pub fn new(name: &str, outputs: OutputRegistry) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            outputs,
            busy: Mutex::new(HashSet::new()),
        })
    }

    /// Accept connections forever, one thread per session.
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
            let ep = self.clone();
            std::thread::Builder::new()
                .name(format!("pm-endpoint {peer}"))
                .spawn(move || {
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
    let registry = OutputRegistry::new();
    registry.register(Arc::new(pm_zones::outputs::NullOutput::from_env()));
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
/// `/etc/hostname`, else a generic name.
pub fn machine_name() -> String {
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
