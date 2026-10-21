//! What a renderer accepts, and how to offer the stream so DLNA TVs take it.
//!
//! - `ConnectionManager.GetProtocolInfo` lists the renderer's sink formats. WAV (`audio/wav`,
//!   `audio/x-wav`) carries any rate and 16/24 bits as-is.
//! - Many TVs (LG and Samsung models among them) only take DLNA's own LPCM: `audio/L16`,
//!   big-endian 16-bit. The node serves that too (`stream.l16`), and the zone's signal path shows
//!   the requantization to 16 bits.
//! - A renderer that lists neither gets no zone: it would only ever fail.
//! - The fourth protocolInfo field tells a DLNA device this is a live stream. That field is also
//!   sent as `contentFeatures.dlna.org` on the HTTP answer, with `transferMode.dlna.org: Streaming`,
//!   because some TVs refuse a stream without them.

use crate::{soap, Renderer, UpnpError};

pub const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// A live stream: no seeking by range or time, streaming transfer, DLNA 1.5.
pub const LIVE_FLAGS: &str = "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

/// How the stream is offered to a renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamKind {
    /// RIFF WAV under the MIME type the renderer listed.
    Wav { mime: String },
    /// DLNA LPCM: raw big-endian 16-bit PCM, `audio/L16;rate=…;channels=…`.
    L16,
}

impl StreamKind {
    /// The path segment the node serves this kind under.
    pub fn file(&self) -> &'static str {
        match self {
            StreamKind::Wav { .. } => "stream.wav",
            StreamKind::L16 => "stream.l16",
        }
    }

    pub fn mime(&self, rate_hz: u32, channels: u8) -> String {
        match self {
            StreamKind::Wav { mime } => mime.clone(),
            StreamKind::L16 => format!("audio/L16;rate={rate_hz};channels={channels}"),
        }
    }

    /// The DLNA fourth field: the LPCM profile for L16, flags for both.
    pub fn content_features(&self) -> String {
        match self {
            StreamKind::Wav { .. } => LIVE_FLAGS.to_string(),
            StreamKind::L16 => format!("DLNA.ORG_PN=LPCM;{LIVE_FLAGS}"),
        }
    }

    pub fn protocol_info(&self, rate_hz: u32, channels: u8) -> String {
        format!(
            "http-get:*:{}:{}",
            self.mime(rate_hz, channels),
            self.content_features()
        )
    }
}

/// The renderer's sink protocolInfo entries; empty when it has no ConnectionManager.
pub fn sink_protocols(r: &Renderer) -> Result<Vec<String>, UpnpError> {
    let Some(cm) = &r.connection_manager else {
        return Ok(Vec::new());
    };
    let xml = soap::call(cm, CONNECTION_MANAGER, "GetProtocolInfo", &[])?;
    Ok(soap::element_text(&xml, "Sink")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Pick how to stream to a renderer from its sink list. `None`: it takes no PCM over HTTP.
/// An empty list (no ConnectionManager, or it did not answer) gets WAV and a chance.
pub fn choose(sinks: &[String]) -> Option<StreamKind> {
    if sinks.is_empty() {
        return Some(StreamKind::Wav {
            mime: "audio/wav".into(),
        });
    }
    let mut l16 = false;
    let mut wav: Option<String> = None;
    for p in sinks {
        let mut parts = p.splitn(4, ':');
        let (proto, _, mime) = (parts.next(), parts.next(), parts.next());
        if !matches!(proto, Some("http-get") | Some("*")) {
            continue;
        }
        let Some(mime) = mime else { continue };
        let base = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        match base.as_str() {
            "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" if wav.is_none() => {
                wav = Some(mime.split(';').next().unwrap_or(mime).trim().to_string())
            }
            "audio/l16" => l16 = true,
            "*" | "audio/*" if wav.is_none() => wav = Some("audio/wav".into()),
            _ => {}
        }
    }
    match (wav, l16) {
        (Some(mime), _) => Some(StreamKind::Wav { mime }),
        (None, true) => Some(StreamKind::L16),
        (None, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn wav_wins_l16_next_and_no_pcm_means_no_zone() {
        assert_eq!(
            choose(&v(&[
                "http-get:*:audio/mpeg:*",
                "http-get:*:audio/x-wav:*",
                "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM"
            ])),
            Some(StreamKind::Wav {
                mime: "audio/x-wav".into()
            })
        );
        // An LG-style list: LPCM only.
        assert_eq!(
            choose(&v(&[
                "http-get:*:audio/mpeg:DLNA.ORG_PN=MP3",
                "http-get:*:audio/L16;rate=48000;channels=2:DLNA.ORG_PN=LPCM"
            ])),
            Some(StreamKind::L16)
        );
        assert_eq!(
            choose(&v(&["http-get:*:audio/mpeg:*", "http-get:*:audio/flac:*"])),
            None
        );
        assert_eq!(
            choose(&[]),
            Some(StreamKind::Wav {
                mime: "audio/wav".into()
            })
        );
        assert_eq!(
            choose(&v(&["http-get:*:*:*"])),
            Some(StreamKind::Wav {
                mime: "audio/wav".into()
            })
        );
        assert_eq!(choose(&v(&["rtsp-rtp-udp:*:audio/L16:*"])), None);
    }

    #[test]
    fn protocol_info_carries_the_dlna_fields() {
        assert_eq!(
            StreamKind::L16.protocol_info(44_100, 2),
            "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM;DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
        );
        let w = StreamKind::Wav {
            mime: "audio/wav".into(),
        };
        assert!(w
            .protocol_info(96_000, 2)
            .starts_with("http-get:*:audio/wav:DLNA.ORG_OP=00"));
        assert_eq!(w.file(), "stream.wav");
        assert_eq!(StreamKind::L16.file(), "stream.l16");
    }
}
