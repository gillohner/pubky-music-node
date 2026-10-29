//! A connected Slimproto player as an `Output`. One per player (MAC) for the
//! node's lifetime: a player that reconnects (Wi-Fi drop, restart) gets its
//! new connection attached to the same output, so its zone keeps working.

use crate::{audg, strm, write_frame, Hello, Stat, FIXED_ONE};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Volume, VolumeKind};
use pm_zones::outputs::PullOutput;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, SinkError};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

/// The last `STAT` we saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlayerStatus {
    pub stream_buffer_size: u32,
    pub stream_buffer_fullness: u32,
    pub output_buffer_size: u32,
    pub output_buffer_fullness: u32,
    pub elapsed_ms: u32,
    pub jiffies: u32,
    pub last_event: [u8; 4],
    /// When this report arrived.
    pub at: Option<std::time::Instant>,
    /// The player has buffered the stream and waits for `strm u` (`STMl`, autostart off).
    pub loaded: bool,
}

impl PlayerStatus {
    /// Milliseconds played of the current stream now: the report's, plus the
    /// time since it while the player is running.
    pub fn played_ms(&self) -> u64 {
        // Not STMr: it answers `strm u`, which may start the output only later (start at).
        let running = matches!(&self.last_event, b"STMt" | b"STMs" | b"STMo");
        let since = match (running, self.at) {
            (true, Some(at)) => at.elapsed().as_millis() as u64,
            _ => 0,
        };
        self.elapsed_ms as u64 + since
    }
}

/// The player's current connection and what it last reported, shared by the
/// output and its open sink.
#[derive(Default)]
pub(crate) struct Link {
    /// The control connection's write side and its number (`attach`).
    conn: Mutex<Option<(u64, TcpStream)>>,
    generation: std::sync::atomic::AtomicU64,
    status: Mutex<PlayerStatus>,
    /// False after `reset` until the player confirms it dropped its old stream (`STMf`): reports
    /// already on the way describe the old one.
    fresh: std::sync::atomic::AtomicBool,
    /// The last volume set (16.16), sent again on every connection: squeezelite starts at gain 0
    /// and plays silence until it gets an `audg`.
    gain: Mutex<Option<u32>>,
}

impl Link {
    /// A new connection replaces the old one; returns its number for `detach`.
    pub(crate) fn attach(&self, stream: TcpStream) -> u64 {
        let g = self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        *self.conn.lock().expect("conn") = Some((g, stream));
        g
    }

    /// Connection `g` ended. A later connection that is already attached stays.
    pub(crate) fn detach(&self, g: u64) {
        let mut c = self.conn.lock().expect("conn");
        if c.as_ref().map(|(cur, _)| *cur) == Some(g) {
            *c = None;
        }
    }

    fn connected(&self) -> bool {
        self.conn.lock().expect("conn").is_some()
    }

    /// A status request (`strm t`, with a timestamp the player echoes) on connection `g`, if it
    /// is still the current one. Players count a server silent for 35 s as gone (squeezelite:
    /// "No messages from server - connection dead"), and LMS asks every few seconds. False once
    /// `g` is replaced or ended.
    pub(crate) fn heartbeat(&self, g: u64, timestamp_ms: u32) -> bool {
        let mut c = self.conn.lock().expect("conn");
        match c.as_mut() {
            Some((cur, w)) if *cur == g => {
                write_frame(w, b"strm", &strm(b't', b'0', 16, 44_100, 2, 0, timestamp_ms, "")).is_ok()
            }
            _ => false,
        }
    }

    fn send(&self, op: &[u8; 4], data: &[u8]) -> std::io::Result<()> {
        let mut c = self.conn.lock().expect("conn");
        let Some((_, w)) = c.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "player disconnected",
            ));
        };
        write_frame(w, op, data)
    }

    fn status(&self) -> PlayerStatus {
        *self.status.lock().expect("status")
    }

    /// A new stream starts: forget what the player said about the old one.
    fn reset(&self) {
        *self.status.lock().expect("status") = PlayerStatus::default();
        self.fresh.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub struct SlimOutput {
    hello: Hello,
    link: Arc<Link>,
    server_port: u16,
    stream_path: Arc<dyn Fn(&str) -> String + Send + Sync>,
    pull: Arc<PullOutput>,
    caps: OutputCaps,
}

impl SlimOutput {
    pub fn new(
        hello: Hello,
        server_port: u16,
        stream_path: Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) -> Self {
        let max = hello.max_sample_rate();
        let mut caps = OutputCaps::unlimited();
        caps.pcm_rates = [
            44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
        ]
        .into_iter()
        .filter(|r| *r <= max && crate::rate_code(*r).is_some())
        .collect();
        caps.max_pcm_bits = 24;
        caps.pcm_bits = vec![16, 24];
        caps.max_channels = 2;
        caps.dsd = None;
        caps.hw_volume = Some(VolumeKind::Db {
            min: -60.0,
            max: 0.0,
            step: 0.5,
        });
        caps.standby = false;
        caps.gapless = true;
        caps.groupable = true;
        caps.fixed_latency_ms = Some(400);
        caps.quality_ceiling = QualityCeiling::BitPerfect;
        let name = format!("slim-{}", hello.mac_string().replace(':', ""));
        Self {
            hello,
            link: Arc::new(Link::default()),
            server_port,
            stream_path,
            pull: Arc::new(PullOutput::unpaced(&name)),
            caps,
        }
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// The pull output the player fetches; register it with the HTTP server.
    pub fn pull(&self) -> &Arc<PullOutput> {
        &self.pull
    }

    pub fn status(&self) -> PlayerStatus {
        self.link.status()
    }

    /// No connection right now (it may come back: the output stays).
    pub fn is_gone(&self) -> bool {
        !self.link.connected()
    }

    pub(crate) fn link(&self) -> &Arc<Link> {
        &self.link
    }

    /// A new connection: the DAC on, and the volume (full until one is set).
    pub(crate) fn greet(&self) -> Result<(), SinkError> {
        self.send(b"aude", &[1, 1])?;
        let g = self.link.gain.lock().expect("gain").unwrap_or(FIXED_ONE);
        self.send(b"audg", &audg(g))
    }

    pub(crate) fn on_stat(&self, st: Stat) {
        tracing::debug!(event = %String::from_utf8_lossy(&st.event), elapsed_ms = st.elapsed_ms, jiffies = st.jiffies,
            output_full = st.output_buffer_fullness, stream_full = st.stream_buffer_fullness, "STAT");
        use std::sync::atomic::Ordering::Relaxed;
        if !self.link.fresh.load(Relaxed) {
            if &st.event != b"STMf" {
                return;
            }
            self.link.fresh.store(true, Relaxed);
        }
        let mut s = self.link.status.lock().expect("status");
        s.stream_buffer_size = st.stream_buffer_size;
        s.stream_buffer_fullness = st.stream_buffer_fullness;
        s.output_buffer_size = st.output_buffer_size;
        s.output_buffer_fullness = st.output_buffer_fullness;
        s.elapsed_ms = st.elapsed_ms;
        s.jiffies = st.jiffies;
        s.last_event = st.event;
        s.at = Some(std::time::Instant::now());
        if &st.event == b"STMl" {
            s.loaded = true;
        }
    }

    pub(crate) fn send(&self, op: &[u8; 4], data: &[u8]) -> Result<(), SinkError> {
        self.link.send(op, data).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotConnected => {
                SinkError::Unavailable(self.id(), "player disconnected".into())
            }
            _ => SinkError::Device(format!("{}: {e}", self.id())),
        })
    }
}

#[async_trait]
impl Output for SlimOutput {
    fn id(&self) -> OutputId {
        OutputId::new("slim", &self.hello.mac_string())
    }

    fn name(&self) -> String {
        format!("{} ({})", self.hello.model(), self.hello.mac_string())
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let inner = self.pull.open(fmt).await?;
        // Raw PCM: squeezelite plays a WAV header as audio (a click) unless started with -W.
        let path = (self.stream_path)(self.pull.stream_name()).replacen("/stream.wav", "/stream.pcm", 1);
        let request = format!("GET {path} HTTP/1.0\r\n\r\n");
        let start = strm(
            b's',
            b'1',
            fmt.bits,
            fmt.rate_hz,
            fmt.channels,
            self.server_port,
            0,
            &request,
        );
        if !self.link.connected() {
            return Err(SinkError::Unavailable(self.id(), "player disconnected".into()));
        }
        self.link.reset();
        // `strm s` goes with the first write, so a group can hold the start first.
        Ok(Box::new(SlimSink {
            inner: Some(inner),
            link: self.link.clone(),
            pull: self.pull.clone(),
            start,
            started: false,
            fmt,
            written: 0,
            base: 0,
        }))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        let gain = match v {
            Volume::Db(db) => 10f64.powf(db.clamp(-60.0, 0.0) as f64 / 20.0),
            Volume::Percent(p) => (p.min(100) as f64 / 100.0).powi(2),
            Volume::Mute(true) => 0.0,
            Volume::Mute(false) => 1.0,
            Volume::Up | Volume::Down => {
                return Err(SinkError::Device(
                    "slimproto volume needs an absolute level".into(),
                ));
            }
        };
        let g = (gain * FIXED_ONE as f64).round() as u32;
        *self.link.gain.lock().expect("gain") = Some(g);
        self.send(b"audg", &audg(g))
    }
}

struct SlimSink {
    /// The HTTP stream the player fetches; dropped at `drain` so the player sees its end.
    inner: Option<Box<dyn AudioSink>>,
    link: Arc<Link>,
    pull: Arc<PullOutput>,
    /// The `strm s` that starts the player on the stream (sent again after a flush). Its
    /// autostart byte is `'0'` once the sink is held.
    start: Vec<u8>,
    /// `start` was sent for the current stream.
    started: bool,
    fmt: AudioFormat,
    written: u64,
    /// Frames written before the player's current stream started (it counts from zero).
    base: u64,
}

/// How far the node runs ahead of what the player has played. Socket and player buffers would
/// take a minute or more at once; this keeps the position live, `drain` short, and bounds what
/// a flush throws away.
const AHEAD: std::time::Duration = std::time::Duration::from_secs(5);
/// A player that plays nothing for this long while audio waits has stopped taking it.
const STALLED: std::time::Duration = std::time::Duration::from_secs(30);
/// How long `drain` waits beyond the audio still queued in the player.
const DRAIN_SLACK: std::time::Duration = std::time::Duration::from_secs(10);

impl SlimSink {
    fn command(&self, c: u8) {
        let _ = self.link.send(
            b"strm",
            &strm(
                c,
                b'0',
                self.fmt.bits,
                self.fmt.rate_hz,
                self.fmt.channels,
                0,
                0,
                "",
            ),
        );
    }

    fn played_frames(&self) -> u64 {
        let ms = self.link.status().played_ms();
        (self.base + ms * self.fmt.rate_hz as u64 / 1000).min(self.written)
    }

    fn ahead_frames(&self) -> u64 {
        self.written - self.played_frames()
    }

    /// Wait while the player has more than `AHEAD` to play.
    fn pace(&self) -> Result<(), SinkError> {
        let limit = AHEAD.as_millis() as u64 * self.fmt.rate_hz as u64 / 1000;
        let mut last = (self.played_frames(), std::time::Instant::now());
        while self.ahead_frames() > limit {
            if !self.link.connected() {
                return Err(SinkError::Device("the player disconnected".into()));
            }
            let played = self.played_frames();
            if played != last.0 {
                last = (played, std::time::Instant::now());
            } else if last.1.elapsed() > STALLED {
                return Err(SinkError::Device(format!(
                    "the player played nothing for {} s",
                    STALLED.as_secs()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Ok(())
    }
}

impl AudioSink for SlimSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }
    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        if !self.started {
            self.link
                .send(b"strm", &self.start)
                .map_err(|e| SinkError::Device(format!("start the player's stream: {e}")))?;
            self.started = true;
        }
        self.pace()?;
        match self.inner.as_mut() {
            Some(inner) => inner.write(block)?,
            None => return Err(SinkError::Device("the stream has ended".into())),
        }
        self.written += block.frames() as u64;
        Ok(())
    }
    fn buffer_level(&self) -> BufferLevel {
        // Everything sent and not yet played, by the player's own count (STAT every second
        // while playing, interpolated in between): its stream and output buffers together.
        BufferLevel {
            queued_frames: self.ahead_frames(),
            capacity_frames: AHEAD.as_millis() as u64 * self.fmt.rate_hz as u64 / 1000,
        }
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        // Stop at once and drop everything queued (`q`; `f` would only drop the next track),
        // then start the player on a new stream for what comes next.
        self.command(b'q');
        self.inner = None;
        self.inner = Some(self.pull.open_now(self.fmt)?);
        self.link.reset();
        self.base = self.written;
        // The next write starts the player on the new stream.
        self.started = false;
        Ok(())
    }
    fn drain(&mut self) -> Result<(), SinkError> {
        // End the HTTP stream so the player decodes to the end, then wait until it has played
        // everything out (STMu: stream over, output empty).
        if let Some(mut inner) = self.inner.take() {
            inner.drain()?;
        }
        let left = self.ahead_frames();
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(left * 1000 / self.fmt.rate_hz.max(1) as u64)
            + DRAIN_SLACK;
        while std::time::Instant::now() < deadline {
            let st = self.link.status();
            if &st.last_event == b"STMu" || !self.link.connected() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Ok(())
    }
    fn frames_written(&self) -> u64 {
        self.written
    }
    fn hold(&mut self) -> bool {
        // Autostart off: the player buffers, reports STMl, and waits for `strm u`.
        self.start[1] = b'0';
        true
    }
    fn ready(&self) -> bool {
        self.link.status().loaded
    }
    fn start_at(&mut self, at: std::time::Instant) -> Result<(), SinkError> {
        // `at` on the player's own millisecond clock: its last report's jiffies plus the time
        // since that report arrived (the network delay of one LAN packet is left out).
        let st = self.link.status();
        let jiffies = match st.at {
            Some(seen) => {
                let ms = match at.checked_duration_since(seen) {
                    Some(d) => d.as_millis() as i64,
                    None => -(seen.duration_since(at).as_millis() as i64),
                };
                (st.jiffies as i64 + ms).max(1) as u32
            }
            // No report yet: start at once (0 = now).
            None => 0,
        };
        self.link
            .send(
                b"strm",
                &strm(
                    b'u',
                    b'0',
                    self.fmt.bits,
                    self.fmt.rate_hz,
                    self.fmt.channels,
                    0,
                    jiffies,
                    "",
                ),
            )
            .map_err(|e| SinkError::Device(format!("start the player: {e}")))
    }
    fn nudge(&mut self, frames: i64) -> bool {
        // Ahead: play `p` ms of silence (not counted as played). Behind: skip `a` ms (counted).
        let (cmd, ms) = if frames > 0 {
            (b'p', self.ms(frames as u64))
        } else {
            (b'a', self.ms(frames.unsigned_abs()))
        };
        if ms == 0 {
            return true;
        }
        let sent = self.link.send(
            b"strm",
            &strm(
                cmd,
                b'0',
                self.fmt.bits,
                self.fmt.rate_hz,
                self.fmt.channels,
                0,
                ms,
                "",
            ),
        );
        tracing::debug!(cmd = %(cmd as char), ms, ok = sent.is_ok(), "slimproto nudge");
        sent.is_ok()
    }
}

impl SlimSink {
    fn ms(&self, frames: u64) -> u32 {
        ((frames * 1000 + self.fmt.rate_hz as u64 / 2) / self.fmt.rate_hz.max(1) as u64) as u32
    }
}

impl Drop for SlimSink {
    fn drop(&mut self) {
        self.command(b'q');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_frame_server;
    use std::io::Write;
    use std::net::TcpListener;

    /// The next frame from the server that is not a heartbeat (`strm t`).
    fn next_frame(player: &mut TcpStream) -> std::io::Result<([u8; 4], Vec<u8>)> {
        loop {
            let (op, data) = read_frame_server(player)?;
            if !(&op == b"strm" && data.first() == Some(&b't')) {
                return Ok((op, data));
            }
        }
    }

    /// An idle player still hears from the server (squeezelite gives up after 35 s of silence).
    #[test]
    fn idle_players_get_heartbeats() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let on_player: Arc<dyn Fn(Arc<SlimOutput>) + Send + Sync> = Arc::new(|_| {});
        let stream_path: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(|n| n.to_string());
        std::thread::spawn(move || crate::serve(listener, 7788, stream_path, on_player).unwrap());
        let mut player = TcpStream::connect(addr).unwrap();
        let mut hello = vec![12u8, 0, 0xaa, 0xbb, 0xcc, 0x00, 0x11, 0x33];
        hello.extend_from_slice(&[0u8; 28]);
        player.write_all(b"HELO").unwrap();
        player.write_all(&(hello.len() as u32).to_be_bytes()).unwrap();
        player.write_all(&hello).unwrap();
        player.set_read_timeout(Some(crate::HEARTBEAT * 5)).unwrap();
        let mut beats = Vec::new();
        while beats.len() < 2 {
            let (op, data) = read_frame_server(&mut player).unwrap();
            if &op == b"strm" && data[0] == b't' {
                beats.push(u32::from_be_bytes([data[14], data[15], data[16], data[17]]));
            }
        }
        assert!(beats[1] > beats[0], "the timestamp moves on: {beats:?}");
    }

    /// A fake squeezelite: connects, sends HELO, then lets the test read
    /// what the server sends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn player_is_told_to_fetch_the_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Arc<SlimOutput>>();
        let on_player: Arc<dyn Fn(Arc<SlimOutput>) + Send + Sync> = Arc::new(move |o| {
            let _ = tx.send(o);
        });
        let stream_path: Arc<dyn Fn(&str) -> String + Send + Sync> =
            Arc::new(|name| format!("/v1/outputs/pull/{name}/stream.wav?token=t"));
        std::thread::spawn(move || crate::serve(listener, 7788, stream_path, on_player).unwrap());

        let mut player = TcpStream::connect(addr).unwrap();
        let mut hello = vec![12u8, 0, 0xaa, 0xbb, 0xcc, 0x00, 0x11, 0x22];
        hello.extend_from_slice(&[0u8; 28]);
        hello.extend_from_slice(b"Model=squeezelite,ModelName=SqueezeLite,MaxSampleRate=96000,pcm,flc");
        player.write_all(b"HELO").unwrap();
        player.write_all(&(hello.len() as u32).to_be_bytes()).unwrap();
        player.write_all(&hello).unwrap();
        let out = rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        assert_eq!(out.id().0, "slim:aa:bb:cc:00:11:22");
        assert!(out.name().starts_with("SqueezeLite"));
        assert_eq!(out.caps().pcm_rates, vec![44_100, 48_000, 88_200, 96_000]);
        // aude first.
        let (op, data) = next_frame(&mut player).unwrap();
        assert_eq!(&op, b"aude");
        assert_eq!(data, vec![1, 1]);
        // Full volume: squeezelite plays silence until it gets one.
        let (op, data) = next_frame(&mut player).unwrap();
        assert_eq!(&op, b"audg");
        assert_eq!(&data[10..14], &FIXED_ONE.to_be_bytes());

        let fmt = AudioFormat::pcm(48_000, 24, 2);
        let mut sink = out.open(fmt).await.unwrap();
        // The player is told to fetch with the first write (a group may hold it first).
        let block = AudioBlock::pcm_i32(fmt, vec![1, -1]);
        let writer = std::thread::spawn(move || {
            sink.write(&block).unwrap();
            drop(sink);
        });
        let (op, data) = next_frame(&mut player).unwrap();
        assert_eq!(&op, b"strm");
        assert_eq!(&data[0..7], b"s1p2421");
        assert_eq!(&data[18..20], &7788u16.to_be_bytes());
        let req = String::from_utf8_lossy(&data[24..]).to_string();
        assert_eq!(
            req,
            "GET /v1/outputs/pull/slim-aabbcc001122/stream.pcm?token=t HTTP/1.0\r\n\r\n"
        );

        // The "player" fetches: take the pull stream and read the block.
        let mut live = out
            .pull()
            .take_stream(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        // PCM only (`stream.pcm` serves it as is)
        let pcm = live.next_chunk().await.unwrap();
        assert_eq!(pcm, vec![1, 0, 0, 0xff, 0xff, 0xff]);
        writer.join().unwrap();
        let (op, data) = next_frame(&mut player).unwrap();
        assert_eq!(&op, b"strm");
        assert_eq!(data[0], b'q');

        // Volume: -6 dB ≈ 0.501 in 16.16.
        out.set_volume(Volume::Db(-6.0)).await.unwrap();
        let (op, data) = next_frame(&mut player).unwrap();
        assert_eq!(&op, b"audg");
        let g = u32::from_be_bytes([data[10], data[11], data[12], data[13]]);
        assert!((g as f64 / FIXED_ONE as f64 - 0.501).abs() < 0.01, "{g}");

        // STAT updates the status, once the player has confirmed the new stream (STMf):
        // a report already on its way describes the old one.
        let stat = |event: &[u8; 4], elapsed_ms: u32| {
            let mut st = Vec::new();
            st.extend_from_slice(event);
            st.extend_from_slice(&[0, 0, 0]);
            st.extend_from_slice(&1000u32.to_be_bytes());
            st.extend_from_slice(&500u32.to_be_bytes());
            st.extend_from_slice(&[0u8; 8]);
            st.extend_from_slice(&0xffffu16.to_be_bytes());
            st.extend_from_slice(&1u32.to_be_bytes());
            st.extend_from_slice(&4000u32.to_be_bytes());
            st.extend_from_slice(&2000u32.to_be_bytes());
            st.extend_from_slice(&(elapsed_ms / 1000).to_be_bytes());
            st.extend_from_slice(&0u16.to_be_bytes());
            st.extend_from_slice(&elapsed_ms.to_be_bytes());
            st.extend_from_slice(&0u32.to_be_bytes());
            st.extend_from_slice(&0u16.to_be_bytes());
            let mut f = b"STAT".to_vec();
            f.extend_from_slice(&(st.len() as u32).to_be_bytes());
            f.extend_from_slice(&st);
            f
        };
        player.write_all(&stat(b"STMt", 99_000)).unwrap();
        player.write_all(&stat(b"STMf", 0)).unwrap();
        player.write_all(&stat(b"STMt", 7500)).unwrap();
        for _ in 0..100 {
            if out.status().elapsed_ms == 7500 {
                break;
            }
            assert_ne!(
                out.status().elapsed_ms,
                99_000,
                "a report from before the new stream counted"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(out.status().elapsed_ms, 7500);
        assert_eq!(out.status().output_buffer_fullness, 2000);
        drop(player);
        for _ in 0..100 {
            if out.is_gone() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(out.is_gone());
        assert!(matches!(
            out.set_volume(Volume::Db(0.0)).await,
            Err(SinkError::Unavailable(..))
        ));

        // The player comes back (Wi-Fi drop, restart): same output, not announced again,
        // and commands reach the new connection.
        let mut again = TcpStream::connect(addr).unwrap();
        again.write_all(b"HELO").unwrap();
        again.write_all(&(hello.len() as u32).to_be_bytes()).unwrap();
        again.write_all(&hello).unwrap();
        let (op, _) = next_frame(&mut again).unwrap();
        assert_eq!(&op, b"aude");
        // The last volume set (0 dB, while it was away) comes back with the connection.
        let (op, data) = next_frame(&mut again).unwrap();
        assert_eq!(&op, b"audg");
        assert_eq!(&data[10..14], &FIXED_ONE.to_be_bytes());
        assert!(!out.is_gone());
        assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err());
        out.set_volume(Volume::Db(0.0)).await.unwrap();
        let (op, _) = next_frame(&mut again).unwrap();
        assert_eq!(&op, b"audg");
    }
}
