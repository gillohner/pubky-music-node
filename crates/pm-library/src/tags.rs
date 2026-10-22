//! Container probing and tag reading.
//!
//! FLAC, ALAC/M4A, WAV and AIFF go through symphonia 0.6's probe and its
//! metadata revisions; DSF and DFF go through [`crate::dsd`] and the `id3`
//! crate. WAV and AIFF additionally get their ID3v2 chunk read with the `id3`
//! crate, because that is where MusicBrainz ids live in those containers
//! (RIFF INFO has no fields for them).
//!
//! What symphonia 0.6 exposes (verified against symphonia-metadata 0.6.1):
//!
//! | container | key → `StandardTag` |
//! | :-- | :-- |
//! | FLAC (Vorbis comments) | `musicbrainz_trackid` → `MusicBrainzTrackId` (Picard writes the **recording** MBID here), `musicbrainz_recordingid` → `MusicBrainzRecordingId`, `musicbrainz_albumid` → `MusicBrainzAlbumId`, `musicbrainz_releasegroupid`, `musicbrainz_artistid`, `musicbrainz_albumartistid`, `isrc` → `IdentIsrc`, `barcode` → `IdentBarcode`, `upc`, `acoustid_id`, `replaygain_track_gain` |
//! | MP4 (iTunes freeform `----:com.apple.iTunes:…`) | `musicbrainz track id` → `MusicBrainzTrackId`, `musicbrainz album id`, `musicbrainz release group id`, `musicbrainz artist id`, `musicbrainz album artist id`, `isrc`, `barcode`, `acoustid id`, `replaygain_track_gain` |
//! | ID3v2 (AIFF chunk, and WAV when symphonia reads it) | `TSRC` → `IdentIsrc`; MusicBrainz ids are `TXXX` frames with a `DESCRIPTION` sub-field (`MusicBrainz Track Id`, …) which this module reads from the raw tag |
//! | RIFF INFO (WAV) | `INAM` title, `IART` artist, `IPRD` album, `ITRK` track, `ICRD` date; no identifiers |

use crate::dsd;
use crate::error::{LibraryError, Result};
use pm_model::{AudioFormat, Isrc, Mbid, Upc};
use std::fs::File;
use std::path::Path;
use symphonia::core::codecs::audio::well_known as codec;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, RawValue, StandardTag, Tag};

/// Everything the scanner wants from a file's tags.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagSet {
    pub title: Option<String>,
    /// Embedded cover art (front cover preferred), when the file has one.
    pub picture: Option<Picture>,
    pub artists: Vec<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub total_discs: Option<u32>,
    pub year: Option<i32>,
    pub recording_mbid: Option<Mbid>,
    pub release_mbid: Option<Mbid>,
    pub release_group_mbid: Option<Mbid>,
    pub artist_mbids: Vec<Mbid>,
    pub isrc: Option<Isrc>,
    pub upc: Option<Upc>,
    pub acoustid: Option<String>,
    pub replaygain_track_gain_db: Option<f32>,
    pub replaygain_album_gain_db: Option<f32>,
    /// Genres, split on `;`, `/`, `,` and NUL, trimmed, de-duplicated case-insensitively.
    pub genres: Vec<String>,
    /// Embedded lyrics (Vorbis `LYRICS`/`UNSYNCEDLYRICS`, ID3 `USLT`, MP4 `©lyr`).
    pub lyrics: Option<String>,
}

impl TagSet {
    /// Add genres from one tag value (which may list several).
    pub fn push_genre(&mut self, value: &str) {
        for g in value.split([';', '/', ',', '\0']) {
            let g = g.trim();
            if g.is_empty() || self.genres.iter().any(|x| x.eq_ignore_ascii_case(g)) {
                continue;
            }
            self.genres.push(g.to_string());
        }
    }
}

/// An embedded image.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Picture {
    pub mime: String,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Picture({} bytes, {})", self.data.len(), self.mime)
    }
}

impl Picture {
    /// File extension for the MIME type.
    pub fn extension(&self) -> &'static str {
        match self.mime.to_ascii_lowercase().as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "jpg",
        }
    }
}

/// What the container says about the audio.
#[derive(Debug, Clone, PartialEq)]
pub struct FileProbe {
    /// `flac`, `alac`, `wav`, `aiff`, `dsf`, `dff`.
    pub container: String,
    pub format: AudioFormat,
    pub duration_ms: Option<u64>,
}

/// Extensions the scanner accepts, lowercase.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "m4a", "alac", "wav", "wave", "aif", "aiff", "dsf", "dff", "mp3", "ogg", "oga",
];

pub fn is_supported(path: &Path) -> bool {
    extension(path)
        .map(|e| SUPPORTED_EXTENSIONS.contains(&e.as_str()))
        .unwrap_or(false)
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Probe the container and read its tags.
pub fn probe_and_read(path: &Path) -> Result<(FileProbe, TagSet)> {
    let ext = extension(path).ok_or_else(|| LibraryError::Unsupported(path.to_path_buf()))?;
    match ext.as_str() {
        "dsf" | "dff" => probe_dsd(path),
        "flac" | "m4a" | "alac" | "wav" | "wave" | "aif" | "aiff" | "mp3" | "ogg" | "oga" => {
            probe_symphonia(path, &ext)
        }
        _ => Err(LibraryError::Unsupported(path.to_path_buf())),
    }
}

fn probe_dsd(path: &Path) -> Result<(FileProbe, TagSet)> {
    let h = dsd::read_header(path)?;
    let probe = FileProbe {
        container: h.container.name().to_string(),
        format: h.format,
        duration_ms: h.duration_ms(),
    };
    let tags = match &h.id3 {
        Some(bytes) => match id3::Tag::read_from2(std::io::Cursor::new(bytes)) {
            Ok(t) => from_id3(&t),
            Err(e) => {
                tracing::warn!(path = %path.display(), "unreadable ID3 tag in DSD file: {e}");
                TagSet::default()
            }
        },
        None => TagSet::default(),
    };
    Ok((probe, tags))
}

fn probe_symphonia(path: &Path, ext: &str) -> Result<(FileProbe, TagSet)> {
    let file = File::open(path).map_err(|e| LibraryError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);
    let bad = |reason: String| LibraryError::Probe {
        path: path.to_path_buf(),
        reason,
    };
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| bad(e.to_string()))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| bad("no audio track".into()))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| bad("no audio codec parameters".into()))?;
    let rate = params
        .sample_rate
        .ok_or_else(|| bad("unknown sample rate".into()))?;
    let channels = params
        .channels
        .as_ref()
        .map(|c| c.count())
        .ok_or_else(|| bad("unknown channel count".into()))?;
    let bits = params
        .bits_per_sample
        .or(params.bits_per_coded_sample)
        .or_else(|| default_bits(params.codec))
        .ok_or_else(|| bad("unknown bit depth".into()))?;
    let audio_format = AudioFormat::pcm(rate, bits as u8, channels as u8);
    audio_format.validate().map_err(|e| bad(e.to_string()))?;

    let duration_ms = track.num_frames.map(|n| n * 1000 / rate as u64).or_else(|| {
        let tb = track.time_base?;
        let d = track.duration?;
        let ts = symphonia::core::units::Timestamp::from(i64::try_from(d.get()).ok()?);
        let t = tb.calc_time_saturating(ts);
        Some(t.as_millis().max(0) as u64)
    });

    let container = match params.codec {
        codec::CODEC_ID_FLAC => "flac",
        codec::CODEC_ID_ALAC => "alac",
        codec::CODEC_ID_MP3 => "mp3",
        codec::CODEC_ID_AAC => "aac",
        codec::CODEC_ID_VORBIS => "vorbis",
        _ => match format.format_info().short_name {
            "wav" | "wave" | "riff" => "wav",
            "aiff" | "aif" => "aiff",
            other => other,
        },
    }
    .to_string();

    let mut tags = TagSet::default();
    {
        let mut md = format.metadata();
        // Walk every revision, oldest first; later ones win on conflict.
        loop {
            if let Some(rev) = md.current() {
                merge_symphonia_tags(&mut tags, &rev.media.tags);
                merge_visuals(&mut tags, &rev.media.visuals);
                for pt in &rev.per_track {
                    merge_symphonia_tags(&mut tags, &pt.metadata.tags);
                    merge_visuals(&mut tags, &pt.metadata.visuals);
                }
            }
            if md.pop().is_none() {
                break;
            }
        }
    }

    // WAV/AIFF/MP3: the ID3v2 tag is authoritative for identifiers.
    if matches!(container.as_str(), "wav" | "aiff" | "mp3") {
        match id3::Tag::read_from_path(path) {
            Ok(t) => tags.merge_from(from_id3(&t)),
            Err(e) if matches!(e.kind, id3::ErrorKind::NoTag) => {}
            Err(e) => tracing::warn!(path = %path.display(), "unreadable ID3 chunk: {e}"),
        }
    }

    Ok((
        FileProbe {
            container,
            format: audio_format,
            duration_ms,
        },
        tags,
    ))
}

fn default_bits(id: symphonia::core::codecs::audio::AudioCodecId) -> Option<u32> {
    Some(match id {
        codec::CODEC_ID_PCM_S16LE | codec::CODEC_ID_PCM_S16BE => 16,
        codec::CODEC_ID_PCM_S24LE | codec::CODEC_ID_PCM_S24BE => 24,
        codec::CODEC_ID_PCM_S32LE | codec::CODEC_ID_PCM_S32BE => 32,
        codec::CODEC_ID_PCM_S8 => 8,
        // Lossy decoders produce float; we carry it as 24-bit PCM.
        codec::CODEC_ID_MP3 | codec::CODEC_ID_AAC | codec::CODEC_ID_VORBIS => 24,
        _ => return None,
    })
}

/// Containers whose audio went through a lossy codec.
pub fn is_lossy_container(container: &str) -> bool {
    matches!(container, "mp3" | "aac" | "vorbis" | "opus")
}

impl TagSet {
    /// Overlay `other` on `self`: set fields present in `other`.
    pub fn merge_from(&mut self, other: TagSet) {
        if other.picture.is_some() {
            self.picture = other.picture;
        }
        macro_rules! take {
            ($($f:ident),*) => { $( if other.$f.is_some() { self.$f = other.$f; } )* };
        }
        take!(
            title,
            album_artist,
            album,
            track_no,
            disc_no,
            total_discs,
            year,
            recording_mbid,
            release_mbid,
            release_group_mbid,
            isrc,
            upc,
            acoustid,
            replaygain_track_gain_db,
            replaygain_album_gain_db,
            lyrics
        );
        if !other.genres.is_empty() {
            self.genres = other.genres;
        }
        if !other.artists.is_empty() {
            self.artists = other.artists;
        }
        if !other.artist_mbids.is_empty() {
            self.artist_mbids = other.artist_mbids;
        }
    }

    fn set_mbid(slot: &mut Option<Mbid>, value: &str, what: &str) {
        match Mbid::parse(value) {
            Ok(m) => *slot = Some(m),
            Err(_) => tracing::warn!("ignoring invalid {what} MBID {value:?}"),
        }
    }

    fn push_artist_mbid(&mut self, value: &str) {
        for part in value.split(['/', ';', ',']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match Mbid::parse(part) {
                Ok(m) if !self.artist_mbids.contains(&m) => self.artist_mbids.push(m),
                Ok(_) => {}
                Err(_) => tracing::warn!("ignoring invalid artist MBID {part:?}"),
            }
        }
    }

    fn set_isrc(&mut self, value: &str) {
        match Isrc::parse(value) {
            Ok(i) => self.isrc = Some(i),
            Err(_) => tracing::warn!("ignoring invalid ISRC {value:?}"),
        }
    }

    fn set_upc(&mut self, value: &str) {
        match Upc::parse(value) {
            Ok(u) => self.upc = Some(u),
            Err(_) => tracing::warn!("ignoring invalid UPC {value:?}"),
        }
    }

    fn set_year_from(&mut self, value: &str) {
        if let Some(y) = parse_year(value) {
            self.year = Some(y);
        }
    }

    /// Apply one user-defined key/value (Vorbis comment name, iTunes freeform
    /// name without namespace, or ID3 TXXX description), case-insensitive.
    fn apply_named(&mut self, key: &str, value: &str) {
        let k = key.trim().to_ascii_lowercase().replace([' ', '_', '-'], "");
        match k.as_str() {
            "musicbrainzrecordingid" => Self::set_mbid(&mut self.recording_mbid, value, "recording"),
            // Picard's historical name for the recording id.
            "musicbrainztrackid" => {
                if self.recording_mbid.is_none() {
                    Self::set_mbid(&mut self.recording_mbid, value, "recording");
                }
            }
            "musicbrainzalbumid" => Self::set_mbid(&mut self.release_mbid, value, "release"),
            "musicbrainzreleasegroupid" => {
                Self::set_mbid(&mut self.release_group_mbid, value, "release group")
            }
            "musicbrainzartistid" => self.push_artist_mbid(value),
            "isrc" => self.set_isrc(value),
            "barcode" | "upc" => self.set_upc(value),
            "acoustidid" => self.acoustid = Some(value.trim().to_string()),
            "replaygaintrackgain" => self.replaygain_track_gain_db = parse_gain(value),
            "replaygainalbumgain" => self.replaygain_album_gain_db = parse_gain(value),
            "lyrics" | "unsyncedlyrics" | "unsynced lyrics" | "©lyr" if !value.trim().is_empty() => {
                self.lyrics = Some(value.to_string());
            }
            _ => {}
        }
    }
}

fn parse_gain(value: &str) -> Option<f32> {
    value
        .trim()
        .trim_end_matches("dB")
        .trim_end_matches("DB")
        .trim()
        .parse()
        .ok()
}

fn parse_year(value: &str) -> Option<i32> {
    let digits: String = value.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 4 {
        digits.parse().ok()
    } else {
        None
    }
}

fn raw_string(v: &RawValue) -> Option<String> {
    match v {
        RawValue::String(s) => Some(s.to_string()),
        RawValue::StringList(l) => l.first().cloned(),
        RawValue::UnsignedInt(n) => Some(n.to_string()),
        RawValue::SignedInt(n) => Some(n.to_string()),
        RawValue::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

/// Keep the front cover if there is one, else the first picture seen.
fn merge_visuals(tags: &mut TagSet, visuals: &[symphonia::core::meta::Visual]) {
    use symphonia::core::meta::StandardVisualKey;
    for v in visuals {
        if v.data.is_empty() {
            continue;
        }
        let front = matches!(v.usage, Some(StandardVisualKey::FrontCover));
        if tags.picture.is_none() || front {
            tags.picture = Some(Picture {
                mime: v.media_type.clone().unwrap_or_else(|| "image/jpeg".into()),
                data: v.data.to_vec(),
            });
            if front {
                return;
            }
        }
    }
}

fn merge_symphonia_tags(tags: &mut TagSet, list: &[Tag]) {
    for tag in list {
        if let Some(std) = &tag.std {
            match std {
                StandardTag::TrackTitle(s) => tags.title = Some(s.to_string()),
                StandardTag::Artist(s) => {
                    let s = s.to_string();
                    if !tags.artists.contains(&s) {
                        tags.artists.push(s);
                    }
                }
                StandardTag::AlbumArtist(s) => tags.album_artist = Some(s.to_string()),
                StandardTag::Album(s) => tags.album = Some(s.to_string()),
                StandardTag::TrackNumber(n) => tags.track_no = Some(*n as u32),
                StandardTag::DiscNumber(n) => tags.disc_no = Some(*n as u32),
                StandardTag::DiscTotal(n) => tags.total_discs = Some(*n as u32),
                StandardTag::ReleaseYear(y)
                | StandardTag::RecordingYear(y)
                | StandardTag::OriginalReleaseYear(y) => {
                    if tags.year.is_none() {
                        tags.year = Some(*y as i32);
                    }
                }
                StandardTag::ReleaseDate(s)
                | StandardTag::RecordingDate(s)
                | StandardTag::OriginalReleaseDate(s) => {
                    if tags.year.is_none() {
                        tags.set_year_from(s);
                    }
                }
                StandardTag::MusicBrainzRecordingId(s) => {
                    TagSet::set_mbid(&mut tags.recording_mbid, s, "recording")
                }
                StandardTag::MusicBrainzTrackId(s) => {
                    if tags.recording_mbid.is_none() {
                        TagSet::set_mbid(&mut tags.recording_mbid, s, "recording");
                    }
                }
                StandardTag::MusicBrainzAlbumId(s) => TagSet::set_mbid(&mut tags.release_mbid, s, "release"),
                StandardTag::MusicBrainzReleaseGroupId(s) => {
                    TagSet::set_mbid(&mut tags.release_group_mbid, s, "release group")
                }
                StandardTag::MusicBrainzArtistId(s) => tags.push_artist_mbid(s),
                StandardTag::IdentIsrc(s) => tags.set_isrc(s),
                StandardTag::IdentBarcode(s) | StandardTag::IdentUpc(s) | StandardTag::IdentEanUpn(s) => {
                    tags.set_upc(s)
                }
                StandardTag::AcoustIdId(s) => tags.acoustid = Some(s.to_string()),
                StandardTag::Genre(s) => tags.push_genre(s),
                StandardTag::Lyrics(s) => {
                    if !s.trim().is_empty() {
                        tags.lyrics = Some(s.to_string());
                    }
                }
                StandardTag::ReplayGainTrackGain(s) => tags.replaygain_track_gain_db = parse_gain(s),
                StandardTag::ReplayGainAlbumGain(s) => tags.replaygain_album_gain_db = parse_gain(s),
                _ => {}
            }
            continue;
        }
        // Raw-only tags: ID3 TXXX (key "TXXX" + DESCRIPTION sub-field) and
        // any Vorbis / iTunes key symphonia did not map.
        let Some(value) = raw_string(&tag.raw.value) else {
            continue;
        };
        let key = tag.raw.key.as_str();
        if key.eq_ignore_ascii_case("TXXX") {
            if let Some(desc) = tag
                .raw
                .sub_fields
                .as_ref()
                .and_then(|sf| sf.iter().find(|f| f.field.eq_ignore_ascii_case("DESCRIPTION")))
                .and_then(|f| raw_string(&f.value))
            {
                tags.apply_named(&desc, &value);
            }
        } else {
            let key = key.rsplit(':').next().unwrap_or(key);
            tags.apply_named(key, &value);
        }
    }
}

/// Build a [`TagSet`] from an ID3v2 tag (DSF, DFF, WAV, AIFF).
pub fn from_id3(t: &id3::Tag) -> TagSet {
    use id3::TagLike;
    let mut tags = TagSet {
        title: t.title().map(str::to_string),
        artists: t
            .artists()
            .map(|v| v.into_iter().map(str::to_string).collect())
            .unwrap_or_default(),
        album_artist: t.album_artist().map(str::to_string),
        album: t.album().map(str::to_string),
        track_no: t.track(),
        disc_no: t.disc(),
        total_discs: t.total_discs(),
        year: t
            .year()
            .or_else(|| t.date_recorded().map(|d| d.year))
            .or_else(|| t.date_released().map(|d| d.year))
            .or_else(|| t.original_date_released().map(|d| d.year)),
        ..TagSet::default()
    };
    if let Some(isrc) = t.text_for_frame_id("TSRC") {
        tags.set_isrc(isrc);
    }
    if let Some(g) = t.genre_parsed() {
        tags.push_genre(&g);
    }
    for et in t.extended_texts() {
        tags.apply_named(&et.description, &et.value);
    }
    if let Some(l) = t.lyrics().find(|l| !l.text.trim().is_empty()) {
        tags.lyrics = Some(l.text.clone());
    }
    for pic in t.pictures() {
        if pic.data.is_empty() {
            continue;
        }
        let front = pic.picture_type == id3::frame::PictureType::CoverFront;
        if tags.picture.is_none() || front {
            tags.picture = Some(Picture {
                mime: pic.mime_type.clone(),
                data: pic.data.clone(),
            });
            if front {
                break;
            }
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use id3::TagLike;

    const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    const MB2: &str = "0383dadf-2a4e-4d10-a46a-e9e041da8eb3";

    fn write_wav(path: &Path, bits: u16, frames: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            let v = ((i % 100) as i32 - 50) * 100;
            if bits == 16 {
                w.write_sample(v as i16).unwrap();
                w.write_sample(-v as i16).unwrap();
            } else {
                w.write_sample(v).unwrap();
                w.write_sample(-v).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    fn full_id3() -> id3::Tag {
        let mut t = id3::Tag::new();
        t.set_title("Get Lucky");
        t.set_artist("Daft Punk");
        t.set_album_artist("Daft Punk");
        t.set_album("Random Access Memories");
        t.set_track(8);
        t.set_disc(1);
        t.set_total_discs(1);
        t.set_year(2013);
        t.set_text("TSRC", "USQX91300108");
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Track Id".into(),
            value: MB.into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Album Id".into(),
            value: MB2.into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Release Group Id".into(),
            value: MB2.into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Artist Id".into(),
            value: MB2.into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "BARCODE".into(),
            value: "888837168618".into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "Acoustid Id".into(),
            value: "acoustid-xyz".into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "REPLAYGAIN_TRACK_GAIN".into(),
            value: "-7.35 dB".into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "REPLAYGAIN_ALBUM_GAIN".into(),
            value: "-6.10 dB".into(),
        });
        t
    }

    fn assert_full(tags: &TagSet) {
        assert_eq!(tags.title.as_deref(), Some("Get Lucky"));
        assert_eq!(tags.artists, vec!["Daft Punk".to_string()]);
        assert_eq!(tags.album.as_deref(), Some("Random Access Memories"));
        assert_eq!(tags.album_artist.as_deref(), Some("Daft Punk"));
        assert_eq!(tags.track_no, Some(8));
        assert_eq!(tags.disc_no, Some(1));
        assert_eq!(tags.total_discs, Some(1));
        assert_eq!(tags.year, Some(2013));
        assert_eq!(tags.recording_mbid, Some(Mbid::parse(MB).unwrap()));
        assert_eq!(tags.release_mbid, Some(Mbid::parse(MB2).unwrap()));
        assert_eq!(tags.release_group_mbid, Some(Mbid::parse(MB2).unwrap()));
        assert_eq!(tags.artist_mbids, vec![Mbid::parse(MB2).unwrap()]);
        assert_eq!(tags.isrc, Some(Isrc::parse("USQX91300108").unwrap()));
        assert_eq!(tags.upc, Some(Upc::parse("888837168618").unwrap()));
        assert_eq!(tags.acoustid.as_deref(), Some("acoustid-xyz"));
        assert_eq!(tags.replaygain_track_gain_db, Some(-7.35));
        assert_eq!(tags.replaygain_album_gain_db, Some(-6.10));
    }

    #[test]
    fn wav_with_id3_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.wav");
        write_wav(&p, 24, 4410);
        full_id3().write_to_path(&p, id3::Version::Id3v24).unwrap();
        let (probe, tags) = probe_and_read(&p).unwrap();
        assert_eq!(probe.container, "wav");
        assert_eq!(probe.format, AudioFormat::pcm(44_100, 24, 2));
        assert_eq!(probe.duration_ms, Some(100));
        assert_full(&tags);
    }

    #[test]
    fn wav_16_without_tags() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("plain.wav");
        write_wav(&p, 16, 441);
        let (probe, tags) = probe_and_read(&p).unwrap();
        assert_eq!(probe.format, AudioFormat::pcm(44_100, 16, 2));
        assert_eq!(probe.duration_ms, Some(10));
        assert_eq!(tags, TagSet::default());
    }

    #[test]
    fn dsf_and_dff_with_id3() {
        let dir = tempfile::tempdir().unwrap();
        let mut id3_bytes = Vec::new();
        full_id3().write_to(&mut id3_bytes, id3::Version::Id3v24).unwrap();
        let data = vec![0x55u8; 4096 * 2 * 2];
        let dsf = dir.path().join("t.dsf");
        std::fs::write(&dsf, dsd::synth::dsf(2_822_400, 2, 4096, &data, Some(&id3_bytes))).unwrap();
        let (probe, tags) = probe_and_read(&dsf).unwrap();
        assert_eq!(probe.container, "dsf");
        assert_eq!(probe.format, AudioFormat::dsd(2_822_400, 2));
        assert_full(&tags);

        let dff = dir.path().join("t.dff");
        std::fs::write(&dff, dsd::synth::dff(11_289_600, 2, &data, Some(&id3_bytes))).unwrap();
        let (probe, tags) = probe_and_read(&dff).unwrap();
        assert_eq!(probe.container, "dff");
        assert_eq!(probe.format, AudioFormat::dsd(11_289_600, 2));
        assert_full(&tags);
    }

    #[test]
    fn invalid_ids_are_dropped() {
        let mut t = id3::Tag::new();
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Track Id".into(),
            value: "not-a-uuid".into(),
        });
        t.set_text("TSRC", "bad");
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Recording Id".into(),
            value: MB.into(),
        });
        let tags = from_id3(&t);
        assert_eq!(tags.recording_mbid, Some(Mbid::parse(MB).unwrap()));
        assert!(tags.isrc.is_none());
    }

    #[test]
    fn recording_id_beats_track_id() {
        let mut t = id3::Tag::new();
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Track Id".into(),
            value: MB2.into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Recording Id".into(),
            value: MB.into(),
        });
        assert_eq!(from_id3(&t).recording_mbid, Some(Mbid::parse(MB).unwrap()));
    }

    #[test]
    fn unsupported_extension() {
        assert!(matches!(
            probe_and_read(Path::new("/x/y.wma")),
            Err(LibraryError::Unsupported(_))
        ));
        assert!(is_supported(Path::new("a.FLAC")));
        assert!(!is_supported(Path::new("a.txt")));
    }

    #[test]
    fn year_and_gain_parsing() {
        assert_eq!(parse_year("2013-05-17"), Some(2013));
        assert_eq!(parse_year("13"), None);
        assert_eq!(parse_gain("-7.35 dB"), Some(-7.35));
        assert_eq!(parse_gain("+1.0dB"), Some(1.0));
        assert_eq!(parse_gain("x"), None);
    }
}
