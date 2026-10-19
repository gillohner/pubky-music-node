//! The folder scanner: walks a root, probes new or changed files, upserts
//! them into the catalogue, and flags files that disappeared.
//!
//! Change detection is by `(size_bytes, mtime_unix)`; an unchanged file is
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
}

impl<'a> Scanner<'a> {
    pub fn new(lib: &'a Library) -> Self {
        Self { lib }
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
        self.lib.upsert_track(&track)
    }
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
                .map(|s| s.to_string_lossy().to_string())
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
        assert_eq!(t.title, "03 - Untagged Song");
        assert!(t.album.is_none());
    }
}
