//! Node-side `Output` for a `pm-endpoint` over the `pm-stream` protocol.
//!
//! [`NetOutput::connect`] asks an endpoint for its outputs (one `NetOutput`
//! per remote output, caps copied verbatim); `open` starts a dedicated TCP
//! session whose `write` sends a block and waits for the endpoint's `Ack`,
//! so the remote DAC paces the zone exactly like a local one. Discovery is
//! mDNS ([`discover`]) or configured addresses.

use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, Volume};
use pm_stream::{expect_control, write_audio, write_frame, Frame, StreamError, SERVICE_TYPE};
use pm_zones::sink::check_block;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, OutputInfo, SinkError};
use std::io::{BufReader, BufWriter};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

fn io_err(e: StreamError) -> SinkError {
    SinkError::Device(e.to_string())
}

struct Conn {
    r: BufReader<TcpStream>,
    w: BufWriter<TcpStream>,
}

impl Conn {
    fn open(addr: SocketAddr) -> Result<(Self, Frame), SinkError> {
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
    endpoint_name: String,
    remote: OutputInfo,
}

impl NetOutput {
    /// Ask the endpoint at `addr` for its outputs.
    pub fn connect(addr: impl ToSocketAddrs) -> Result<Vec<NetOutput>, SinkError> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| SinkError::Device("endpoint address does not resolve".into()))?;
        let (_c, welcome) = Conn::open(addr)?;
        let Frame::Welcome {
            name,
            version,
            outputs,
        } = welcome
        else {
            return Err(SinkError::Device("endpoint did not send Welcome".into()));
        };
        if version != pm_stream::PROTOCOL_VERSION {
            return Err(SinkError::Device(format!(
                "endpoint speaks protocol {version}, this node {}",
                pm_stream::PROTOCOL_VERSION
            )));
        }
        Ok(outputs
            .into_iter()
            .map(|remote| NetOutput {
                addr,
                endpoint_name: name.clone(),
                remote,
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

    fn device(&self) -> Option<pm_zones::OutputDevice> {
        self.remote.device.clone()
    }

    fn caps(&self) -> &OutputCaps {
        &self.remote.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if !self.remote.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let (mut c, _welcome) = Conn::open(self.addr)?;
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
            written: 0,
        }))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        if self.remote.caps.hw_volume.is_none() {
            return Err(SinkError::NoVolume(self.id()));
        }
        let (mut c, _) = Conn::open(self.addr)?;
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
    level: BufferLevel,
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
        write_audio(&mut self.conn.w, block).map_err(io_err)?;
        match expect_control(&mut self.conn.r).map_err(io_err)? {
            Frame::Ack {
                level,
                frames_written,
            } => {
                self.level = level;
                self.written = frames_written;
                Ok(())
            }
            other => Err(SinkError::Device(format!("{}: unexpected {other:?}", self.id))),
        }
    }

    fn buffer_level(&self) -> BufferLevel {
        self.level
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.control(Frame::Flush, |f| matches!(f, Frame::Flushed))?;
        self.level.queued_frames = 0;
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        self.control(Frame::Drain, |f| matches!(f, Frame::Drained))?;
        self.level.queued_frames = 0;
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

/// Endpoints found over mDNS within `timeout`: `(name, address)`.
pub fn discover(timeout: Duration) -> Vec<(String, SocketAddr)> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(rx) = daemon.browse(SERVICE_TYPE) else {
        return Vec::new();
    };
    let deadline = std::time::Instant::now() + timeout;
    let mut found: Vec<(String, SocketAddr)> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                // Nodes announce on the same service type (pm-discovery).
                if info.get_property_val_str("role") == Some("node") {
                    continue;
                }
                let name = info
                    .get_property_val_str("name")
                    .map(str::to_string)
                    .unwrap_or_else(|| info.get_fullname().to_string());
                for ip in info.get_addresses() {
                    let addr = SocketAddr::new(ip.to_ip_addr(), info.get_port());
                    if !found.iter().any(|(_, a)| *a == addr) {
                        found.push((name.clone(), addr));
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    found
}
