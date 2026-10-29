//! The node ↔ endpoint wire protocol (handoff M2, ADR 0007): a plain TCP
//! stream of length-prefixed frames, control frames as JSON, audio as the
//! block's samples verbatim.
//!
//! Principles (handoff §6.1):
//! * **The endpoint owns the clock.** The node sends one block and waits
//!   for the endpoint's `Ack`, which the endpoint sends only after the
//!   block is in its sink. A full DAC buffer therefore stalls the sender;
//!   no timers, no rate estimation on the node side.
//! * **Nothing is converted on the wire.** PCM samples travel as the
//!   `AudioBlock` layout itself (`i32` little-endian, right-justified), DSD
//!   as its bytes. The endpoint packs for its device exactly as a local
//!   output would, so the signal path is unchanged by the network.
//! * **Formats are negotiated by caps**, like any other output: `Welcome`
//!   carries the endpoint's outputs with their `OutputCaps`; `Open` names
//!   one and a format it accepts as-is.
//!
//! Frame: `kind: u8`, `len: u32 BE`, `payload[len]`. Control payloads are
//! JSON; `Audio` payload is raw sample data.
//!
//! The endpoint is the desktop app's built-in player, paired with its own node: the app makes a
//! random key when it starts the player and registers it with the node, which sends it in `Hello`
//! (protocol 2). The endpoint answers nothing else to a client without that key.

use pm_model::{AudioBlock, AudioFormat, Samples, Volume};
use pm_zones::{BufferLevel, OutputInfo};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// 2: `Hello` carries the endpoint's key.
pub const PROTOCOL_VERSION: u32 = 2;
pub const DEFAULT_PORT: u16 = 7790;
/// Largest audio frame sent or accepted. A decoded packet is at most a few MB (65 535 frames of
/// 8-channel 32-bit is 2 MB); a larger block goes out in several frames ([`write_audio`]).
pub const MAX_FRAME: u32 = 4 * 1024 * 1024;
/// Largest control frame (JSON) accepted: a `Welcome` with every output's caps fits many times.
pub const MAX_CONTROL_FRAME: u32 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    /// Client → endpoint, first frame. `key` is the endpoint's (protocol 2; absent in 1).
    Hello {
        client: String,
        version: u32,
        #[serde(default)]
        key: Key,
    },
    /// Endpoint → client, reply to Hello.
    Welcome {
        name: String,
        version: u32,
        outputs: Vec<OutputInfo>,
    },
    /// Open an output in exactly this format.
    Open {
        output: String,
        format: AudioFormat,
    },
    Opened {
        buffer_frames: u64,
    },
    /// Raw samples for the open format; answered by `Ack` once in the sink.
    Audio(AudioPayload),
    Ack {
        level: BufferLevel,
        frames_written: u64,
    },
    Flush,
    Flushed,
    Drain,
    Drained,
    SetVolume {
        output: String,
        volume: Volume,
    },
    Done,
    Close,
    Error {
        message: String,
    },
}

/// The secret a node shows the endpoint in `Hello`. Never printed: `Debug` hides it, so a logged
/// frame or error does not carry it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Key(String);

impl Key {
    /// A new key: 32 bytes from the operating system's CSPRNG, as hex.
    pub fn random() -> Self {
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        Key(b.iter().map(|x| format!("{x:02x}")).collect())
    }

    /// A key given by hand or read from a file: 32 to 256 visible ASCII characters.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if !(32..=256).contains(&s.len()) || !s.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("an endpoint key is 32 to 256 visible ASCII characters".into());
        }
        Ok(Key(s.to_string()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The key itself, to send or store; never to log.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether `other` is this key, in time that does not depend on where they differ. An empty
    /// key matches nothing.
    pub fn matches(&self, other: &Key) -> bool {
        let (a, b) = (self.0.as_bytes(), other.0.as_bytes());
        if a.is_empty() || a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "Key(none)"
        } else {
            "Key(…)"
        })
    }
}

/// Audio bytes travel outside JSON; this placeholder keeps `Frame` one enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AudioPayload {
    pub frames: u64,
}

const KIND_CONTROL: u8 = 1;
const KIND_AUDIO: u8 = 2;

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error("endpoint error: {0}")]
    Remote(String),
    #[error("unexpected frame {0}")]
    Unexpected(String),
}

/// Write a control frame.
pub fn write_frame(w: &mut dyn Write, frame: &Frame) -> Result<(), StreamError> {
    let body = serde_json::to_vec(frame).map_err(|e| StreamError::Malformed(e.to_string()))?;
    write_raw(w, KIND_CONTROL, &body)
}

/// Write a block's samples in wire layout as audio frames: one, or several of whole sample frames
/// when the block is larger than [`MAX_FRAME`]. Returns how many frames were sent (each is
/// answered by its own `Ack`).
pub fn write_audio(w: &mut dyn Write, block: &AudioBlock) -> Result<usize, StreamError> {
    let bytes = encode_samples(block);
    let per_frame = match block.format.kind {
        pm_model::SampleKind::Pcm => 4 * block.format.channels as usize,
        pm_model::SampleKind::Dsd => block.format.channels as usize,
    }
    .max(1);
    let chunk = (MAX_FRAME as usize / per_frame) * per_frame;
    if bytes.is_empty() {
        write_raw(w, KIND_AUDIO, &bytes)?;
        return Ok(1);
    }
    let mut sent = 0;
    for part in bytes.chunks(chunk) {
        write_raw(w, KIND_AUDIO, part)?;
        sent += 1;
    }
    Ok(sent)
}

fn write_raw(w: &mut dyn Write, kind: u8, body: &[u8]) -> Result<(), StreamError> {
    if body.len() as u64 > MAX_FRAME as u64 {
        return Err(StreamError::Malformed(format!(
            "frame of {} bytes too large",
            body.len()
        )));
    }
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..5].copy_from_slice(&(body.len() as u32).to_be_bytes());
    w.write_all(&head)?;
    w.write_all(body)?;
    w.flush()?;
    Ok(())
}

/// What `read_frame` yields: a control frame or audio bytes.
#[derive(Debug)]
pub enum Incoming {
    Control(Frame),
    Audio(Vec<u8>),
}

pub fn read_frame(r: &mut dyn Read) -> Result<Incoming, StreamError> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]);
    let max = if head[0] == KIND_AUDIO {
        MAX_FRAME
    } else {
        MAX_CONTROL_FRAME
    };
    if len > max {
        return Err(StreamError::Malformed(format!("frame of {len} bytes too large")));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    match head[0] {
        KIND_CONTROL => {
            let f: Frame =
                serde_json::from_slice(&body).map_err(|e| StreamError::Malformed(e.to_string()))?;
            Ok(Incoming::Control(f))
        }
        KIND_AUDIO => Ok(Incoming::Audio(body)),
        k => Err(StreamError::Malformed(format!("unknown frame kind {k}"))),
    }
}

/// Read a control frame, treating audio or `Error` as failures.
pub fn expect_control(r: &mut dyn Read) -> Result<Frame, StreamError> {
    match read_frame(r)? {
        Incoming::Control(Frame::Error { message }) => Err(StreamError::Remote(message)),
        Incoming::Control(f) => Ok(f),
        Incoming::Audio(_) => Err(StreamError::Unexpected("audio".into())),
    }
}

/// Wire layout of a block: PCM as `i32` LE per sample, DSD as its bytes.
/// Float blocks are not sent (outputs never take them).
pub fn encode_samples(block: &AudioBlock) -> Vec<u8> {
    match &block.samples {
        Samples::PcmI32(s) => {
            let mut v = Vec::with_capacity(s.len() * 4);
            for x in s {
                v.extend_from_slice(&x.to_le_bytes());
            }
            v
        }
        Samples::Dsd(b) => b.clone(),
        Samples::PcmF64(f) => {
            // Defensive: never happens after the pipeline's requantize stage.
            let mut v = Vec::with_capacity(f.len() * 4);
            for x in f {
                let s = (x.clamp(-1.0, 1.0) * i32::MAX as f64) as i32;
                v.extend_from_slice(&s.to_le_bytes());
            }
            v
        }
    }
}

/// Inverse of [`encode_samples`] for the open format.
pub fn decode_samples(fmt: AudioFormat, bytes: &[u8]) -> Result<AudioBlock, StreamError> {
    match fmt.kind {
        pm_model::SampleKind::Pcm => {
            if !bytes.len().is_multiple_of(4 * fmt.channels as usize) {
                return Err(StreamError::Malformed(format!(
                    "{} audio bytes is not whole {}-channel i32 frames",
                    bytes.len(),
                    fmt.channels
                )));
            }
            let samples: Vec<i32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| i32::from_le_bytes(*c))
                .collect();
            Ok(AudioBlock::pcm_i32(fmt, samples))
        }
        pm_model::SampleKind::Dsd => {
            if !bytes.len().is_multiple_of(fmt.channels as usize) {
                return Err(StreamError::Malformed("DSD bytes not whole frames".into()));
            }
            Ok(AudioBlock::dsd(fmt, bytes.to_vec()))
        }
    }
}

/// Run a future to completion on this thread (for `Output::open` from std threads).
pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_audio_round_trip() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let block = AudioBlock::pcm_i32(fmt, vec![1, -1, 32767, -32768]);
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Frame::Open {
                output: "null".into(),
                format: fmt,
            },
        )
        .unwrap();
        write_audio(&mut buf, &block).unwrap();
        write_frame(&mut buf, &Frame::Drain).unwrap();
        let mut r = io::Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut r).unwrap(),
            Incoming::Control(Frame::Open { .. })
        ));
        let Incoming::Audio(bytes) = read_frame(&mut r).unwrap() else {
            panic!()
        };
        assert_eq!(bytes.len(), 16);
        assert_eq!(decode_samples(fmt, &bytes).unwrap(), block);
        assert!(matches!(expect_control(&mut r).unwrap(), Frame::Drain));
        assert!(read_frame(&mut r).is_err(), "EOF");
    }

    #[test]
    fn dsd_and_errors() {
        let fmt = AudioFormat::dsd(AudioFormat::DSD64_RATE, 2);
        let block = AudioBlock::dsd(fmt, vec![0x69, 0x96, 0x11, 0x22]);
        assert_eq!(decode_samples(fmt, &encode_samples(&block)).unwrap(), block);
        assert!(decode_samples(fmt, &[1, 2, 3]).is_err());
        assert!(decode_samples(AudioFormat::pcm(44_100, 16, 2), &[0u8; 6]).is_err());
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Frame::Error {
                message: "busy".into(),
            },
        )
        .unwrap();
        let err = expect_control(&mut io::Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, StreamError::Remote(m) if m == "busy"));
        let bad = [9u8, 0, 0, 0, 0];
        assert!(read_frame(&mut io::Cursor::new(bad.to_vec())).is_err());
    }

    #[test]
    fn keys_compare_whole_and_never_print() {
        let k = Key::random();
        assert_eq!(k.expose().len(), 64);
        assert_ne!(k, Key::random());
        assert!(k.matches(&k.clone()));
        assert!(!k.matches(&Key::default()));
        assert!(!Key::default().matches(&Key::default()), "no key is not a key");
        let mut other = k.expose().to_string();
        other.replace_range(63.., if other.ends_with('0') { "1" } else { "0" });
        assert!(!k.matches(&Key::parse(&other).unwrap()));
        assert!(!k.matches(&Key::parse(&k.expose()[..40]).unwrap()));
        assert_eq!(Key::parse(&format!(" {} ", k.expose())).unwrap(), k);
        assert!(Key::parse("short").is_err());
        assert!(Key::parse(&"a b".repeat(20)).is_err());
        let hello = Frame::Hello {
            client: "n".into(),
            version: PROTOCOL_VERSION,
            key: k.clone(),
        };
        assert!(!format!("{hello:?}").contains(k.expose()));
        // On the wire it is a plain string, and a protocol-1 Hello (no key) still reads
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(json["Hello"]["key"], k.expose());
        let v1: Frame = serde_json::from_str(r#"{"Hello":{"client":"old","version":1}}"#).unwrap();
        assert!(matches!(v1, Frame::Hello { version: 1, key, .. } if key.is_empty()));
    }

    #[test]
    fn frame_sizes_are_bounded_and_big_blocks_split() {
        // A control frame claiming more than MAX_CONTROL_FRAME is refused before it is read
        let mut head = vec![KIND_CONTROL];
        head.extend_from_slice(&(MAX_CONTROL_FRAME + 1).to_be_bytes());
        assert!(matches!(
            read_frame(&mut io::Cursor::new(head)),
            Err(StreamError::Malformed(_))
        ));
        let mut head = vec![KIND_AUDIO];
        head.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        assert!(matches!(
            read_frame(&mut io::Cursor::new(head)),
            Err(StreamError::Malformed(_))
        ));
        const { assert!(MAX_FRAME <= 4 * 1024 * 1024) };
        // A block of 1.5 MAX_FRAME goes out as two frames of whole sample frames, read back whole
        let fmt = AudioFormat::pcm(768_000, 32, 6);
        let n = (MAX_FRAME as usize * 3 / 2) / 4;
        let n = n - n % 6;
        let block = AudioBlock::pcm_i32(fmt, (0..n as i32).collect());
        let mut buf = Vec::new();
        assert_eq!(write_audio(&mut buf, &block).unwrap(), 2);
        let mut r = io::Cursor::new(buf);
        let mut all = Vec::new();
        for _ in 0..2 {
            let Incoming::Audio(b) = read_frame(&mut r).unwrap() else {
                panic!()
            };
            assert!(b.len() <= MAX_FRAME as usize && b.len() % 24 == 0);
            all.extend(b);
        }
        assert_eq!(decode_samples(fmt, &all).unwrap(), block);
    }
}
