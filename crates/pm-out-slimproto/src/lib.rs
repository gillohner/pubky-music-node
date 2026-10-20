//! Slimproto server (handoff M7): Squeezebox-family players and squeezelite
//! connect to us on TCP 3483, we tell them to fetch a pull stream of the
//! zone's post-DSP PCM (`strm s` with the format in the header and an HTTP
//! request line), and their fetching paces the zone. Volume is the player's
//! 16.16 digital gain (`audg`); `STAT` reports feed the buffer level and
//! elapsed time. Wire layout follows squeezelite's `slimproto.h`.
//!
//! The server only receives connections; it never scans the network. The
//! UDP discovery responder answers players' broadcasts so `squeezelite`
//! without `-s` finds the node.

pub mod output;

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use output::{PlayerStatus, SlimOutput};

pub const DEFAULT_PORT: u16 = 3483;

#[derive(Debug, thiserror::Error)]
pub enum SlimError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
}

/// What a player told us in `HELO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub device_id: u8,
    pub revision: u8,
    pub mac: [u8; 6],
    pub capabilities: Vec<String>,
}

impl Hello {
    pub fn mac_string(&self) -> String {
        self.mac
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    pub fn model(&self) -> String {
        self.cap("ModelName")
            .or_else(|| self.cap("Model"))
            .unwrap_or_else(|| match self.device_id {
                2 => "Squeezebox".into(),
                3 => "Softsqueeze".into(),
                4 => "Squeezebox2".into(),
                5 => "Transporter".into(),
                7 => "Squeezebox Receiver".into(),
                8 => "Squeezebox Boom".into(),
                9 => "Squeezebox Radio".into(),
                12 => "SqueezePlay".into(),
                other => format!("Slim device {other}"),
            })
    }

    pub fn cap(&self, key: &str) -> Option<String> {
        self.capabilities.iter().find_map(|c| {
            c.split_once('=')
                .filter(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.to_string())
        })
    }

    pub fn max_sample_rate(&self) -> u32 {
        self.cap("MaxSampleRate")
            .and_then(|v| v.parse().ok())
            .unwrap_or(48_000)
    }

    pub fn supports_pcm(&self) -> bool {
        self.capabilities.iter().any(|c| c.eq_ignore_ascii_case("pcm"))
            || self.capabilities.iter().all(|c| c.contains('='))
    }
}

/// Parse a HELO payload (after the opcode and length).
pub fn parse_hello(data: &[u8]) -> Result<Hello, SlimError> {
    if data.len() < 8 {
        return Err(SlimError::Protocol(format!(
            "HELO too short: {} bytes",
            data.len()
        )));
    }
    let device_id = data[0];
    let revision = data[1];
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&data[2..8]);
    // Fixed part: 8, then uuid 16, wlan 2, bytes received 8, lang 2 = 36 for
    // modern firmware; capabilities follow as text. Older players stop early.
    let caps_start = if data.len() > 36 { 36 } else { data.len() };
    let text = String::from_utf8_lossy(&data[caps_start..]);
    let capabilities = text
        .split(',')
        .map(|c| c.trim_matches(char::from(0)).trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    Ok(Hello {
        device_id,
        revision,
        mac,
        capabilities,
    })
}

/// Read one client frame: 4-char opcode + u32 BE length + payload.
pub fn read_frame(r: &mut dyn Read) -> io::Result<([u8; 4], Vec<u8>)> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head)?;
    let mut op = [0u8; 4];
    op.copy_from_slice(&head[..4]);
    let len = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
    if len > 1 << 20 {
        return Err(io::Error::other(format!("frame of {len} bytes too large")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok((op, body))
}

/// Read one server frame as a player sees it (tests): u16 BE length, opcode, data.
pub fn read_frame_server(r: &mut dyn Read) -> io::Result<([u8; 4], Vec<u8>)> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len)?;
    let len = u16::from_be_bytes(len) as usize;
    if len < 4 {
        return Err(io::Error::other("short server frame"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let mut op = [0u8; 4];
    op.copy_from_slice(&body[..4]);
    Ok((op, body[4..].to_vec()))
}

/// Write one server frame: u16 BE length (opcode + data) then opcode + data.
pub fn write_frame(w: &mut dyn Write, op: &[u8; 4], data: &[u8]) -> io::Result<()> {
    let len = (4 + data.len()) as u16;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(op)?;
    w.write_all(data)?;
    w.flush()
}

/// The `strm` sample-rate code squeezelite indexes with `rate - '0'`.
pub fn rate_code(rate_hz: u32) -> Option<u8> {
    const RATES: [u32; 15] = [
        11_025, 22_050, 32_000, 44_100, 48_000, 8_000, 12_000, 16_000, 24_000, 96_000, 88_200, 176_400,
        192_000, 352_800, 384_000,
    ];
    RATES.iter().position(|r| *r == rate_hz).map(|i| b'0' + i as u8)
}

/// `strm` payload for `command`; `request` is the HTTP request text sent
/// after the 24-byte header (start only).
#[allow(clippy::too_many_arguments)]
pub fn strm(
    command: u8,
    autostart: u8,
    bits: u8,
    rate_hz: u32,
    channels: u8,
    server_port: u16,
    replay_gain: u32,
    request: &str,
) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + request.len());
    p.push(command);
    p.push(autostart);
    p.push(b'p'); // PCM
    p.push(b'0' + bits.div_ceil(8).saturating_sub(1)); // bytes per sample - 1
    p.push(rate_code(rate_hz).unwrap_or(b'3'));
    p.push(b'0' + channels);
    p.push(b'1'); // little endian
    p.push(10); // threshold: KB before autostart
    p.push(0); // spdif auto
    p.push(0); // transition period
    p.push(0); // transition type
    p.push(0); // flags
    p.push(1); // output threshold, tenths of a second
    p.push(0); // slaves / reserved
    p.extend_from_slice(&replay_gain.to_be_bytes());
    p.extend_from_slice(&server_port.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // server ip 0 = the control connection's host
    p.extend_from_slice(request.as_bytes());
    p
}

/// `audg` payload: digital volume on, 16.16 gain for both channels.
pub fn audg(gain_16_16: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(18);
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.push(1); // adjust (dvc)
    p.push(255); // preamp
    p.extend_from_slice(&gain_16_16.to_be_bytes());
    p.extend_from_slice(&gain_16_16.to_be_bytes());
    p
}

pub const FIXED_ONE: u32 = 0x1_0000;

/// A parsed `STAT` report (the fields we use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stat {
    pub event: [u8; 4],
    pub stream_buffer_size: u32,
    pub stream_buffer_fullness: u32,
    pub output_buffer_size: u32,
    pub output_buffer_fullness: u32,
    pub elapsed_ms: u32,
}

pub fn parse_stat(data: &[u8]) -> Option<Stat> {
    if data.len() < 43 {
        return None;
    }
    let u32_at = |i: usize| u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
    let mut event = [0u8; 4];
    event.copy_from_slice(&data[0..4]);
    // event 4, num_crlf 1, mas_init 1, mas_mode 1 = 7; then u32s.
    let stream_buffer_size = u32_at(7);
    let stream_buffer_fullness = u32_at(11);
    // bytes received 8 (15..23), signal 2 (23..25), jiffies 4 (25..29)
    let output_buffer_size = u32_at(29);
    let output_buffer_fullness = u32_at(33);
    // elapsed seconds 4 (37..41), voltage 2 (41..43), elapsed ms 4 (43..47)
    let elapsed_ms = if data.len() >= 47 {
        u32_at(43)
    } else {
        u32_at(37) * 1000
    };
    Some(Stat {
        event,
        stream_buffer_size,
        stream_buffer_fullness,
        output_buffer_size,
        output_buffer_fullness,
        elapsed_ms,
    })
}

/// Serve players on `listener`; `on_player` is called once per connected
/// player with its output. Blocks; run on a thread.
pub fn serve(
    listener: TcpListener,
    server_port: u16,
    stream_path: Arc<dyn Fn(&str) -> String + Send + Sync>,
    on_player: Arc<dyn Fn(Arc<SlimOutput>) + Send + Sync>,
) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("slimproto accept: {e}");
                continue;
            }
        };
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        let on_player = on_player.clone();
        let stream_path = stream_path.clone();
        std::thread::Builder::new()
            .name(format!("slimproto {peer}"))
            .spawn(move || {
                if let Err(e) = session(stream, server_port, stream_path, on_player) {
                    tracing::info!(%peer, "slimproto session ended: {e}");
                }
            })?;
    }
    Ok(())
}

fn session(
    stream: TcpStream,
    server_port: u16,
    stream_path: Arc<dyn Fn(&str) -> String + Send + Sync>,
    on_player: Arc<dyn Fn(Arc<SlimOutput>) + Send + Sync>,
) -> Result<(), SlimError> {
    stream.set_nodelay(true)?;
    let mut reader = stream.try_clone()?;
    let (op, body) = read_frame(&mut reader)?;
    if &op != b"HELO" {
        return Err(SlimError::Protocol(format!(
            "expected HELO, got {}",
            String::from_utf8_lossy(&op)
        )));
    }
    let hello = parse_hello(&body)?;
    tracing::info!(player = %hello.mac_string(), model = %hello.model(), max_rate = hello.max_sample_rate(), "slimproto player");
    let writer = Arc::new(Mutex::new(stream));
    let output = Arc::new(SlimOutput::new(
        hello.clone(),
        writer.clone(),
        server_port,
        stream_path,
    ));
    // Turn the DAC on and reset volume to the player's last setting (full).
    write_frame(&mut *writer.lock().expect("writer"), b"aude", &[1, 1])?;
    on_player(output.clone());
    loop {
        let (op, body) = match read_frame(&mut reader) {
            Ok(f) => f,
            Err(e) => {
                output.mark_gone();
                return Err(e.into());
            }
        };
        match &op {
            b"STAT" => {
                if let Some(st) = parse_stat(&body) {
                    output.on_stat(st);
                }
            }
            b"HELO" => {}
            b"BYE!" => {
                output.mark_gone();
                return Ok(());
            }
            _ => {}
        }
    }
}

/// Answer players' UDP discovery broadcasts on port 3483 with our name and
/// web port so `squeezelite` without `-s` finds the node. Blocks.
pub fn discovery_responder(name: &str, web_port: u16) -> io::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", DEFAULT_PORT))?;
    socket.set_broadcast(true)?;
    let mut buf = [0u8; 1024];
    loop {
        let (n, from) = socket.recv_from(&mut buf)?;
        if n == 0 || buf[0] != b'e' {
            continue;
        }
        // Request: 'e' then TLVs: 4-char tag + 1 byte len + value; reply 'E' + TLVs.
        let mut reply = vec![b'E'];
        let mut i = 1;
        while i + 5 <= n {
            let tag = &buf[i..i + 4];
            let len = buf[i + 4] as usize;
            i += 5 + len;
            let value: Option<String> = match tag {
                b"NAME" => Some(name.to_string()),
                b"JSON" => Some(web_port.to_string()),
                b"VERS" => Some("7.9.0".into()),
                b"UUID" => Some("pubky-music-node".into()),
                _ => None,
            };
            if let Some(v) = value {
                reply.extend_from_slice(tag);
                reply.push(v.len().min(255) as u8);
                reply.extend_from_slice(&v.as_bytes()[..v.len().min(255)]);
            }
        }
        let _ = socket.send_to(&reply, from);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_parsing_old_and_new_players() {
        let mut d = vec![12u8, 0, 0x02, 0x42, 0xac, 0x11, 0x00, 0x02];
        d.extend_from_slice(&[0u8; 28]);
        d.extend_from_slice(
            b"Model=squeezelite,ModelName=SqueezeLite,MaxSampleRate=192000,aac,ogg,flc,pcm,mp3",
        );
        let h = parse_hello(&d).unwrap();
        assert_eq!(h.mac_string(), "02:42:ac:11:00:02");
        assert_eq!(h.model(), "SqueezeLite");
        assert_eq!(h.max_sample_rate(), 192_000);
        assert!(h.supports_pcm());
        let old = parse_hello(&[2u8, 130, 1, 2, 3, 4, 5, 6, 0, 0]).unwrap();
        assert_eq!(old.model(), "Squeezebox");
        assert_eq!(old.max_sample_rate(), 48_000);
        assert!(parse_hello(&[1, 2, 3]).is_err());
    }

    #[test]
    fn frames_and_strm_layout() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"aude", &[1, 1]).unwrap();
        assert_eq!(buf, vec![0, 6, b'a', b'u', b'd', b'e', 1, 1]);
        let p = strm(b's', 1, 24, 96_000, 2, 7788, 0, "GET /x HTTP/1.0\r\n\r\n");
        assert_eq!(&p[0..7], &[b's', 1, b'p', b'2', b'9', b'2', b'1']);
        assert_eq!(p[7], 10);
        assert_eq!(&p[18..20], &7788u16.to_be_bytes());
        assert_eq!(&p[20..24], &[0, 0, 0, 0]);
        assert_eq!(&p[24..], b"GET /x HTTP/1.0\r\n\r\n");
        assert_eq!(rate_code(192_000), Some(b'<'));
        assert_eq!(rate_code(44_100), Some(b'3'));
        assert_eq!(rate_code(12_345), None);
        let g = audg(FIXED_ONE / 2);
        assert_eq!(g.len(), 18);
        assert_eq!(g[8], 1);
        assert_eq!(&g[10..14], &0x8000u32.to_be_bytes());
        let mut f = io::Cursor::new({
            let mut v = b"STAT".to_vec();
            v.extend_from_slice(&3u32.to_be_bytes());
            v.extend_from_slice(&[1, 2, 3]);
            v
        });
        let (op, body) = read_frame(&mut f).unwrap();
        assert_eq!(&op, b"STAT");
        assert_eq!(body, vec![1, 2, 3]);
    }

    #[test]
    fn stat_parsing() {
        let mut d = Vec::new();
        d.extend_from_slice(b"STMt");
        d.extend_from_slice(&[0, 0, 0]);
        d.extend_from_slice(&1_000_000u32.to_be_bytes()); // stream size
        d.extend_from_slice(&250_000u32.to_be_bytes()); // stream fullness
        d.extend_from_slice(&[0u8; 8]); // bytes received
        d.extend_from_slice(&0xffffu16.to_be_bytes());
        d.extend_from_slice(&12345u32.to_be_bytes()); // jiffies
        d.extend_from_slice(&3_000_000u32.to_be_bytes()); // output size
        d.extend_from_slice(&1_500_000u32.to_be_bytes()); // output fullness
        d.extend_from_slice(&12u32.to_be_bytes()); // elapsed s
        d.extend_from_slice(&0u16.to_be_bytes()); // voltage
        d.extend_from_slice(&12_345u32.to_be_bytes()); // elapsed ms
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        let st = parse_stat(&d).unwrap();
        assert_eq!(&st.event, b"STMt");
        assert_eq!(st.stream_buffer_fullness, 250_000);
        assert_eq!(st.output_buffer_fullness, 1_500_000);
        assert_eq!(st.elapsed_ms, 12_345);
        assert!(parse_stat(&d[..20]).is_none());
    }
}
