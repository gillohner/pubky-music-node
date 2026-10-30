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
    /// An AcoustID fingerprint match with this score (0–1): one recording
    /// at or above [`Confidence::FINGERPRINT_ACCEPT`].
    Fingerprint { score: f32 },
    /// A fingerprint match disambiguated by the folder as a release (track
    /// count, position and duration fit one MusicBrainz release).
    #[serde(rename = "fingerprint+release")]
    FingerprintRelease { score: f32 },
    /// The file's folder fits one MusicBrainz release (album, artist, and each file's position,
    /// length and title), with this score (0–1) for how well the folder covers it.
    AlbumFit { score: f32 },
    /// The owner confirmed a suggestion.
    ConfirmedByUser,
    /// Artist / title / duration match with this score (0–1). A suggestion
    /// only: never canonical, never published (identify brief).
    Fuzzy { score: f32 },
}

impl Confidence {
    /// Below this, a canonical URI is kept locally but never published.
    pub const PUBLISH_THRESHOLD: f32 = 0.85;
    /// AcoustID results at or above this score count as a match.
    pub const FINGERPRINT_ACCEPT: f32 = 0.9;

    pub fn score(&self) -> f32 {
        match self {
            Confidence::TaggedMbid | Confidence::ConfirmedByUser => 1.0,
            Confidence::TaggedIsrc => 0.95,
            Confidence::Fingerprint { score }
            | Confidence::FingerprintRelease { score }
            | Confidence::AlbumFit { score }
            | Confidence::Fuzzy { score } => *score,
        }
    }

    /// Whether this identity may be used for social data (tags, playlists,
    /// activity, reviews) and Pinakes lookups: everything but a fuzzy
    /// suggestion, at or above the publish threshold.
    pub fn publishable(&self) -> bool {
        !matches!(self, Confidence::Fuzzy { .. }) && self.score() >= Self::PUBLISH_THRESHOLD
    }

    /// The identify brief's name for this level: `tagged_mbid`,
    /// `tagged_isrc`, `fingerprint`, `fingerprint+release`, `album_fit`,
    /// `confirmed_by_user`, `suggested`.
    pub fn label(&self) -> &'static str {
        match self {
            Confidence::TaggedMbid => "tagged_mbid",
            Confidence::TaggedIsrc => "tagged_isrc",
            Confidence::Fingerprint { .. } => "fingerprint",
            Confidence::FingerprintRelease { .. } => "fingerprint+release",
            Confidence::AlbumFit { .. } => "album_fit",
            Confidence::ConfirmedByUser => "confirmed_by_user",
            Confidence::Fuzzy { .. } => "suggested",
        }
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
