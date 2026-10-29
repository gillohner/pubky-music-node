//! Node-side `Output` for the desktop app's built-in player (`pm-endpoint`) over the `pm-stream`
//! protocol.
//!
//! [`NetOutput::connect`] asks an endpoint for its outputs (one `NetOutput`
//! per remote output, caps copied verbatim); `open` starts a dedicated TCP
//! session whose `write` sends a block and waits for the endpoint's `Ack`,
//! so the remote DAC paces the zone exactly like a local one. Endpoints are
//! never discovered: the app registers its player's address and key with
//! the node, and every connection shows that key.
//!
//! Each `Ack` reads the endpoint's clock: what its sink has written and still holds. An output
//! on a `hw:` DAC (ALSA) holds what the hardware has not played yet, so the node keeps such
//! outputs in time with each other in a group ([`OutputCaps::groupable`], ADR 0009); one behind a
//! system mixer (cpal: a phone, a Mac, a Windows PC) has latency nobody reports.

use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, SampleKind, Volume};
use pm_stream::{expect_control, write_audio, write_frame, Frame, Key, StreamError};
use pm_zones::sink::check_block;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, OutputInfo, SinkError};
use std::io::{BufReader, BufWriter};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

fn io_err(e: StreamError) -> SinkError {
    SinkError::Device(e.to_string())
}

/// The key of one endpoint, shared by all its outputs: when the app registers again with a new
/// key (it makes one each start), the zones that already play there use it too.
#[derive(Clone, Default)]
pub struct KeyCell(Arc<RwLock<Key>>);

impl KeyCell {
    pub fn new(key: Key) -> Self {
        Self(Arc::new(RwLock::new(key)))
    }

    pub fn set(&self, key: Key) {
        *self.0.write().expect("endpoint key") = key;
    }

    pub fn get(&self) -> Key {
        self.0.read().expect("endpoint key").clone()
    }
}

struct Conn {
    r: BufReader<TcpStream>,
    w: BufWriter<TcpStream>,
}

impl Conn {
    fn open(addr: SocketAddr, key: &KeyCell) -> Result<(Self, Frame), SinkError> {
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
        stream.set_nodelay(true)?;
        let mut c = Conn {
            r: BufReader::new(stream.try_clone()?),
            w: BufWriter::new(stream),
        };
        write_frame(
            &mut c.w,
            &Frame::Hello {
                client: format!("pubky-music-node/{}", env!("CARGO_PKG_VERSION")),
                version: pm_stream::PROTOCOL_VERSION,
                key: key.get(),
            },
        )
        .map_err(io_err)?;
        let welcome = expect_control(&mut c.r).map_err(io_err)?;
        Ok((c, welcome))
    }
}

/// One output of one endpoint.
pub struct NetOutput {
    addr: SocketAddr,
    key: KeyCell,
    endpoint_name: String,
    remote: OutputInfo,
}

impl NetOutput {
    /// Ask the endpoint at `addr` for its outputs, showing it `key`.
    pub fn connect(addr: impl ToSocketAddrs, key: &KeyCell) -> Result<Vec<NetOutput>, SinkError> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| SinkError::Device("endpoint address does not resolve".into()))?;
        let (_c, welcome) = Conn::open(addr, key)?;
        let Frame::Welcome {
            name,
            version,
            outputs,
        } = welcome
        else {
            return Err(SinkError::Device("endpoint did not send Welcome".into()));
        };
        if version != pm_stream::PROTOCOL_VERSION {
            let update = if version < pm_stream::PROTOCOL_VERSION {
                "update the Pubky Music app"
            } else {
                "update the node"
            };
            return Err(SinkError::Device(format!(
                "endpoint speaks protocol {version}, this node {}; {update}",
                pm_stream::PROTOCOL_VERSION
            )));
        }
        Ok(outputs
            .into_iter()
            .map(|mut remote| {
                // A hw: DAC reports its clock in every Ack; the node keeps it in time
                remote.caps.groupable = remote.id.protocol() == "alsa";
                NetOutput {
                    addr,
                    key: key.clone(),
                    endpoint_name: name.clone(),
                    remote,
                }
            })
            .collect())
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn remote_id(&self) -> &OutputId {
        &self.remote.id
    }
}

#[async_trait]
impl Output for NetOutput {
    fn id(&self) -> OutputId {
        OutputId::new("net", &format!("{}/{}", self.addr, self.remote.id))
    }

    fn name(&self) -> String {
        format!("{} · {}", self.endpoint_name, self.remote.name)
    }

    fn caps(&self) -> &OutputCaps {
        &self.remote.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if !self.remote.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let (mut c, _welcome) = Conn::open(self.addr, &self.key)?;
        write_frame(
            &mut c.w,
            &Frame::Open {
                output: self.remote.id.0.clone(),
                format: fmt,
            },
        )
        .map_err(io_err)?;
        let capacity = match expect_control(&mut c.r).map_err(io_err)? {
            Frame::Opened { buffer_frames } => buffer_frames,
            other => return Err(SinkError::Device(format!("unexpected {other:?} after Open"))),
        };
        Ok(Box::new(NetSink {
            id: self.id(),
            conn: c,
            fmt,
            level: BufferLevel {
                queued_frames: 0,
                capacity_frames: capacity,
            },
            acked: None,
            written: 0,
        }))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        if self.remote.caps.hw_volume.is_none() {
            return Err(SinkError::NoVolume(self.id()));
        }
        let (mut c, _) = Conn::open(self.addr, &self.key)?;
        write_frame(
            &mut c.w,
            &Frame::SetVolume {
                output: self.remote.id.0.clone(),
                volume: v,
            },
        )
        .map_err(io_err)?;
        match expect_control(&mut c.r).map_err(io_err)? {
            Frame::Done => Ok(()),
            other => Err(SinkError::Device(format!("unexpected {other:?}"))),
        }
    }
}

struct NetSink {
    id: OutputId,
    conn: Conn,
    fmt: AudioFormat,
    /// The endpoint's level at its last `Ack`, and when that came.
    level: BufferLevel,
    acked: Option<Instant>,
    written: u64,
}

impl NetSink {
    fn control(&mut self, send: Frame, expect: fn(&Frame) -> bool) -> Result<(), SinkError> {
        write_frame(&mut self.conn.w, &send).map_err(io_err)?;
        let reply = expect_control(&mut self.conn.r).map_err(io_err)?;
        if expect(&reply) {
            Ok(())
        } else {
            Err(SinkError::Device(format!("{}: unexpected {reply:?}", self.id)))
        }
    }
}

impl AudioSink for NetSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        // A block larger than a frame goes out in parts, each acknowledged
        let parts = write_audio(&mut self.conn.w, block).map_err(io_err)?;
        for _ in 0..parts {
            match expect_control(&mut self.conn.r).map_err(io_err)? {
                Frame::Ack {
                    level,
                    frames_written,
                } => {
                    self.level = level;
                    self.acked = Some(Instant::now());
                    self.written = frames_written;
                }
                other => return Err(SinkError::Device(format!("{}: unexpected {other:?}", self.id))),
            }
        }
        Ok(())
    }

    /// What the endpoint holds now: its level at the last `Ack`, less what its device has
    /// played since. A group compares members at one instant, and a member whose `Ack` came
    /// later (a slower network) would otherwise seem ahead by the difference.
    fn buffer_level(&self) -> BufferLevel {
        let played = self.acked.map_or(0, |at| {
            let per_s = match self.fmt.kind {
                SampleKind::Pcm => self.fmt.rate_hz as u128,
                SampleKind::Dsd => self.fmt.rate_hz as u128 / 8,
            };
            (at.elapsed().as_micros() * per_s / 1_000_000) as u64
        });
        BufferLevel {
            queued_frames: self.level.queued_frames.saturating_sub(played),
            ..self.level
        }
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.control(Frame::Flush, |f| matches!(f, Frame::Flushed))?;
        self.level.queued_frames = 0;
        self.acked = None;
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        self.control(Frame::Drain, |f| matches!(f, Frame::Drained))?;
        self.level.queued_frames = 0;
        self.acked = None;
        Ok(())
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}

impl Drop for NetSink {
    fn drop(&mut self) {
        let _ = write_frame(&mut self.conn.w, &Frame::Close);
    }
}
