//! Library entities: what a scanned file is and what we know about it.

use crate::audio::AudioFormat;
use crate::ids::{Isrc, Mbid, MusicUri, Upc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Database row id for a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub i64);

/// How sure we are that a track is the recording its canonical URI names
/// (handoff §8.2: "store confidence; never publish below a threshold").
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Confidence {
    /// The file's tags carry the MBID.
    TaggedMbid,
    /// The file's tags carry an ISRC that resolved to one recording.
    TaggedIsrc,
    /// An AcoustID fingerprint match with this score (0–1).
    Fingerprint { score: f32 },
    /// Artist / title / duration match with this score (0–1).
    Fuzzy { score: f32 },
}

impl Confidence {
    /// Below this, a canonical URI is kept locally but never published.
    pub const PUBLISH_THRESHOLD: f32 = 0.85;

    pub fn score(&self) -> f32 {
        match self {
            Confidence::TaggedMbid => 1.0,
            Confidence::TaggedIsrc => 0.95,
            Confidence::Fingerprint { score } | Confidence::Fuzzy { score } => *score,
        }
    }

    pub fn publishable(&self) -> bool {
        self.score() >= Self::PUBLISH_THRESHOLD
    }
}

/// A file on disk (or on a mounted share) that the scanner found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioFile {
    pub path: PathBuf,
    pub size_bytes: u64,
    /// Modification time as Unix seconds; the scanner's change detector.
    pub mtime_unix: i64,
    /// Container/codec as the decoder reports it: `flac`, `alac`, `wav`,
    /// `aiff`, `dsf`, `dff`.
    pub container: String,
    pub format: AudioFormat,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artist {
    pub id: Option<i64>,
    pub name: String,
    pub mbid: Option<Mbid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub id: Option<i64>,
    pub title: String,
    pub album_artist: Option<String>,
    pub release_mbid: Option<Mbid>,
    pub release_group_mbid: Option<Mbid>,
    pub year: Option<i32>,
    pub upc: Option<Upc>,
    pub total_discs: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: Option<TrackId>,
    pub title: String,
    pub artists: Vec<Artist>,
    pub album: Option<Album>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub duration_ms: Option<u64>,
    pub file: AudioFile,
    pub recording_mbid: Option<Mbid>,
    pub isrc: Option<Isrc>,
    pub acoustid: Option<String>,
    /// Resolved canonical URI and how sure we are of it.
    pub canonical: Option<(MusicUri, Confidence)>,
    pub replaygain_track_gain_db: Option<f32>,
    pub replaygain_album_gain_db: Option<f32>,
    /// Genres from the file's tags, as written (several when the tag lists several).
    #[serde(default)]
    pub genres: Vec<String>,
}

impl Track {
    /// The URI to publish, if any: canonical and above the threshold.
    pub fn publishable_uri(&self) -> Option<&MusicUri> {
        self.canonical
            .as_ref()
            .filter(|(_, c)| c.publishable())
            .map(|(u, _)| u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_threshold() {
        assert!(Confidence::TaggedMbid.publishable());
        assert!(Confidence::TaggedIsrc.publishable());
        assert!(Confidence::Fingerprint { score: 0.9 }.publishable());
        assert!(!Confidence::Fuzzy { score: 0.6 }.publishable());
    }
}
