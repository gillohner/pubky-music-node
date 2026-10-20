//! A connected Slimproto player as an `Output`.

use crate::{audg, strm, write_frame, Hello, Stat, FIXED_ONE};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Volume, VolumeKind};
use pm_zones::outputs::PullOutput;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, SinkError};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The last `STAT` we saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlayerStatus {
    pub stream_buffer_size: u32,
    pub stream_buffer_fullness: u32,
    pub output_buffer_size: u32,
    pub output_buffer_fullness: u32,
    pub elapsed_ms: u32,
    pub last_event: [u8; 4],
}

pub struct SlimOutput {
    hello: Hello,
    writer: Arc<Mutex<TcpStream>>,
    server_port: u16,
    stream_path: Arc<dyn Fn(&str) -> String + Send + Sync>,
    pull: Arc<PullOutput>,
    caps: OutputCaps,
    status: Mutex<PlayerStatus>,
    gone: AtomicBool,
}

impl SlimOutput {
    pub fn new(
        hello: Hello,
        writer: Arc<Mutex<TcpStream>>,
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
            writer,
            server_port,
            stream_path,
            pull: Arc::new(PullOutput::new(&name)),
            caps,
            status: Mutex::new(PlayerStatus::default()),
            gone: AtomicBool::new(false),
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
        *self.status.lock().expect("status")
    }

    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    pub(crate) fn on_stat(&self, st: Stat) {
        let mut s = self.status.lock().expect("status");
        s.stream_buffer_size = st.stream_buffer_size;
        s.stream_buffer_fullness = st.stream_buffer_fullness;
        s.output_buffer_size = st.output_buffer_size;
        s.output_buffer_fullness = st.output_buffer_fullness;
        s.elapsed_ms = st.elapsed_ms;
        s.last_event = st.event;
    }

    pub(crate) fn mark_gone(&self) {
        self.gone.store(true, Ordering::Relaxed);
    }

    fn send(&self, op: &[u8; 4], data: &[u8]) -> Result<(), SinkError> {
        if self.is_gone() {
            return Err(SinkError::Unavailable(self.id(), "player disconnected".into()));
        }
        let mut w = self.writer.lock().expect("writer");
        write_frame(&mut *w, op, data).map_err(|e| SinkError::Device(format!("{}: {e}", self.id())))
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
        let path = (self.stream_path)(self.pull.stream_name());
        let request = format!("GET {path} HTTP/1.0\r\n\r\n");
        let payload = strm(
            b's',
            1,
            fmt.bits,
            fmt.rate_hz,
            fmt.channels,
            self.server_port,
            0,
            &request,
        );
        self.send(b"strm", &payload)?;
        Ok(Box::new(SlimSink {
            inner,
            writer: self.writer.clone(),
            status: self.status(),
            fmt,
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
        self.send(b"audg", &audg((gain * FIXED_ONE as f64).round() as u32))
    }
}

struct SlimSink {
    inner: Box<dyn AudioSink>,
    writer: Arc<Mutex<TcpStream>>,
    status: PlayerStatus,
    fmt: AudioFormat,
}

impl AudioSink for SlimSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }
    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        self.inner.write(block)
    }
    fn buffer_level(&self) -> BufferLevel {
        // The player's output buffer in frames; the pull channel adds a little.
        let frame_bytes = (self.fmt.bits as u64).div_ceil(8) * self.fmt.channels as u64;
        BufferLevel {
            queued_frames: self.status.output_buffer_fullness as u64 / frame_bytes.max(1),
            capacity_frames: self.status.output_buffer_size as u64 / frame_bytes.max(1),
        }
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        let mut w = self.writer.lock().expect("writer");
        let _ = write_frame(
            &mut *w,
            b"strm",
            &strm(
                b'f',
                0,
                self.fmt.bits,
                self.fmt.rate_hz,
                self.fmt.channels,
                0,
                0,
                "",
            ),
        );
        self.inner.flush()
    }
    fn drain(&mut self) -> Result<(), SinkError> {
        self.inner.drain()
    }
    fn frames_written(&self) -> u64 {
        self.inner.frames_written()
    }
}

impl Drop for SlimSink {
    fn drop(&mut self) {
        let mut w = self.writer.lock().expect("writer");
        let _ = write_frame(
            &mut *w,
            b"strm",
            &strm(
                b'q',
                0,
                self.fmt.bits,
                self.fmt.rate_hz,
                self.fmt.channels,
                0,
                0,
                "",
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_frame_server;
    use std::io::Write;
    use std::net::TcpListener;

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
        let (op, data) = read_frame_server(&mut player).unwrap();
        assert_eq!(&op, b"aude");
        assert_eq!(data, vec![1, 1]);

        let fmt = AudioFormat::pcm(48_000, 24, 2);
        let mut sink = out.open(fmt).await.unwrap();
        let (op, data) = read_frame_server(&mut player).unwrap();
        assert_eq!(&op, b"strm");
        assert_eq!(&data[0..7], &[b's', 1, b'p', b'2', b'4', b'2', b'1']);
        assert_eq!(&data[18..20], &7788u16.to_be_bytes());
        let req = String::from_utf8_lossy(&data[24..]).to_string();
        assert_eq!(
            req,
            "GET /v1/outputs/pull/slim-aabbcc001122/stream.wav?token=t HTTP/1.0\r\n\r\n"
        );

        // The "player" fetches: take the pull stream and read a block.
        let mut live = out
            .pull()
            .take_stream(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let block = AudioBlock::pcm_i32(fmt, vec![1, -1]);
        let writer = std::thread::spawn(move || {
            sink.write(&block).unwrap();
            drop(sink);
        });
        let header = live.rx.recv().await.unwrap();
        assert_eq!(&header[0..4], b"RIFF");
        let pcm = live.rx.recv().await.unwrap();
        assert_eq!(pcm, vec![1, 0, 0, 0xff, 0xff, 0xff]);
        writer.join().unwrap();
        let (op, data) = read_frame_server(&mut player).unwrap();
        assert_eq!(&op, b"strm");
        assert_eq!(data[0], b'q');

        // Volume: -6 dB ≈ 0.501 in 16.16.
        out.set_volume(Volume::Db(-6.0)).await.unwrap();
        let (op, data) = read_frame_server(&mut player).unwrap();
        assert_eq!(&op, b"audg");
        let g = u32::from_be_bytes([data[10], data[11], data[12], data[13]]);
        assert!((g as f64 / FIXED_ONE as f64 - 0.501).abs() < 0.01, "{g}");

        // STAT updates the status.
        let mut st = Vec::new();
        st.extend_from_slice(b"STMt");
        st.extend_from_slice(&[0, 0, 0]);
        st.extend_from_slice(&1000u32.to_be_bytes());
        st.extend_from_slice(&500u32.to_be_bytes());
        st.extend_from_slice(&[0u8; 8]);
        st.extend_from_slice(&0xffffu16.to_be_bytes());
        st.extend_from_slice(&1u32.to_be_bytes());
        st.extend_from_slice(&4000u32.to_be_bytes());
        st.extend_from_slice(&2000u32.to_be_bytes());
        st.extend_from_slice(&7u32.to_be_bytes());
        st.extend_from_slice(&0u16.to_be_bytes());
        st.extend_from_slice(&7500u32.to_be_bytes());
        st.extend_from_slice(&0u32.to_be_bytes());
        st.extend_from_slice(&0u16.to_be_bytes());
        player.write_all(b"STAT").unwrap();
        player.write_all(&(st.len() as u32).to_be_bytes()).unwrap();
        player.write_all(&st).unwrap();
        for _ in 0..100 {
            if out.status().elapsed_ms == 7500 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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
    }
}
