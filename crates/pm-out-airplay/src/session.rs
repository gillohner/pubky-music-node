//! One RAOP (AirPlay 1) session: RTSP control over TCP, audio over RTP/UDP.
//!
//! - **RTSP:** OPTIONS, ANNOUNCE (SDP: AppleLossless 44.1 kHz 16-bit stereo, 352 frames per
//!   packet), SETUP (our control and timing ports, the receiver's audio, control and timing ports
//!   back), RECORD, SET_PARAMETER (volume, DMAP text metadata), FLUSH, TEARDOWN.
//! - **Audio:** RTP payload type 96 with verbatim ALAC frames, unencrypted (`et=0`).
//! - **Timing:** the receiver asks for our clock (type 0x52) and we answer (0x53) with NTP times.
//! - **Sync:** once a second we tell it which RTP time is playing now (0x54), so it plays each packet
//!   a fixed latency after we sent it.
//! - **Resend:** the receiver asks for lost packets (0x55); we keep the last few seconds to answer.

use crate::alac::{encode_verbatim, FRAMES_PER_PACKET};
use crate::AirPlayError;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SAMPLE_RATE: u32 = 44_100;
/// The Curve25519 public key sent in `auth-setup`: a fixed one, since the receiver checks nothing.
const AUTH_SETUP_KEY: [u8; 32] = [
    0x59, 0x02, 0xed, 0xe9, 0x0d, 0x4e, 0xf2, 0xbd, 0x4c, 0xb6, 0x8a, 0x63, 0x30, 0x03, 0x82, 0x07, 0xa9,
    0x4d, 0xbd, 0x50, 0xd8, 0xaa, 0x46, 0x5b, 0x5d, 0x8c, 0x01, 0x2a, 0x0c, 0x7e, 0x1d, 0x4e,
];
/// The receiver's playout delay, in frames (two seconds, the AirPlay 1 default).
pub const LATENCY_FRAMES: u32 = 88_200;
/// Packets kept for resend requests (about four seconds).
const RESEND_KEEP: usize = 500;

/// Seconds between 1900 (NTP) and 1970 (Unix).
const NTP_EPOCH: u64 = 2_208_988_800;

fn ntp_now() -> (u32, u32) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = (d.as_secs() + NTP_EPOCH) as u32;
    let frac = ((u64::from(d.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    (secs, frac)
}

/// What the receiver shows (DMAP text).
#[derive(Debug, Clone, Default)]
pub struct Text {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// RTSP over a TCP connection.
struct Rtsp {
    conn: TcpStream,
    reader: BufReader<TcpStream>,
    cseq: u32,
    url: String,
    session: Option<String>,
    instance: String,
    /// When the last request went out: receivers close an RTSP connection that stays quiet.
    last: std::time::Instant,
}

/// An OPTIONS this often while playing keeps the RTSP connection open (receivers such as a
/// Linn's close one that is quiet for about a minute).
const KEEP_ALIVE: Duration = Duration::from_secs(25);

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Rtsp {
    fn request(
        &mut self,
        method: &str,
        extra: &[(&str, String)],
        body: Option<(&str, &[u8])>,
    ) -> Result<Reply, AirPlayError> {
        self.request_to(method, None, extra, body)
    }

    /// A request to `uri` (default: the session's URL, `*` for OPTIONS).
    fn request_to(
        &mut self,
        method: &str,
        uri: Option<&str>,
        extra: &[(&str, String)],
        body: Option<(&str, &[u8])>,
    ) -> Result<Reply, AirPlayError> {
        self.cseq += 1;
        self.last = std::time::Instant::now();
        let uri = if let Some(u) = uri {
            u.to_string()
        } else if method == "OPTIONS" {
            "*".to_string()
        } else {
            self.url.clone()
        };
        let mut req = format!(
            "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: PubkyMusic/1.0\r\nClient-Instance: {}\r\nDACP-ID: {}\r\nActive-Remote: 1\r\n",
            self.cseq, self.instance, self.instance
        );
        if let Some(s) = &self.session {
            req.push_str(&format!("Session: {s}\r\n"));
        }
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some((ct, b)) = body {
            req.push_str(&format!("Content-Type: {ct}\r\nContent-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");
        let sent = self.conn.write_all(req.as_bytes()).and_then(|()| match body {
            Some((_, b)) => self.conn.write_all(b),
            None => Ok(()),
        });
        if let Err(e) = sent {
            return Err(match e.kind() {
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset => {
                    AirPlayError::Closed(format!("{method}: {e}"))
                }
                _ => e.into(),
            });
        }
        let mut status_line = String::new();
        let read = self.reader.read_line(&mut status_line);
        // No answer at all: the receiver hung up (a reset reads as an error, a close as nothing)
        match read {
            Ok(0) => return Err(AirPlayError::Closed(format!("no answer to {method}"))),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Err(AirPlayError::Closed(format!("{method}: {e}")))
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(AirPlayError::Protocol(format!(
                    "the receiver did not answer {method}"
                )))
            }
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| AirPlayError::Protocol(format!("{method}: bad answer {status_line:?}")))?;
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                break;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let reply = Reply { status, headers };
        if let Some(len) = reply
            .header("Content-Length")
            .and_then(|l| l.parse::<usize>().ok())
        {
            let mut skip = vec![0u8; len];
            self.reader.read_exact(&mut skip)?;
        }
        if reply.status != 200 {
            return Err(AirPlayError::Refused(method.into(), reply.status));
        }
        Ok(reply)
    }
}

/// Parse `key=value` pairs of a Transport header.
fn transport_port(t: &str, key: &str) -> Option<u16> {
    t.split(';')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// DMAP: a 4-byte tag, a 4-byte big-endian length, the value.
fn dmap(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + value.len());
    v.extend_from_slice(tag);
    v.extend_from_slice(&(value.len() as u32).to_be_bytes());
    v.extend_from_slice(value);
    v
}

pub fn dmap_text(t: &Text) -> Vec<u8> {
    let mut items = Vec::new();
    if let Some(x) = &t.title {
        items.extend(dmap(b"minm", x.as_bytes()));
    }
    if let Some(x) = &t.artist {
        items.extend(dmap(b"asar", x.as_bytes()));
    }
    if let Some(x) = &t.album {
        items.extend(dmap(b"asal", x.as_bytes()));
    }
    dmap(b"mlit", &items)
}

/// What the background threads share.
struct Shared {
    stop: AtomicBool,
    /// The RTP time of the next packet to send.
    head: AtomicU32,
    /// Recent packets by sequence number, for resends.
    sent: Mutex<VecDeque<(u16, Vec<u8>)>>,
}

pub struct Session {
    rtsp: Rtsp,
    audio: UdpSocket,
    audio_to: SocketAddr,
    control: UdpSocket,
    control_to: SocketAddr,
    seq: u16,
    ssrc: u32,
    first: bool,
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
    /// Interleaved samples waiting for a full packet.
    pending: Vec<i16>,
}

impl Session {
    /// Connect to `addr`, announce, set up and start recording. `auth_setup`: send
    /// `POST /auth-setup` first ([`crate::Device::needs_auth_setup`]).
    pub fn start(addr: SocketAddr, auth_setup: bool) -> Result<Self, AirPlayError> {
        let conn = TcpStream::connect_timeout(&addr, Duration::from_secs(4))?;
        conn.set_read_timeout(Some(Duration::from_secs(8)))?;
        conn.set_nodelay(true)?;
        let local = conn.local_addr()?.ip();
        let sid: u32 = rand::random();
        let instance = format!("{:016X}", rand::random::<u64>());
        let host = match local {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => v6.to_string(),
        };
        let mut rtsp = Rtsp {
            reader: BufReader::new(conn.try_clone()?),
            conn,
            cseq: 0,
            url: format!("rtsp://{host}/{sid}"),
            session: None,
            instance,
            last: std::time::Instant::now(),
        };
        let bind = |ip: IpAddr| UdpSocket::bind(SocketAddr::new(ip, 0));
        let audio = bind(local)?;
        let control = bind(local)?;
        let timing = bind(local)?;
        // The time is answered from the start: AirPlay 2 receivers ask for it during SETUP and
        // RECORD and wait for the answer (the RTP head is set once RECORD names it)
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            head: AtomicU32::new(0),
            sent: Mutex::new(VecDeque::new()),
        });
        let timing_thread = spawn_timing(timing.try_clone()?, shared.clone());
        let started = Self::handshake(&mut rtsp, addr, auth_setup, sid, &host, local, &control, &timing);
        let (server_port, their_control, seq, head) = match started {
            Ok(v) => v,
            Err(e) => {
                shared.stop.store(true, Ordering::Relaxed);
                let _ = timing_thread.join();
                return Err(e);
            }
        };
        shared.head.store(head, Ordering::Relaxed);
        let audio_to = SocketAddr::new(addr.ip(), server_port);
        let control_to = SocketAddr::new(addr.ip(), their_control);
        let threads = vec![
            timing_thread,
            spawn_resend(control.try_clone()?, control_to, shared.clone()),
        ];
        Ok(Self {
            rtsp,
            audio,
            audio_to,
            control,
            control_to,
            seq,
            ssrc: rand::random(),
            first: true,
            shared,
            threads,
            pending: Vec::with_capacity(FRAMES_PER_PACKET * 2),
        })
    }

    /// OPTIONS, auth-setup when asked, ANNOUNCE, SETUP and RECORD. Returns the receiver's audio and
    /// control ports and the first sequence number and RTP time.
    #[allow(clippy::too_many_arguments)]
    fn handshake(
        rtsp: &mut Rtsp,
        addr: SocketAddr,
        auth_setup: bool,
        sid: u32,
        host: &str,
        local: IpAddr,
        control: &UdpSocket,
        timing: &UdpSocket,
    ) -> Result<(u16, u16, u16, u32), AirPlayError> {
        rtsp.request("OPTIONS", &[], None)?;
        let ip_kind = if local.is_ipv4() { "IP4" } else { "IP6" };
        let sdp = format!(
            "v=0\r\no=iTunes {sid} 0 IN {ip_kind} {host}\r\ns=iTunes\r\nc=IN {ip_kind} {}\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\na=fmtp:96 {FRAMES_PER_PACKET} 0 16 40 10 14 2 255 0 0 {SAMPLE_RATE}\r\n",
            addr.ip()
        );
        if auth_setup {
            // 0x01 (no encryption) and a Curve25519 public key; the receiver's answer (its key and
            // MFi signature) is read and not checked
            let mut body = vec![0x01u8];
            body.extend_from_slice(&AUTH_SETUP_KEY);
            if let Err(e) = rtsp.request_to(
                "POST",
                Some("/auth-setup"),
                &[],
                Some(("application/octet-stream", &body)),
            ) {
                tracing::debug!("AirPlay auth-setup: {e}");
            }
        }
        match rtsp.request("ANNOUNCE", &[], Some(("application/sdp", sdp.as_bytes()))) {
            Err(AirPlayError::Refused(_, 403)) => return Err(AirPlayError::Unsupported(
                "this AirPlay speaker refused the stream (403): it wants AirPlay 2 pairing, which the node \
                     does not do yet. Play to it over its other protocol (BluOS, UPnP) if it has one"
                    .into(),
            )),
            r => {
                r?;
            }
        }
        let reply = rtsp.request(
            "SETUP",
            &[(
                "Transport",
                format!(
                    "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
                    control.local_addr()?.port(),
                    timing.local_addr()?.port()
                ),
            )],
            None,
        )?;
        let transport = reply.header("Transport").unwrap_or_default().to_string();
        let server_port = transport_port(&transport, "server_port")
            .ok_or_else(|| AirPlayError::Protocol(format!("SETUP: no server_port in {transport:?}")))?;
        let their_control = transport_port(&transport, "control_port").unwrap_or(server_port + 1);
        rtsp.session = reply
            .header("Session")
            .map(|s| s.split(';').next().unwrap_or(s).to_string());
        let seq: u16 = rand::random();
        let head: u32 = rand::random();
        rtsp.request(
            "RECORD",
            &[
                ("Range", "npt=0-".into()),
                ("RTP-Info", format!("seq={seq};rtptime={head}")),
            ],
            None,
        )?;
        Ok((server_port, their_control, seq, head))
    }

    /// Queue interleaved stereo samples; full packets go out at once. Returns frames sent.
    pub fn push(&mut self, samples: &[i16]) -> Result<usize, AirPlayError> {
        if self.rtsp.last.elapsed() >= KEEP_ALIVE {
            self.rtsp.request("OPTIONS", &[], None)?;
        }
        self.pending.extend_from_slice(samples);
        let mut sent = 0;
        while self.pending.len() >= FRAMES_PER_PACKET * 2 {
            let packet: Vec<i16> = self.pending.drain(..FRAMES_PER_PACKET * 2).collect();
            self.send_packet(&packet)?;
            sent += FRAMES_PER_PACKET;
        }
        Ok(sent)
    }

    /// Send what is left as a short packet (end of a run).
    pub fn flush_pending(&mut self) -> Result<(), AirPlayError> {
        if !self.pending.is_empty() {
            let packet = std::mem::take(&mut self.pending);
            self.send_packet(&packet)?;
        }
        Ok(())
    }

    fn send_packet(&mut self, samples: &[i16]) -> Result<(), AirPlayError> {
        let ts = self.shared.head.load(Ordering::Relaxed);
        if self.first || self.seq.is_multiple_of(126) {
            self.send_sync(ts, self.first)?;
        }
        let mut p = Vec::with_capacity(12 + samples.len() * 2 + 8);
        p.push(0x80);
        p.push(if self.first { 0xe0 } else { 0x60 });
        p.extend_from_slice(&self.seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&self.ssrc.to_be_bytes());
        p.extend(encode_verbatim(samples));
        self.audio.send_to(&p, self.audio_to)?;
        {
            let mut sent = self.shared.sent.lock().expect("sent lock");
            sent.push_back((self.seq, p));
            if sent.len() > RESEND_KEEP {
                sent.pop_front();
            }
        }
        self.first = false;
        self.seq = self.seq.wrapping_add(1);
        self.shared
            .head
            .store(ts.wrapping_add((samples.len() / 2) as u32), Ordering::Relaxed);
        Ok(())
    }

    /// Tell the receiver that RTP time `ts` is being sent now.
    fn send_sync(&self, ts: u32, first: bool) -> Result<(), AirPlayError> {
        let (s, f) = ntp_now();
        let mut p = vec![if first { 0x90 } else { 0x80 }, 0xd4, 0x00, 0x07];
        p.extend_from_slice(&ts.wrapping_sub(LATENCY_FRAMES).to_be_bytes());
        p.extend_from_slice(&s.to_be_bytes());
        p.extend_from_slice(&f.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        self.control.send_to(&p, self.control_to)?;
        Ok(())
    }

    /// Volume in dB (-30 = quietest, 0 = full), or mute (-144).
    pub fn set_volume_db(&mut self, db: f32) -> Result<(), AirPlayError> {
        let body = format!("volume: {db:.6}\r\n");
        self.rtsp
            .request("SET_PARAMETER", &[], Some(("text/parameters", body.as_bytes())))?;
        Ok(())
    }

    pub fn set_text(&mut self, t: &Text) -> Result<(), AirPlayError> {
        let ts = self.shared.head.load(Ordering::Relaxed);
        let body = dmap_text(t);
        self.rtsp.request(
            "SET_PARAMETER",
            &[("RTP-Info", format!("rtptime={ts}"))],
            Some(("application/x-dmap-tagged", &body)),
        )?;
        Ok(())
    }

    /// Drop what the receiver buffered (seek, pause).
    pub fn flush(&mut self) -> Result<(), AirPlayError> {
        self.pending.clear();
        let ts = self.shared.head.load(Ordering::Relaxed);
        self.rtsp.request(
            "FLUSH",
            &[("RTP-Info", format!("seq={};rtptime={ts}", self.seq))],
            None,
        )?;
        self.first = true;
        Ok(())
    }

    pub fn teardown(mut self) {
        let _ = self.rtsp.request("TEARDOWN", &[], None);
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Answer the receiver's clock questions.
fn spawn_timing(sock: UdpSocket, shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(300)));
    std::thread::spawn(move || {
        let mut buf = [0u8; 128];
        while !shared.stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if n < 32 || buf[1] & 0x7f != 0x52 {
                continue;
            }
            let (s, f) = ntp_now();
            let mut p = vec![0x80, 0xd3, 0x00, 0x07, 0, 0, 0, 0];
            p.extend_from_slice(&buf[24..32]); // their send time becomes our origin
            for _ in 0..2 {
                p.extend_from_slice(&s.to_be_bytes());
                p.extend_from_slice(&f.to_be_bytes());
            }
            let _ = sock.send_to(&p, from);
        }
    })
}

/// Resend lost packets the receiver asks for; answers go to its control port.
fn spawn_resend(
    control: UdpSocket,
    control_to: SocketAddr,
    shared: Arc<Shared>,
) -> std::thread::JoinHandle<()> {
    let _ = control.set_read_timeout(Some(Duration::from_millis(300)));
    std::thread::spawn(move || {
        let mut buf = [0u8; 128];
        while !shared.stop.load(Ordering::Relaxed) {
            let Ok((n, _)) = control.recv_from(&mut buf) else {
                continue;
            };
            if n < 8 || buf[1] & 0x7f != 0x55 {
                continue;
            }
            let first = u16::from_be_bytes([buf[4], buf[5]]);
            let count = u16::from_be_bytes([buf[6], buf[7]]);
            let sent = shared.sent.lock().expect("sent lock");
            for i in 0..count {
                let want = first.wrapping_add(i);
                if let Some((_, p)) = sent.iter().find(|(s, _)| *s == want) {
                    // A resent packet travels inside a control packet (type 0x56).
                    let mut r = vec![0x80, 0xd6, 0x00, 0x01];
                    r.extend_from_slice(p);
                    let _ = control.send_to(&r, control_to);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A receiver that says nothing more than RTSP needs: 403 on ANNOUNCE unless auth-setup came
    /// first (an AirPlay 2 speaker), and it hangs up on FLUSH (a Linn after a pause).
    fn fake_receiver(log: Arc<Mutex<Vec<String>>>) -> SocketAddr {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(conn) = conn else { break };
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    let mut w = conn;
                    let mut authed = false;
                    loop {
                        let mut first = String::new();
                        if r.read_line(&mut first).unwrap_or(0) == 0 {
                            break;
                        }
                        let (mut len, mut cseq) = (0usize, String::new());
                        loop {
                            let mut h = String::new();
                            r.read_line(&mut h).unwrap();
                            let h = h.trim_end().to_string();
                            if h.is_empty() {
                                break;
                            }
                            if let Some(v) = h.strip_prefix("Content-Length: ") {
                                len = v.parse().unwrap();
                            }
                            if let Some(v) = h.strip_prefix("CSeq: ") {
                                cseq = v.to_string();
                            }
                        }
                        let mut body = vec![0u8; len];
                        r.read_exact(&mut body).unwrap();
                        let method = first.split_whitespace().next().unwrap_or("").to_string();
                        let target = first.split_whitespace().nth(1).unwrap_or("").to_string();
                        log.lock().unwrap().push(format!("{method} {target}"));
                        let (status, extra) = match method.as_str() {
                            "POST" if target == "/auth-setup" => {
                                authed = body.len() == 33 && body[0] == 1;
                                ("200 OK", String::new())
                            }
                            "ANNOUNCE" if !authed => ("403 Forbidden", String::new()),
                            "SETUP" => (
                                "200 OK",
                                "Transport: RTP/AVP/UDP;unicast;mode=record;server_port=6000;control_port=6001;timing_port=6002\r\nSession: 1\r\n".to_string(),
                            ),
                            "FLUSH" => return, // hangs up
                            _ => ("200 OK", String::new()),
                        };
                        write!(w, "RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n{extra}\r\n").unwrap();
                    }
                });
            }
        });
        addr
    }

    #[test]
    fn an_airplay_2_speaker_takes_the_stream_after_auth_setup_and_a_hang_up_is_said_plainly() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let addr = fake_receiver(log.clone());
        // Without auth-setup: the zone's error says what is missing
        let e = Session::start(addr, false).err().expect("refused");
        assert!(e.to_string().contains("AirPlay 2 pairing"), "{e}");
        // With it: the session starts
        let mut s = Session::start(addr, true).expect("auth-setup, then ANNOUNCE");
        let calls = log.lock().unwrap().clone();
        let at = calls
            .iter()
            .position(|c| c == "POST /auth-setup")
            .expect("auth-setup sent");
        assert!(calls[at + 1].starts_with("ANNOUNCE"), "{calls:?}");
        // The receiver hangs up on FLUSH: a plain "closed", not a protocol error
        assert!(matches!(s.flush(), Err(AirPlayError::Closed(_))));
    }

    #[test]
    fn transport_and_dmap() {
        let t = "RTP/AVP/UDP;unicast;mode=record;server_port=6000;control_port=6001;timing_port=6002";
        assert_eq!(transport_port(t, "server_port"), Some(6000));
        assert_eq!(transport_port(t, "control_port"), Some(6001));
        assert_eq!(transport_port(t, "nope"), None);
        let d = dmap_text(&Text {
            title: Some("Ab".into()),
            artist: None,
            album: None,
        });
        assert_eq!(d, b"mlit\0\0\0\x0aminm\0\0\0\x02Ab".to_vec());
    }
}
