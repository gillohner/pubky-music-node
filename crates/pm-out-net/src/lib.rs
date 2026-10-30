//! Node-side `Output` for the desktop app's built-in player (`pm-endpoint`) over the `pm-stream`
//! protocol.
//!
//! [`NetOutput::connect`] asks an endpoint for its outputs (one `NetOutput`
//! per remote output, caps copied verbatim); `open` starts a dedicated TCP
//! session whose `write` sends a block without waiting for the endpoint's
//! `Ack` (its own thread collects them), as long as at most [`IN_FLIGHT`] of
//! audio is not acknowledged yet; past that it waits, so the remote DAC paces
//! the zone like a local one, a network round trip later. A group writes its
//! members one after another: waiting for each member's round trip on every
//! block would add them up, and starve the DACs on Wi-Fi. Endpoints are
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
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Audio sent and not acknowledged yet, at most (at least one block is always sent).
pub const IN_FLIGHT: Duration = Duration::from_millis(100);
/// An endpoint that answers nothing for this long is gone.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

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
                remote.caps.groupable = remote.id.0.starts_with("alsa:hw:");
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
        // The endpoint's answers, read on their own thread so a write need not wait for them
        let Conn { mut r, w } = c;
        let stream = w.get_ref().try_clone()?;
        let shared = Arc::new(Answers {
            state: Mutex::new(AckState {
                level: BufferLevel {
                    queued_frames: 0,
                    capacity_frames: capacity,
                },
                at: None,
                written: 0,
                acks: 0,
                replies: std::collections::VecDeque::new(),
                closed: None,
            }),
            changed: Condvar::new(),
        });
        let reader = shared.clone();
        std::thread::Builder::new()
            .name("pm-net-acks".into())
            .spawn(move || loop {
                // Read to the end even after the sink went: closing with answers unread would
                // reset the connection and lose the audio the endpoint has not read yet
                let f = expect_control(&mut r);
                let mut st = reader.state.lock().expect("answers");
                match f {
                    Ok(Frame::Ack {
                        level,
                        frames_written,
                    }) => {
                        // Stamped as it arrives: the level counts down from then
                        st.level = level;
                        st.at = Some(Instant::now());
                        st.written = frames_written;
                        st.acks += 1;
                    }
                    Ok(other) => st.replies.push_back(other),
                    Err(e) => {
                        st.closed = Some(e.to_string());
                        reader.changed.notify_all();
                        break;
                    }
                }
                reader.changed.notify_all();
            })
            .map_err(|e| SinkError::Device(format!("cannot read the endpoint's answers: {e}")))?;
        Ok(Box::new(NetSink {
            id: self.id(),
            w,
            stream,
            answers: shared,
            parts: 0,
            sent: std::collections::VecDeque::new(),
            fmt,
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

/// What the endpoint has answered, kept by the sink's answer thread.
struct AckState {
    /// The endpoint's level at its last `Ack`, and when that came.
    level: BufferLevel,
    at: Option<Instant>,
    /// What the endpoint's sink has written, at its last `Ack`.
    written: u64,
    /// `Ack`s so far: one per audio part.
    acks: u64,
    /// Replies to control frames, in order.
    replies: std::collections::VecDeque<Frame>,
    /// The connection ended (why).
    closed: Option<String>,
}

struct Answers {
    state: Mutex<AckState>,
    changed: Condvar,
}

struct NetSink {
    id: OutputId,
    w: BufWriter<TcpStream>,
    /// The session's socket, to end it when the sink goes.
    stream: TcpStream,
    answers: Arc<Answers>,
    /// Audio parts sent so far.
    parts: u64,
    /// Writes not fully acknowledged, oldest first.
    sent: std::collections::VecDeque<Sent>,
    fmt: AudioFormat,
}

/// One write on its way: its audio parts end at `end` (counting from the session's first), and
/// it has `frames`, `per_part` in each of its parts but the last.
struct Sent {
    end: u64,
    parts: u64,
    frames: u64,
    per_part: u64,
}

impl Sent {
    /// Its frames the endpoint has not acknowledged after `acks` answers.
    fn unacked(&self, acks: u64) -> u64 {
        let done = acks.saturating_sub(self.end - self.parts).min(self.parts);
        if done == self.parts {
            0
        } else {
            self.frames - done * self.per_part
        }
    }
}

impl NetSink {
    /// Audio sent that the endpoint has not acknowledged yet: it is written and held all the
    /// same, a network away. Leaving it out would put the position ahead by that much (a zone
    /// counts from `frames_written` when a track starts, a group from the leader's level).
    fn unacked(&self, st: &AckState) -> u64 {
        self.sent.iter().map(|s| s.unacked(st.acks)).sum()
    }

    fn window(&self) -> u64 {
        let per_s = match self.fmt.kind {
            SampleKind::Pcm => self.fmt.rate_hz as u64,
            SampleKind::Dsd => self.fmt.rate_hz as u64 / 8,
        };
        per_s * IN_FLIGHT.as_millis() as u64 / 1000
    }

    /// Wait on the answers until `done` says so; an ended connection or [`ANSWER_TIMEOUT`]
    /// without a word is an error.
    fn wait<T>(&self, mut done: impl FnMut(&mut AckState) -> Option<T>) -> Result<T, SinkError> {
        let mut st = self.answers.state.lock().expect("answers");
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            if let Some(v) = done(&mut st) {
                return Ok(v);
            }
            if let Some(why) = &st.closed {
                return Err(SinkError::Device(format!("{}: {why}", self.id)));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(SinkError::Device(format!(
                    "{}: the endpoint stopped answering",
                    self.id
                )));
            }
            st = self
                .answers
                .changed
                .wait_timeout(st, deadline - now)
                .expect("answers")
                .0;
        }
    }

    /// Send a control frame and wait for its reply (the `Ack`s of audio sent before it come
    /// first).
    fn control(&mut self, send: Frame, expect: fn(&Frame) -> bool) -> Result<(), SinkError> {
        write_frame(&mut self.w, &send).map_err(io_err)?;
        let reply = self.wait(|st| st.replies.pop_front())?;
        if expect(&reply) {
            Ok(())
        } else {
            Err(SinkError::Device(format!("{}: unexpected {reply:?}", self.id)))
        }
    }

    /// After a flush or drain the endpoint holds nothing.
    fn emptied(&mut self) {
        let mut st = self.answers.state.lock().expect("answers");
        st.level.queued_frames = 0;
        st.at = None;
        self.sent.clear();
    }
}

impl AudioSink for NetSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        // A block larger than a frame goes out in parts, each acknowledged
        let parts = write_audio(&mut self.w, block).map_err(io_err)? as u64;
        self.parts += parts;
        self.sent.push_back(Sent {
            end: self.parts,
            parts,
            frames: block.frames() as u64,
            per_part: pm_stream::frames_per_part(self.fmt),
        });
        // Wait only while too much is unacknowledged (one write is always on its way)
        let window = self.window();
        let mut sent = std::mem::take(&mut self.sent);
        let r = self.wait(|st| {
            while sent.front().is_some_and(|s| s.end <= st.acks) {
                sent.pop_front();
            }
            let in_flight: u64 = sent.iter().map(|s| s.unacked(st.acks)).sum();
            (sent.len() <= 1 || in_flight <= window).then_some(())
        });
        self.sent = sent;
        r
    }

    /// What the endpoint holds now: its level at the last `Ack`, less what its device has
    /// played since, plus what is on its way. A group compares members at one instant, and a
    /// member whose `Ack` came later (a slower network) would otherwise seem ahead by the
    /// difference.
    fn buffer_level(&self) -> BufferLevel {
        let st = self.answers.state.lock().expect("answers");
        let played = st.at.map_or(0, |at| {
            let per_s = match self.fmt.kind {
                SampleKind::Pcm => self.fmt.rate_hz as u128,
                SampleKind::Dsd => self.fmt.rate_hz as u128 / 8,
            };
            (at.elapsed().as_micros() * per_s / 1_000_000) as u64
        });
        BufferLevel {
            queued_frames: st.level.queued_frames.saturating_sub(played) + self.unacked(&st),
            ..st.level
        }
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.control(Frame::Flush, |f| matches!(f, Frame::Flushed))?;
        self.emptied();
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        self.control(Frame::Drain, |f| matches!(f, Frame::Drained))?;
        self.emptied();
        Ok(())
    }

    fn frames_written(&self) -> u64 {
        let st = self.answers.state.lock().expect("answers");
        st.written + self.unacked(&st)
    }
}

impl Drop for NetSink {
    fn drop(&mut self) {
        let _ = write_frame(&mut self.w, &Frame::Close);
        // The endpoint plays what it has, then closes; the answer thread reads to the end
        let _ = self.stream.shutdown(std::net::Shutdown::Write);
    }
}

#[cfg(test)]
mod tests {
    use super::Sent;

    /// A block sent in three parts (parts 5 to 7) counts down part by part as they are answered.
    #[test]
    fn a_write_in_parts_is_acknowledged_part_by_part() {
        let s = Sent {
            end: 7,
            parts: 3,
            frames: 250,
            per_part: 100,
        };
        assert_eq!(
            [3, 4, 5, 6, 7, 8].map(|acks| s.unacked(acks)),
            [250, 250, 150, 50, 0, 0]
        );
    }
}
