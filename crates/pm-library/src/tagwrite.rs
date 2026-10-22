//! "Write IDs into my files" (identify brief; off by default): after a file
//! is identified, its recording MBID (and release MBID, AcoustID id when
//! known) can be written into its own tags, so other players and taggers see
//! them. Before anything changes, the tags the write touches are saved to a
//! JSON backup next to the node's data, and only the ID fields change: the
//! audio frames are never rewritten.
//!
//! Formats: FLAC (Vorbis comments, the names Picard uses), WAV and AIFF (ID3
//! `TXXX` frames, as the scanner reads them). ALAC/MP4 and DSD are reported as
//! not supported rather than risked.

use id3::TagLike;
use pm_model::Mbid;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What to write.
#[derive(Debug, Clone, PartialEq)]
pub struct Ids {
    pub recording: Mbid,
    pub release: Option<Mbid>,
    pub acoustid: Option<String>,
}

/// Fixes for a file's tags (owner, 2026-10-22: "a writer that fixes this"): the IDs a match
/// found, and the artists as the node knows them, one value each (`ARTISTS`, with their
/// MusicBrainz ids when known), so other players see "Sefa" and "Dr. Peacock", not one artist
/// called "Sefa, Dr. Peacock". The display credit (`ARTIST`/`TPE1`) is left as it is.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fixes {
    pub ids: Option<Ids>,
    pub artists: Vec<String>,
    /// One per artist, in the same order, or empty.
    pub artist_mbids: Vec<Mbid>,
}

/// The saved state of a file's ID tags before a write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Backup {
    pub path: PathBuf,
    /// `flac`, `wav` or `aiff`.
    pub format: String,
    pub saved_at: i64,
    /// FLAC: every Vorbis comment as it was (key, value), in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vorbis: Vec<(String, String)>,
    /// ID3: the `TXXX` frames the write touches, `None` when absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub txxx: Vec<(String, Option<String>)>,
    /// Whether the file had an ID3 tag at all.
    #[serde(default)]
    pub had_id3: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("{0}: writing IDs into this format is not supported")]
    Unsupported(String),
    #[error("{path}: {reason}")]
    Failed { path: PathBuf, reason: String },
}

fn failed(path: &Path, e: impl std::fmt::Display) -> WriteError {
    WriteError::Failed {
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}

const FLAC_KEYS: [&str; 5] = [
    "MUSICBRAINZ_TRACKID",
    "MUSICBRAINZ_ALBUMID",
    "ACOUSTID_ID",
    "ARTISTS",
    "MUSICBRAINZ_ARTISTID",
];
const TXXX_KEYS: [&str; 5] = [
    "MusicBrainz Track Id",
    "MusicBrainz Album Id",
    "Acoustid Id",
    "ARTISTS",
    "MusicBrainz Artist Id",
];

fn format_of(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "flac" => Some("flac"),
        "wav" | "wave" => Some("wav"),
        "aif" | "aiff" | "aifc" => Some("aiff"),
        "mp3" => Some("mp3"),
        _ => None,
    }
}

/// The values per key (several for the artist keys); empty = leave that key alone.
fn values(f: &Fixes) -> [Vec<String>; 5] {
    let ids = f.ids.as_ref();
    let artist_ids_fit = !f.artists.is_empty() && f.artist_mbids.len() == f.artists.len();
    [
        ids.map(|i| i.recording.to_string()).into_iter().collect(),
        ids.and_then(|i| i.release)
            .map(|r| r.to_string())
            .into_iter()
            .collect(),
        ids.and_then(|i| i.acoustid.clone()).into_iter().collect(),
        // One artist needs no ARTISTS tag (the display credit already says it)
        if f.artists.len() > 1 {
            f.artists.clone()
        } else {
            Vec::new()
        },
        if artist_ids_fit {
            f.artist_mbids.iter().map(|m| m.to_string()).collect()
        } else {
            Vec::new()
        },
    ]
}

/// Write `ids` into the file at `path`, after saving a backup under
/// `backup_dir`. Returns the backup's path.
pub fn write_ids(path: &Path, ids: &Ids, backup_dir: &Path) -> Result<PathBuf, WriteError> {
    write_fixes(
        path,
        &Fixes {
            ids: Some(ids.clone()),
            ..Default::default()
        },
        backup_dir,
    )
}

/// One tag a fix changes: `(key, values now, values after)`.
pub type Change = (String, Vec<String>, Vec<String>);

/// What [`write_fixes`] would change in the file: `(key, now, then)` for each key that differs.
pub fn preview(path: &Path, f: &Fixes) -> Result<Vec<Change>, WriteError> {
    let format = format_of(path).ok_or_else(|| WriteError::Unsupported(path.display().to_string()))?;
    let vals = values(f);
    let mut out = Vec::new();
    if format == "flac" {
        let tag = metaflac::Tag::read_from_path(path).map_err(|e| failed(path, e))?;
        for (k, v) in FLAC_KEYS.iter().zip(vals.iter()) {
            if v.is_empty() {
                continue;
            }
            let now: Vec<String> = tag
                .get_vorbis(k)
                .map(|i| i.map(str::to_string).collect())
                .unwrap_or_default();
            if &now != v {
                out.push(((*k).to_string(), now, v.clone()));
            }
        }
    } else {
        let tag = id3::Tag::read_from_path(path).unwrap_or_default();
        for (d, v) in TXXX_KEYS.iter().zip(vals.iter()) {
            if v.is_empty() {
                continue;
            }
            let now: Vec<String> = tag
                .extended_texts()
                .find(|t| t.description == *d)
                .map(|t| t.value.split('\0').map(str::to_string).collect())
                .unwrap_or_default();
            if &now != v {
                out.push(((*d).to_string(), now, v.clone()));
            }
        }
    }
    Ok(out)
}

/// Write `f` into the file at `path`, after saving a backup under `backup_dir` (only the keys
/// it touches; the audio is never rewritten). Returns the backup's path.
pub fn write_fixes(path: &Path, f: &Fixes, backup_dir: &Path) -> Result<PathBuf, WriteError> {
    let format = format_of(path).ok_or_else(|| WriteError::Unsupported(path.display().to_string()))?;
    std::fs::create_dir_all(backup_dir).map_err(|e| failed(backup_dir, e))?;
    let vals = values(f);
    let mut backup = Backup {
        path: path.to_path_buf(),
        format: format.into(),
        saved_at: crate::db::now_unix(),
        vorbis: Vec::new(),
        txxx: Vec::new(),
        had_id3: false,
    };
    match format {
        "flac" => {
            let mut tag = metaflac::Tag::read_from_path(path).map_err(|e| failed(path, e))?;
            if let Some(vc) = tag.vorbis_comments() {
                for (k, vs) in &vc.comments {
                    for v in vs {
                        backup.vorbis.push((k.clone(), v.clone()));
                    }
                }
            }
            let bpath = save_backup(&backup, backup_dir)?;
            for (k, v) in FLAC_KEYS.iter().zip(vals.iter()) {
                if !v.is_empty() {
                    tag.set_vorbis(*k, v.clone());
                }
            }
            tag.save().map_err(|e| failed(path, e))?;
            Ok(bpath)
        }
        _ => {
            let mut tag = match id3::Tag::read_from_path(path) {
                Ok(t) => {
                    backup.had_id3 = true;
                    t
                }
                Err(e) if matches!(e.kind, id3::ErrorKind::NoTag) => id3::Tag::new(),
                Err(e) => return Err(failed(path, e)),
            };
            for d in TXXX_KEYS {
                let cur = tag
                    .extended_texts()
                    .find(|t| t.description == d)
                    .map(|t| t.value.clone());
                backup.txxx.push((d.to_string(), cur));
            }
            let bpath = save_backup(&backup, backup_dir)?;
            for (d, v) in TXXX_KEYS.iter().zip(vals.iter()) {
                if !v.is_empty() {
                    tag.remove_extended_text(Some(d), None);
                    // ID3v2.4: several values in one frame, NUL-separated (as Picard writes them)
                    tag.add_frame(id3::frame::ExtendedText {
                        description: (*d).into(),
                        value: v.join("\0"),
                    });
                }
            }
            tag.write_to_path(path, id3::Version::Id3v24)
                .map_err(|e| failed(path, e))?;
            Ok(bpath)
        }
    }
}

fn save_backup(b: &Backup, dir: &Path) -> Result<PathBuf, WriteError> {
    let name = format!(
        "{}-{}.json",
        &blake3::hash(b.path.to_string_lossy().as_bytes()).to_hex()[..16],
        b.saved_at
    );
    let p = dir.join(name);
    let json = serde_json::to_vec_pretty(b).map_err(|e| failed(&p, e))?;
    std::fs::write(&p, json).map_err(|e| failed(&p, e))?;
    Ok(p)
}

/// Put the ID tags back as the backup has them.
pub fn restore(backup: &Backup) -> Result<(), WriteError> {
    let path = &backup.path;
    match backup.format.as_str() {
        "flac" => {
            let mut tag = metaflac::Tag::read_from_path(path).map_err(|e| failed(path, e))?;
            for k in FLAC_KEYS {
                tag.remove_vorbis(k);
            }
            for k in FLAC_KEYS {
                let vs: Vec<String> = backup
                    .vorbis
                    .iter()
                    .filter(|(bk, _)| bk.eq_ignore_ascii_case(k))
                    .map(|(_, v)| v.clone())
                    .collect();
                if !vs.is_empty() {
                    tag.set_vorbis(k, vs);
                }
            }
            tag.save().map_err(|e| failed(path, e))
        }
        "wav" | "aiff" | "mp3" => {
            let mut tag = id3::Tag::read_from_path(path).map_err(|e| failed(path, e))?;
            for (d, v) in &backup.txxx {
                tag.remove_extended_text(Some(d), None);
                if let Some(v) = v {
                    tag.add_frame(id3::frame::ExtendedText {
                        description: d.clone(),
                        value: v.clone(),
                    });
                }
            }
            tag.write_to_path(path, id3::Version::Id3v24)
                .map_err(|e| failed(path, e))
        }
        other => Err(WriteError::Unsupported(other.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    const REL: &str = "1a5b6a49-4c4f-3d18-9e3d-1c4d1b0a7a3e";

    fn ids() -> Ids {
        Ids {
            recording: Mbid::parse(MB).unwrap(),
            release: Some(Mbid::parse(REL).unwrap()),
            acoustid: Some("84eed5cd-5b1f-4f5c-8c92-2686d2877862".into()),
        }
    }

    /// Decoded samples, to prove the audio is untouched.
    fn samples(path: &Path) -> Vec<i32> {
        use symphonia::core::codecs::audio::AudioDecoderOptions;
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::formats::{FormatOptions, TrackType};
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        let mss = MediaSourceStream::new(Box::new(std::fs::File::open(path).unwrap()), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(path.extension().unwrap().to_str().unwrap());
        let mut fmt = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .unwrap();
        let track = fmt.default_track(TrackType::Audio).unwrap();
        let id = track.id;
        let params = track.codec_params.as_ref().unwrap().audio().unwrap().clone();
        let mut dec = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .unwrap();
        let mut out = Vec::new();
        while let Ok(Some(p)) = fmt.next_packet() {
            if p.track_id != id {
                continue;
            }
            let buf = dec.decode(&p).unwrap();
            let mut v = Vec::new();
            buf.copy_to_vec_interleaved::<i32>(&mut v);
            out.extend(v);
        }
        out
    }

    #[test]
    fn flac_ids_are_written_with_a_backup_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.flac");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/ietf-60-mono-audio.flac"
            ),
            &f,
        )
        .unwrap();
        let before = samples(&f);
        let (_, tags_before) = crate::probe_and_read(&f).unwrap();
        assert!(tags_before.recording_mbid.is_none());
        let bpath = write_ids(&f, &ids(), &dir.path().join("backups")).unwrap();
        assert!(bpath.exists());
        let (_, tags) = crate::probe_and_read(&f).unwrap();
        assert_eq!(
            tags.recording_mbid.unwrap().to_string(),
            MB,
            "the scanner reads it back"
        );
        assert_eq!(samples(&f), before, "audio untouched");
        let b: Backup = serde_json::from_slice(&std::fs::read(&bpath).unwrap()).unwrap();
        assert!(b
            .vorbis
            .iter()
            .all(|(k, _)| !k.eq_ignore_ascii_case("MUSICBRAINZ_TRACKID")));
        restore(&b).unwrap();
        let (_, back) = crate::probe_and_read(&f).unwrap();
        assert!(back.recording_mbid.is_none(), "restored");
        assert_eq!(samples(&f), before);
    }

    /// The artists written one value each: the scanner reads them back as separate artists, the
    /// preview says what changes, and the backup puts the file back.
    #[test]
    fn flac_artists_are_written_one_per_value() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.flac");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/ietf-60-mono-audio.flac"
            ),
            &f,
        )
        .unwrap();
        let before = samples(&f);
        let fixes = Fixes {
            ids: None,
            artists: vec!["Sefa".into(), "Dr. Peacock".into()],
            artist_mbids: vec![],
        };
        let p = preview(&f, &fixes).unwrap();
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].0, "ARTISTS");
        assert_eq!(p[0].2, vec!["Sefa", "Dr. Peacock"]);
        let bpath = write_fixes(&f, &fixes, &dir.path().join("backups")).unwrap();
        let (_, tags) = crate::probe_and_read(&f).unwrap();
        assert_eq!(tags.artist_list, vec!["Sefa", "Dr. Peacock"]);
        assert!(preview(&f, &fixes).unwrap().is_empty(), "nothing left to change");
        assert_eq!(samples(&f), before, "audio untouched");
        let b: Backup = serde_json::from_slice(&std::fs::read(&bpath).unwrap()).unwrap();
        restore(&b).unwrap();
        let (_, back) = crate::probe_and_read(&f).unwrap();
        assert!(back.artist_list.is_empty(), "restored");
    }

    #[test]
    fn mp3_artists_go_into_one_id3_frame() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.mp3");
        std::fs::write(&f, b"").unwrap();
        let fixes = Fixes {
            ids: None,
            artists: vec!["Sefa".into(), "Cyclon".into()],
            artist_mbids: vec![],
        };
        write_fixes(&f, &fixes, &dir.path().join("backups")).unwrap();
        let t = id3::Tag::read_from_path(&f).unwrap();
        let v = t
            .extended_texts()
            .find(|x| x.description == "ARTISTS")
            .unwrap()
            .value
            .clone();
        assert_eq!(v, "Sefa\0Cyclon");
        let mut back = crate::tags::TagSet::default();
        for et in t.extended_texts() {
            back.merge_from(crate::tags::from_id3(&t));
            let _ = et;
        }
        assert_eq!(back.artist_list, vec!["Sefa", "Cyclon"]);
    }

    #[test]
    fn wav_ids_go_into_an_id3_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&f, spec).unwrap();
        for i in 0..44_100 {
            let v = ((i as f64 * 0.05).sin() * 9000.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        let before = samples(&f);
        write_ids(&f, &ids(), &dir.path().join("backups")).unwrap();
        let tag = id3::Tag::read_from_path(&f).unwrap();
        let mb = tag
            .extended_texts()
            .find(|t| t.description == "MusicBrainz Track Id")
            .map(|t| t.value.clone());
        assert_eq!(mb.as_deref(), Some(MB));
        assert_eq!(samples(&f), before, "audio untouched");
    }

    #[test]
    fn other_formats_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.m4a");
        std::fs::write(&f, b"x").unwrap();
        assert!(matches!(
            write_ids(&f, &ids(), dir.path()),
            Err(WriteError::Unsupported(_))
        ));
        assert!(
            std::fs::read_dir(dir.path()).unwrap().count() == 1,
            "no backup written"
        );
    }
}
