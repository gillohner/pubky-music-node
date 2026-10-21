//! The folder scanner: walks a root, probes new or changed files, upserts
//! them into the catalogue, and flags files that disappeared.
//!
//! Change detection is by `(size_bytes, mtime_unix)` where the mtime is the
//! newer of the audio file and its sidecar `.lrc`; an unchanged file is
//! skipped without opening it. Files that vanish are **kept** with
//! `files.missing = 1` (see the schema) so that playlists, history and
//! canonical-URI lookups still resolve; a later scan that finds the path
//! again clears the flag.

use crate::canonical;
use crate::db::{Library, ScanCounts};
use crate::error::{LibraryError, Result};
use crate::tags::{self, TagSet};
use pm_model::{Album, Artist, AudioFile, Track, TrackId};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub scan_id: i64,
    pub files_seen: u64,
    pub files_added: u64,
    pub files_updated: u64,
    pub files_removed: u64,
    pub files_unchanged: u64,
    /// Files that could not be read, with the reason. The scan continues past them.
    pub errors: Vec<(PathBuf, String)>,
}

pub struct Scanner<'a> {
    lib: &'a Library,
    /// Where extracted embedded covers are written; `None` skips cover art.
    art_dir: Option<PathBuf>,
}

/// Folder image names tried, in order, when a file has no embedded picture.
const FOLDER_COVERS: &[&str] = &["cover", "folder", "front", "album", "artwork"];

impl<'a> Scanner<'a> {
    pub fn new(lib: &'a Library) -> Self {
        Self { lib, art_dir: None }
    }

    /// Extract embedded covers into `dir` (created on demand) and pick up
    /// `cover.jpg`-style images next to the files.
    pub fn with_art_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.art_dir = Some(dir.into());
        self
    }

    /// Give the album its cover if it has none yet: the file's embedded
    /// picture, else an image in the file's folder.
    fn cover_for(&self, path: &Path, album_id: i64, tags: &TagSet) -> Result<()> {
        let Some(art_dir) = &self.art_dir else {
            return Ok(());
        };
        if self.lib.album_art(album_id)?.is_some() {
            return Ok(());
        }
        if let Some(pic) = &tags.picture {
            std::fs::create_dir_all(art_dir).map_err(|e| LibraryError::Io {
                path: art_dir.clone(),
                source: e,
            })?;
            let hash = blake3::hash(&pic.data).to_hex();
            let file = art_dir.join(format!("{}.{}", &hash[..32], pic.extension()));
            if !file.exists() {
                std::fs::write(&file, &pic.data).map_err(|e| LibraryError::Io {
                    path: file.clone(),
                    source: e,
                })?;
            }
            return self.lib.set_album_art(&crate::db::AlbumArt {
                album_id,
                file,
                mime: pic.mime.clone(),
                source: "embedded".into(),
            });
        }
        if let Some((file, mime)) = folder_cover(path) {
            return self.lib.set_album_art(&crate::db::AlbumArt {
                album_id,
                file,
                mime,
                source: "folder".into(),
            });
        }
        Ok(())
    }

    pub fn scan(&self, root: &Path) -> Result<ScanReport> {
        let root = root.canonicalize().map_err(|e| LibraryError::Io {
            path: root.to_path_buf(),
            source: e,
        })?;
        let scan_id = self.lib.begin_scan(&root)?;
        let mut report = ScanReport {
            scan_id,
            ..Default::default()
        };

        for entry in WalkDir::new(&root).follow_links(true).into_iter() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    let p = e.path().map(Path::to_path_buf).unwrap_or_else(|| root.clone());
                    report.errors.push((p, e.to_string()));
                    continue;
                }
            };
            if !entry.file_type().is_file() || !tags::is_supported(entry.path()) {
                continue;
            }
            report.files_seen += 1;
            let path = entry.path().to_path_buf();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    report.errors.push((path, e.to_string()));
                    continue;
                }
            };
            let size = meta.len();
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // A sidecar .lrc is part of the fingerprint: adding, editing or
            // removing one re-reads the track without touching the audio.
            let mtime = mtime.max(crate::lyrics::sidecar_mtime(&path).unwrap_or(0));

            let existing = self.lib.file_by_path(&path)?;
            if let Some(row) = &existing {
                if row.size_bytes == size && row.mtime_unix == mtime {
                    self.lib.mark_file_seen(&path, scan_id)?;
                    report.files_unchanged += 1;
                    continue;
                }
            }
            match self.index_file(&path, size, mtime, existing.as_ref().map(|r| r.track_id)) {
                Ok(_) => {
                    self.lib.mark_file_seen(&path, scan_id)?;
                    if existing.is_some() {
                        report.files_updated += 1;
                    } else {
                        report.files_added += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), "skipping file: {e}");
                    report.errors.push((path, e.to_string()));
                }
            }
        }

        report.files_removed = self.lib.mark_missing_under(&root, scan_id)?;
        self.lib.finish_scan(
            scan_id,
            &ScanCounts {
                files_seen: report.files_seen,
                files_added: report.files_added,
                files_updated: report.files_updated,
                files_removed: report.files_removed,
            },
        )?;
        Ok(report)
    }

    fn index_file(&self, path: &Path, size: u64, mtime: i64, existing: Option<TrackId>) -> Result<TrackId> {
        let (probe, tagset) = tags::probe_and_read(path)?;
        let has_album = tagset.album.is_some();
        let file = AudioFile {
            path: path.to_path_buf(),
            size_bytes: size,
            mtime_unix: mtime,
            container: probe.container,
            format: probe.format,
            duration_ms: probe.duration_ms,
        };
        let mut track = track_from_tags(file, &tagset);
        track.id = existing;
        let id = self.lib.upsert_track(&track)?;
        // A sidecar .lrc (usually synchronised) wins over embedded text.
        let lyrics = crate::lyrics::sidecar(path)
            .map(|t| (t, "lrc"))
            .or_else(|| tagset.lyrics.clone().map(|t| (t, "tag")));
        self.lib
            .set_lyrics(id, lyrics.as_ref().map(|(t, s)| (t.as_str(), *s)))?;
        if has_album && self.art_dir.is_some() {
            if let Some(album_id) = self.lib.get_track(id)?.and_then(|t| t.album.and_then(|a| a.id)) {
                if let Err(e) = self.cover_for(path, album_id, &tagset) {
                    tracing::warn!(path = %path.display(), "cover art: {e}");
                }
            }
        }
        Ok(id)
    }
}

/// `cover.jpg`, `Folder.png`, … next to `audio_path`, case-insensitive.
/// A title for a file without tags (owner: "Most played shows test-fixture file names"): its
/// name without the leading track number, with spaces for underscores, first letter capital.
/// `14_wasted_bits` → `Wasted bits`, `03 - Untagged Song` → `Untagged Song`.
pub fn title_from_file_name(stem: &str) -> String {
    let spaced: String = stem.replace('_', " ");
    let trimmed = spaced.trim();
    // A leading track number: "03 - ", "14 ", "1. "
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    let rest = if (1..=3).contains(&digits) {
        let r = trimmed[digits..].trim_start_matches([' ', '-', '.', '_']).trim();
        if r.is_empty() {
            trimmed
        } else {
            r
        }
    } else {
        trimmed
    };
    let words = rest.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut c = words.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

pub fn folder_cover(audio_path: &Path) -> Option<(PathBuf, String)> {
    let dir = audio_path.parent()?;
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(usize, PathBuf, String)> = None;
    for e in entries.flatten() {
        let p = e.path();
        let Some(stem) = p
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
        else {
            continue;
        };
        let Some(ext) = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
        else {
            continue;
        };
        let mime = match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "webp" => "image/webp",
            _ => continue,
        };
        let Some(rank) = FOLDER_COVERS.iter().position(|n| *n == stem) else {
            continue;
        };
        if best.as_ref().is_none_or(|(r, _, _)| rank < *r) {
            best = Some((rank, p, mime.to_string()));
        }
    }
    best.map(|(_, p, m)| (p, m))
}

/// Build a [`Track`] from a probed file and its tags. A missing title falls
/// back to the file stem.
pub fn track_from_tags(file: AudioFile, t: &TagSet) -> Track {
    let title = t
        .title
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            file.path
                .file_stem()
                .map(|s| title_from_file_name(&s.to_string_lossy()))
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "Untitled".into())
        });
    let artists = t
        .artists
        .iter()
        .enumerate()
        .map(|(i, name)| Artist {
            id: None,
            name: name.clone(),
            mbid: t.artist_mbids.get(i).copied(),
        })
        .collect();
    let album = t.album.as_ref().map(|title| Album {
        id: None,
        title: title.clone(),
        album_artist: t.album_artist.clone().or_else(|| t.artists.first().cloned()),
        release_mbid: t.release_mbid,
        release_group_mbid: t.release_group_mbid,
        year: t.year,
        upc: t.upc.clone(),
        total_discs: t.total_discs,
    });
    Track {
        id: None,
        title,
        artists,
        album,
        track_no: t.track_no,
        disc_no: t.disc_no,
        duration_ms: file.duration_ms,
        file,
        recording_mbid: t.recording_mbid,
        isrc: t.isrc.clone(),
        acoustid: t.acoustid.clone(),
        canonical: canonical::assign(t),
        replaygain_track_gain_db: t.replaygain_track_gain_db,
        replaygain_album_gain_db: t.replaygain_album_gain_db,
        genres: t.genres.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use id3::TagLike;
    use pm_model::{Confidence, Mbid, MusicUri};
    use std::fs;

    const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";

    fn write_wav(path: &Path, frames: u32, title: &str, with_mbid: bool) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            w.write_sample((i % 50) as i16).unwrap();
            w.write_sample(-((i % 50) as i16)).unwrap();
        }
        w.finalize().unwrap();
        let mut t = id3::Tag::new();
        t.set_title(title);
        t.set_artist("Tester");
        t.set_album("Scans");
        if with_mbid {
            t.add_frame(id3::frame::ExtendedText {
                description: "MusicBrainz Track Id".into(),
                value: MB.into(),
            });
        }
        t.write_to_path(path, id3::Version::Id3v24).unwrap();
    }

    #[test]
    fn scan_rescan_modify_delete() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("album");
        fs::create_dir(&sub).unwrap();
        let a = sub.join("01 - one.wav");
        let b = sub.join("02 - two.wav");
        write_wav(&a, 4410, "One", true);
        write_wav(&b, 2205, "Two", false);
        fs::write(sub.join("cover.jpg"), b"not audio").unwrap();
        fs::write(sub.join("broken.flac"), b"fLaC garbage").unwrap();

        let lib = Library::open_in_memory().unwrap();
        let scanner = Scanner::new(&lib);
        let r = scanner.scan(dir.path()).unwrap();
        assert_eq!(r.files_seen, 3);
        assert_eq!(r.files_added, 2);
        assert_eq!(r.files_updated, 0);
        assert_eq!(r.files_removed, 0);
        assert_eq!(r.errors.len(), 1);
        assert!(r.errors[0].0.ends_with("broken.flac"));
        let stats = lib.stats().unwrap();
        assert_eq!(stats.tracks, 2);
        assert_eq!(stats.albums, 1);
        assert_eq!(stats.artists, 1);
        assert_eq!(stats.total_duration_ms, 100 + 50);

        let one = &lib.search_tracks("one", 5).unwrap()[0];
        assert_eq!(one.title, "One");
        assert_eq!(one.file.format.bits, 16);
        assert_eq!(
            one.canonical,
            Some((
                MusicUri::Recording {
                    mbid: Mbid::parse(MB).unwrap()
                },
                Confidence::TaggedMbid
            ))
        );
        assert!(one.publishable_uri().is_some());
        let two = &lib.search_tracks("two", 5).unwrap()[0];
        assert!(two.canonical.is_none());

        // Re-scan: nothing changes.
        let r = scanner.scan(dir.path()).unwrap();
        assert_eq!(
            (r.files_added, r.files_updated, r.files_removed, r.files_unchanged),
            (0, 0, 0, 2)
        );

        // Modify one file (size changes), delete the other.
        write_wav(&a, 8820, "One (remaster)", true);
        fs::remove_file(&b).unwrap();
        let r = scanner.scan(dir.path()).unwrap();
        assert_eq!((r.files_added, r.files_updated, r.files_removed), (0, 1, 1));
        assert_eq!(lib.search_tracks("remaster", 5).unwrap().len(), 1);
        let stats = lib.stats().unwrap();
        assert_eq!(stats.tracks, 2);
        assert_eq!(stats.files, 1);
        assert_eq!(stats.missing_files, 1);
        assert!(lib.file_by_path(&b).unwrap().unwrap().missing);

        // The file comes back: flag clears.
        write_wav(&b, 2205, "Two", false);
        // Same size and (second-resolution) mtime count as unchanged; either
        // way the missing flag clears.
        let r = scanner.scan(dir.path()).unwrap();
        assert_eq!((r.files_added, r.files_removed), (0, 0));
        assert_eq!(r.files_updated + r.files_unchanged, 2);
        assert_eq!(lib.stats().unwrap().missing_files, 0);
        assert!(!lib.file_by_path(&b).unwrap().unwrap().missing);

        // A sidecar .lrc that is newer than the audio re-indexes the track
        // and lands as lyrics; removing it clears them again.
        let id = lib.file_by_path(&a).unwrap().unwrap().track_id;
        assert!(lib.lyrics(id).unwrap().is_none());
        let lrc = sub.join("01 - one.lrc");
        fs::write(&lrc, "[00:01.00]Hello").unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::open(&lrc).unwrap().set_modified(future).unwrap();
        let r = scanner.scan(dir.path()).unwrap();
        assert_eq!(r.files_updated, 1);
        assert_eq!(
            lib.lyrics(id).unwrap(),
            Some(("[00:01.00]Hello".to_string(), "lrc".to_string()))
        );
        fs::remove_file(&lrc).unwrap();
        let r = scanner.scan(dir.path()).unwrap();
        assert_eq!(r.files_updated, 1);
        assert!(lib.lyrics(id).unwrap().is_none());
    }

    #[test]
    fn covers_from_tags_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        // Album A: embedded front cover in the ID3 tag.
        let wav = a.join("01.wav");
        write_wav(&wav, 441, "A1", false);
        let mut t = id3::Tag::read_from_path(&wav).unwrap();
        t.set_album("Album A");
        t.add_frame(id3::frame::Picture {
            mime_type: "image/png".into(),
            picture_type: id3::frame::PictureType::CoverFront,
            description: String::new(),
            data: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
        });
        t.write_to_path(&wav, id3::Version::Id3v24).unwrap();
        // Album B: no picture, a Folder.jpg beside the file.
        let wav_b = b.join("01.wav");
        write_wav(&wav_b, 441, "B1", false);
        let mut t = id3::Tag::read_from_path(&wav_b).unwrap();
        t.set_album("Album B");
        t.write_to_path(&wav_b, id3::Version::Id3v24).unwrap();
        fs::write(b.join("Folder.jpg"), b"\xff\xd8jpeg").unwrap();
        fs::write(b.join("back.png"), b"png").unwrap();

        let lib = Library::open_in_memory().unwrap();
        let art = dir.path().join("art");
        Scanner::new(&lib).with_art_dir(&art).scan(dir.path()).unwrap();
        let albums = lib.list_albums(None, 10).unwrap();
        let find = |title: &str| albums.iter().find(|x| x.title == title).unwrap().id.unwrap();
        let art_a = lib.album_art(find("Album A")).unwrap().unwrap();
        assert_eq!(art_a.source, "embedded");
        assert_eq!(art_a.mime, "image/png");
        assert!(art_a.file.starts_with(&art));
        assert_eq!(
            fs::read(&art_a.file).unwrap(),
            vec![0x89, b'P', b'N', b'G', 1, 2, 3]
        );
        let art_b = lib.album_art(find("Album B")).unwrap().unwrap();
        assert_eq!(art_b.source, "folder");
        assert_eq!(art_b.mime, "image/jpeg");
        assert!(art_b.file.ends_with("Folder.jpg"));
        // Without an art dir nothing is recorded.
        let lib2 = Library::open_in_memory().unwrap();
        Scanner::new(&lib2).scan(dir.path()).unwrap();
        assert!(lib2.album_art(1).unwrap().is_none());
    }

    #[test]
    fn title_falls_back_to_file_stem() {
        let file = AudioFile {
            path: PathBuf::from("/m/03 - Untagged Song.flac"),
            size_bytes: 1,
            mtime_unix: 0,
            container: "flac".into(),
            format: pm_model::AudioFormat::pcm(44_100, 16, 2),
            duration_ms: None,
        };
        let t = track_from_tags(file, &TagSet::default());
        assert_eq!(t.title, "Untagged Song");
        assert_eq!(title_from_file_name("14_wasted_bits"), "Wasted bits");
        assert_eq!(
            title_from_file_name("63_predictor_overflow_check_24-bit"),
            "Predictor overflow check 24-bit"
        );
        assert_eq!(title_from_file_name("1999"), "1999", "a year is a title");
        assert_eq!(title_from_file_name("01"), "01");
        assert!(t.album.is_none());
    }
}
