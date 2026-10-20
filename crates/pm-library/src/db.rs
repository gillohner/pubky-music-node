//! The SQLite catalogue.
//!
//! One `Library` wraps one connection behind a mutex, so it is `Send + Sync`
//! and every call is synchronous. The node runs calls on a blocking thread.
//! Full-text search uses FTS5 (compiled into rusqlite's bundled SQLite).

use crate::error::{LibraryError, Result};
use pm_model::AudioFormat;
use pm_model::{Album, Artist, AudioFile, Confidence, Isrc, Mbid, MusicUri, SampleKind, Track, TrackId, Upc};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("migrations/0001_init.sql")),
    (2, include_str!("migrations/0002_music_posts.sql")),
    (3, include_str!("migrations/0003_album_art.sql")),
];

pub struct Library {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LibraryStats {
    pub tracks: u64,
    pub albums: u64,
    pub artists: u64,
    pub files: u64,
    pub missing_files: u64,
    pub total_duration_ms: u64,
}

/// The scanner's view of a stored file.
/// Where an album's cover lives on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumArt {
    pub album_id: i64,
    pub file: PathBuf,
    pub mime: String,
    /// `embedded` (extracted from a tag) or `folder` (an image next to the files).
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub id: i64,
    pub track_id: TrackId,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub mtime_unix: i64,
    pub missing: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanCounts {
    pub files_seen: u64,
    pub files_added: u64,
    pub files_updated: u64,
    pub files_removed: u64,
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;",
        )?;
        let lib = Library {
            conn: Mutex::new(conn),
        };
        lib.migrate()?;
        Ok(lib)
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.lock();
        let has_table: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)?;
        let current: i64 = if has_table {
            conn.query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| {
                r.get(0)
            })?
        } else {
            0
        };
        for (version, sql) in MIGRATIONS {
            if *version > current {
                let tx = conn.transaction()?;
                tx.execute_batch(sql)?;
                tx.execute(
                    "INSERT INTO schema_version(version, applied_at) VALUES (?1, ?2)",
                    params![version, now_unix()],
                )?;
                tx.commit()?;
                tracing::info!(version, "applied library schema migration");
            }
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .lock()
            .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| {
                r.get(0)
            })?)
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ----- tracks -------------------------------------------------------

    /// Insert or update a track, keyed by its file path. Returns the id.
    pub fn upsert_track(&self, track: &Track) -> Result<TrackId> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let album_id = match &track.album {
            Some(a) => Some(upsert_album(&tx, a)?),
            None => None,
        };
        let path = track.file.path.to_string_lossy().to_string();
        let existing: Option<i64> = tx
            .query_row("SELECT track_id FROM files WHERE path = ?1", params![path], |r| {
                r.get(0)
            })
            .optional()?;
        let (uri, src, score) = match &track.canonical {
            Some((u, c)) => (
                Some(u.to_url()),
                Some(confidence_source(c)),
                Some(c.score() as f64),
            ),
            None => (None, None, None),
        };
        let track_id = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE tracks SET title=?2, album_id=?3, track_no=?4, disc_no=?5, duration_ms=?6, recording_mbid=?7, isrc=?8, acoustid=?9,
                     canonical_uri=?10, canonical_confidence_source=?11, canonical_confidence_score=?12, replaygain_track_gain_db=?13, replaygain_album_gain_db=?14
                     WHERE id=?1",
                    params![
                        id, track.title, album_id, track.track_no, track.disc_no, track.duration_ms.map(|d| d as i64),
                        track.recording_mbid.map(|m| m.to_string()), track.isrc.as_ref().map(|i| i.to_string()), track.acoustid,
                        uri, src, score, track.replaygain_track_gain_db, track.replaygain_album_gain_db
                    ],
                )?;
                tx.execute("DELETE FROM track_artists WHERE track_id = ?1", params![id])?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO tracks(title, album_id, track_no, disc_no, duration_ms, recording_mbid, isrc, acoustid, canonical_uri,
                     canonical_confidence_source, canonical_confidence_score, replaygain_track_gain_db, replaygain_album_gain_db)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![
                        track.title, album_id, track.track_no, track.disc_no, track.duration_ms.map(|d| d as i64),
                        track.recording_mbid.map(|m| m.to_string()), track.isrc.as_ref().map(|i| i.to_string()), track.acoustid,
                        uri, src, score, track.replaygain_track_gain_db, track.replaygain_album_gain_db
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };
        for (pos, artist) in track.artists.iter().enumerate() {
            let artist_id = upsert_artist(&tx, artist)?;
            tx.execute(
                "INSERT INTO track_artists(track_id, artist_id, position) VALUES (?1, ?2, ?3)",
                params![track_id, artist_id, pos as i64],
            )?;
        }
        let f = &track.file;
        tx.execute(
            "INSERT INTO files(track_id, path, size_bytes, mtime_unix, container, sample_kind, rate_hz, bits, channels, duration_ms, missing)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0)
             ON CONFLICT(path) DO UPDATE SET track_id=excluded.track_id, size_bytes=excluded.size_bytes, mtime_unix=excluded.mtime_unix,
             container=excluded.container, sample_kind=excluded.sample_kind, rate_hz=excluded.rate_hz, bits=excluded.bits,
             channels=excluded.channels, duration_ms=excluded.duration_ms, missing=0",
            params![
                track_id, path, f.size_bytes as i64, f.mtime_unix, f.container, sample_kind_str(f.format.kind),
                f.format.rate_hz, f.format.bits, f.format.channels, f.duration_ms.map(|d| d as i64)
            ],
        )?;
        let artist_text = track
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let album_text = track.album.as_ref().map(|a| a.title.clone()).unwrap_or_default();
        tx.execute("DELETE FROM tracks_fts WHERE track_id = ?1", params![track_id])?;
        tx.execute(
            "INSERT INTO tracks_fts(track_id, title, artist, album) VALUES (?1, ?2, ?3, ?4)",
            params![track_id, track.title, artist_text, album_text],
        )?;
        tx.commit()?;
        Ok(TrackId(track_id))
    }

    pub fn get_track(&self, id: TrackId) -> Result<Option<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!("{TRACK_SELECT} WHERE t.id = ?1"))?;
        let row = stmt.query_row(params![id.0], row_to_track_raw).optional()?;
        match row {
            Some(raw) => Ok(Some(finish_track(&conn, raw)?)),
            None => Ok(None),
        }
    }

    /// Tracks ordered by id, keyset-paged: pass the last id seen.
    pub fn list_tracks(&self, after: Option<TrackId>, limit: u32) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare_cached(&format!("{TRACK_SELECT} WHERE t.id > ?1 ORDER BY t.id LIMIT ?2"))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(
                params![after.map(|a| a.0).unwrap_or(0), limit as i64],
                row_to_track_raw,
            )?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    /// Full-text search over title, artist and album.
    pub fn search_tracks(&self, query: &str, limit: u32) -> Result<Vec<Track>> {
        let q = fts_query(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "{TRACK_SELECT} JOIN tracks_fts f ON f.track_id = t.id WHERE tracks_fts MATCH ?1 ORDER BY bm25(tracks_fts) LIMIT ?2"
        ))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(params![q, limit as i64], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    pub fn tracks_by_canonical(&self, uri: &MusicUri) -> Result<Vec<Track>> {
        self.tracks_where("t.canonical_uri = ?1", uri.to_url())
    }

    pub fn tracks_by_mbid(&self, mbid: Mbid) -> Result<Vec<Track>> {
        self.tracks_where("t.recording_mbid = ?1", mbid.to_string())
    }

    fn tracks_where(&self, clause: &str, arg: String) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!("{TRACK_SELECT} WHERE {clause} ORDER BY t.id"))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(params![arg], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    pub fn remove_track(&self, id: TrackId) -> Result<bool> {
        let conn = self.lock();
        conn.execute("DELETE FROM tracks_fts WHERE track_id = ?1", params![id.0])?;
        Ok(conn.execute("DELETE FROM tracks WHERE id = ?1", params![id.0])? > 0)
    }

    // ----- albums -------------------------------------------------------

    pub fn list_albums(&self, after: Option<i64>, limit: u32) -> Result<Vec<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs FROM albums WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let v = stmt
            .query_map(params![after.unwrap_or(0), limit as i64], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().collect()
    }

    pub fn album_tracks(&self, album_id: i64) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "{TRACK_SELECT} WHERE t.album_id = ?1 ORDER BY t.disc_no, t.track_no, t.id"
        ))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(params![album_id], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    // ----- album art -----------------------------------------------------

    pub fn album_art(&self, album_id: i64) -> Result<Option<AlbumArt>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT album_id, file, mime, source FROM album_art WHERE album_id = ?1",
                [album_id],
                |r| {
                    Ok(AlbumArt {
                        album_id: r.get(0)?,
                        file: PathBuf::from(r.get::<_, String>(1)?),
                        mime: r.get(2)?,
                        source: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_album_art(&self, art: &AlbumArt) -> Result<()> {
        self.lock().execute(
            "INSERT INTO album_art(album_id, file, mime, source, set_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(album_id) DO UPDATE SET file = excluded.file, mime = excluded.mime,
             source = excluded.source, set_at = excluded.set_at",
            params![
                art.album_id,
                art.file.to_string_lossy().into_owned(),
                art.mime,
                art.source,
                now_unix()
            ],
        )?;
        Ok(())
    }

    // ----- stats and folders -------------------------------------------

    pub fn stats(&self) -> Result<LibraryStats> {
        let conn = self.lock();
        let count =
            |sql: &str| -> Result<u64> { Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64) };
        Ok(LibraryStats {
            tracks: count("SELECT COUNT(*) FROM tracks")?,
            albums: count("SELECT COUNT(*) FROM albums")?,
            artists: count("SELECT COUNT(*) FROM artists")?,
            files: count("SELECT COUNT(*) FROM files WHERE missing = 0")?,
            missing_files: count("SELECT COUNT(*) FROM files WHERE missing = 1")?,
            total_duration_ms: count("SELECT COALESCE(SUM(duration_ms), 0) FROM files WHERE missing = 0")?,
        })
    }

    pub fn add_folder(&self, path: &Path) -> Result<()> {
        self.lock().execute(
            "INSERT OR IGNORE INTO folders(path, enabled) VALUES (?1, 1)",
            params![path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn list_folders(&self) -> Result<Vec<(PathBuf, bool)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT path, enabled FROM folders ORDER BY path")?;
        let v = stmt
            .query_map([], |r| {
                Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, i64>(1)? != 0))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    // ----- scanner support ----------------------------------------------

    pub fn file_by_path(&self, path: &Path) -> Result<Option<FileRow>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, track_id, path, size_bytes, mtime_unix, missing FROM files WHERE path = ?1",
                params![path.to_string_lossy()],
                |r| {
                    Ok(FileRow {
                        id: r.get(0)?,
                        track_id: TrackId(r.get(1)?),
                        path: PathBuf::from(r.get::<_, String>(2)?),
                        size_bytes: r.get::<_, i64>(3)? as u64,
                        mtime_unix: r.get(4)?,
                        missing: r.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()?)
    }

    pub fn begin_scan(&self, root: &Path) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO scans(root, started_at) VALUES (?1, ?2)",
            params![root.to_string_lossy(), now_unix()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_scan(&self, scan_id: i64, counts: &ScanCounts) -> Result<()> {
        self.lock().execute(
            "UPDATE scans SET finished_at=?2, files_seen=?3, files_added=?4, files_updated=?5, files_removed=?6 WHERE id=?1",
            params![scan_id, now_unix(), counts.files_seen as i64, counts.files_added as i64, counts.files_updated as i64, counts.files_removed as i64],
        )?;
        Ok(())
    }

    pub fn mark_file_seen(&self, path: &Path, scan_id: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE files SET last_seen_scan_id = ?2, missing = 0 WHERE path = ?1",
            params![path.to_string_lossy(), scan_id],
        )?;
        Ok(())
    }

    /// Flag every file under `root` that this scan did not see. Returns how
    /// many changed state.
    pub fn mark_missing_under(&self, root: &Path, scan_id: i64) -> Result<u64> {
        let mut prefix = root.to_string_lossy().to_string();
        if !prefix.ends_with(std::path::MAIN_SEPARATOR) {
            prefix.push(std::path::MAIN_SEPARATOR);
        }
        let n = self.lock().execute(
            "UPDATE files SET missing = 1 WHERE missing = 0 AND path LIKE ?1 ESCAPE '\\' AND (last_seen_scan_id IS NULL OR last_seen_scan_id <> ?2)",
            params![format!("{}%", like_escape(&prefix)), scan_id],
        )?;
        Ok(n as u64)
    }
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Turn free text into an FTS5 query: each word a prefix term, ANDed.
fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|w| w.replace('"', ""))
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn confidence_source(c: &Confidence) -> &'static str {
    match c {
        Confidence::TaggedMbid => "tagged_mbid",
        Confidence::TaggedIsrc => "tagged_isrc",
        Confidence::Fingerprint { .. } => "fingerprint",
        Confidence::Fuzzy { .. } => "fuzzy",
    }
}

fn confidence_from(source: &str, score: f64) -> Result<Confidence> {
    Ok(match source {
        "tagged_mbid" => Confidence::TaggedMbid,
        "tagged_isrc" => Confidence::TaggedIsrc,
        "fingerprint" => Confidence::Fingerprint { score: score as f32 },
        "fuzzy" => Confidence::Fuzzy { score: score as f32 },
        other => {
            return Err(LibraryError::Corrupt {
                column: "canonical_confidence_source",
                value: other.to_string(),
            })
        }
    })
}

fn sample_kind_str(k: SampleKind) -> &'static str {
    match k {
        SampleKind::Pcm => "pcm",
        SampleKind::Dsd => "dsd",
    }
}

fn upsert_artist(tx: &Transaction<'_>, a: &Artist) -> Result<i64> {
    let mbid = a.mbid.map(|m| m.to_string());
    tx.execute(
        "INSERT INTO artists(name, mbid, sort_name) VALUES (?1, ?2, ?3) ON CONFLICT(name, COALESCE(mbid, '')) DO UPDATE SET sort_name = COALESCE(excluded.sort_name, artists.sort_name)",
        params![a.name, mbid, Option::<String>::None],
    )?;
    Ok(tx.query_row(
        "SELECT id FROM artists WHERE name = ?1 AND COALESCE(mbid, '') = COALESCE(?2, '')",
        params![a.name, mbid],
        |r| r.get(0),
    )?)
}

fn upsert_album(tx: &Transaction<'_>, a: &Album) -> Result<i64> {
    let release = a.release_mbid.map(|m| m.to_string());
    tx.execute(
        "INSERT INTO albums(title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs) VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(title, COALESCE(album_artist, ''), COALESCE(release_mbid, '')) DO UPDATE SET
         release_group_mbid = COALESCE(excluded.release_group_mbid, albums.release_group_mbid),
         year = COALESCE(excluded.year, albums.year), upc = COALESCE(excluded.upc, albums.upc),
         total_discs = COALESCE(excluded.total_discs, albums.total_discs)",
        params![a.title, a.album_artist, release, a.release_group_mbid.map(|m| m.to_string()), a.year, a.upc.as_ref().map(|u| u.to_string()), a.total_discs],
    )?;
    Ok(tx.query_row(
        "SELECT id FROM albums WHERE title = ?1 AND COALESCE(album_artist, '') = COALESCE(?2, '') AND COALESCE(release_mbid, '') = COALESCE(?3, '')",
        params![a.title, a.album_artist, release],
        |r| r.get(0),
    )?)
}

const TRACK_SELECT: &str = "SELECT t.id, t.title, t.album_id, t.track_no, t.disc_no, t.duration_ms, t.recording_mbid, t.isrc, t.acoustid,
    t.canonical_uri, t.canonical_confidence_source, t.canonical_confidence_score, t.replaygain_track_gain_db, t.replaygain_album_gain_db,
    fi.path, fi.size_bytes, fi.mtime_unix, fi.container, fi.sample_kind, fi.rate_hz, fi.bits, fi.channels, fi.duration_ms,
    al.id, al.title, al.album_artist, al.release_mbid, al.release_group_mbid, al.year, al.upc, al.total_discs
    FROM tracks t
    JOIN files fi ON fi.track_id = t.id
    LEFT JOIN albums al ON al.id = t.album_id";

struct RawTrack {
    track: Track,
    strings: RawStrings,
}

fn opt_mbid(v: Option<String>, column: &'static str) -> Result<Option<Mbid>> {
    v.map(|s| Mbid::parse(&s).map_err(|_| LibraryError::Corrupt { column, value: s }))
        .transpose()
}

fn row_to_track_raw(r: &Row<'_>) -> rusqlite::Result<RawTrack> {
    // Column access is checked; semantic parsing happens in finish_track.
    let get_str = |i: usize| r.get::<_, Option<String>>(i);
    let canonical = match (get_str(9)?, get_str(10)?, r.get::<_, Option<f64>>(11)?) {
        (Some(uri), Some(src), Some(score)) => Some((uri, src, score)),
        _ => None,
    };
    let album = match r.get::<_, Option<i64>>(23)? {
        Some(id) => Some(Album {
            id: Some(id),
            title: r.get(24)?,
            album_artist: get_str(25)?,
            release_mbid: None,
            release_group_mbid: None,
            year: r.get(28)?,
            upc: None,
            total_discs: r.get(30)?,
        }),
        None => None,
    };
    let kind = match r.get::<_, String>(18)?.as_str() {
        "dsd" => SampleKind::Dsd,
        _ => SampleKind::Pcm,
    };
    let format = AudioFormat {
        kind,
        rate_hz: r.get(19)?,
        bits: r.get(20)?,
        channels: r.get(21)?,
    };
    let file = AudioFile {
        path: PathBuf::from(r.get::<_, String>(14)?),
        size_bytes: r.get::<_, i64>(15)? as u64,
        mtime_unix: r.get(16)?,
        container: r.get(17)?,
        format,
        duration_ms: r.get::<_, Option<i64>>(22)?.map(|d| d as u64),
    };
    let mut track = Track {
        id: Some(TrackId(r.get(0)?)),
        title: r.get(1)?,
        artists: Vec::new(),
        album,
        track_no: r.get(3)?,
        disc_no: r.get(4)?,
        duration_ms: r.get::<_, Option<i64>>(5)?.map(|d| d as u64),
        file,
        recording_mbid: None,
        isrc: None,
        acoustid: get_str(8)?,
        canonical: None,
        replaygain_track_gain_db: r.get(12)?,
        replaygain_album_gain_db: r.get(13)?,
    };
    let raw_strings = RawStrings {
        recording_mbid: get_str(6)?,
        isrc: get_str(7)?,
        canonical,
        release_mbid: get_str(26)?,
        release_group_mbid: get_str(27)?,
        upc: get_str(29)?,
    };
    track.id = Some(TrackId(r.get(0)?));
    Ok(RawTrack {
        track,
        strings: raw_strings,
    })
}

struct RawStrings {
    recording_mbid: Option<String>,
    isrc: Option<String>,
    canonical: Option<(String, String, f64)>,
    release_mbid: Option<String>,
    release_group_mbid: Option<String>,
    upc: Option<String>,
}

fn finish_track(conn: &Connection, raw: RawTrack) -> Result<Track> {
    let RawTrack { mut track, strings } = raw;
    track.recording_mbid = opt_mbid(strings.recording_mbid, "recording_mbid")?;
    track.isrc = strings
        .isrc
        .map(|s| {
            Isrc::parse(&s).map_err(|_| LibraryError::Corrupt {
                column: "isrc",
                value: s,
            })
        })
        .transpose()?;
    track.canonical = match strings.canonical {
        Some((uri, src, score)) => {
            let uri = MusicUri::parse(&uri).map_err(|_| LibraryError::Corrupt {
                column: "canonical_uri",
                value: uri,
            })?;
            Some((uri, confidence_from(&src, score)?))
        }
        None => None,
    };
    if let Some(album) = track.album.as_mut() {
        album.release_mbid = opt_mbid(strings.release_mbid, "release_mbid")?;
        album.release_group_mbid = opt_mbid(strings.release_group_mbid, "release_group_mbid")?;
        album.upc = strings
            .upc
            .map(|s| {
                Upc::parse(&s).map_err(|_| LibraryError::Corrupt {
                    column: "upc",
                    value: s,
                })
            })
            .transpose()?;
    }
    let id = track.id.expect("loaded track has id").0;
    let mut stmt = conn.prepare_cached(
        "SELECT a.id, a.name, a.mbid FROM track_artists ta JOIN artists a ON a.id = ta.artist_id WHERE ta.track_id = ?1 ORDER BY ta.position",
    )?;
    let artists = stmt
        .query_map(params![id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (aid, name, mbid) in artists {
        track.artists.push(Artist {
            id: Some(aid),
            name,
            mbid: opt_mbid(mbid, "artists.mbid")?,
        });
    }
    Ok(track)
}

fn row_to_album(r: &Row<'_>) -> rusqlite::Result<Result<Album>> {
    let release: Option<String> = r.get(3)?;
    let rg: Option<String> = r.get(4)?;
    let upc: Option<String> = r.get(6)?;
    let base = Album {
        id: Some(r.get(0)?),
        title: r.get(1)?,
        album_artist: r.get(2)?,
        release_mbid: None,
        release_group_mbid: None,
        year: r.get(5)?,
        upc: None,
        total_discs: r.get(7)?,
    };
    Ok((|| {
        let mut a = base;
        a.release_mbid = opt_mbid(release, "release_mbid")?;
        a.release_group_mbid = opt_mbid(rg, "release_group_mbid")?;
        a.upc = upc
            .map(|s| {
                Upc::parse(&s).map_err(|_| LibraryError::Corrupt {
                    column: "upc",
                    value: s,
                })
            })
            .transpose()?;
        Ok(a)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";

    fn sample_track(path: &str, title: &str) -> Track {
        Track {
            id: None,
            title: title.into(),
            artists: vec![Artist {
                id: None,
                name: "Daft Punk".into(),
                mbid: Some(Mbid::parse(MB).unwrap()),
            }],
            album: Some(Album {
                id: None,
                title: "Random Access Memories".into(),
                album_artist: Some("Daft Punk".into()),
                release_mbid: Some(Mbid::parse(MB).unwrap()),
                release_group_mbid: None,
                year: Some(2013),
                upc: Some(Upc::parse("888837168618").unwrap()),
                total_discs: Some(1),
            }),
            track_no: Some(8),
            disc_no: Some(1),
            duration_ms: Some(369_000),
            file: AudioFile {
                path: PathBuf::from(path),
                size_bytes: 123,
                mtime_unix: 1_700_000_000,
                container: "flac".into(),
                format: AudioFormat::pcm(44_100, 16, 2),
                duration_ms: Some(369_000),
            },
            recording_mbid: Some(Mbid::parse(MB).unwrap()),
            isrc: Some(Isrc::parse("USQX91300108").unwrap()),
            acoustid: None,
            canonical: Some((
                MusicUri::Recording {
                    mbid: Mbid::parse(MB).unwrap(),
                },
                Confidence::TaggedMbid,
            )),
            replaygain_track_gain_db: Some(-7.35),
            replaygain_album_gain_db: None,
        }
    }

    #[test]
    fn migrate_and_roundtrip() {
        let lib = Library::open_in_memory().unwrap();
        assert_eq!(lib.schema_version().unwrap(), 3);
        let t = sample_track("/music/a.flac", "Get Lucky");
        let id = lib.upsert_track(&t).unwrap();
        let back = lib.get_track(id).unwrap().unwrap();
        assert_eq!(back.title, "Get Lucky");
        assert_eq!(back.artists.len(), 1);
        assert_eq!(back.artists[0].mbid, Some(Mbid::parse(MB).unwrap()));
        assert_eq!(back.album.as_ref().unwrap().title, "Random Access Memories");
        assert_eq!(back.album.as_ref().unwrap().upc, t.album.as_ref().unwrap().upc);
        assert_eq!(back.recording_mbid, t.recording_mbid);
        assert_eq!(back.isrc, t.isrc);
        assert_eq!(back.canonical, t.canonical);
        assert_eq!(back.file.format, AudioFormat::pcm(44_100, 16, 2));
        assert_eq!(back.replaygain_track_gain_db, Some(-7.35));

        // Upsert by path keeps the id and updates fields.
        let mut t2 = t.clone();
        t2.title = "Get Lucky (Radio Edit)".into();
        let id2 = lib.upsert_track(&t2).unwrap();
        assert_eq!(id, id2);
        assert_eq!(
            lib.get_track(id).unwrap().unwrap().title,
            "Get Lucky (Radio Edit)"
        );
        let stats = lib.stats().unwrap();
        assert_eq!(stats.tracks, 1);
        assert_eq!(stats.albums, 1);
        assert_eq!(stats.artists, 1);
        assert_eq!(stats.total_duration_ms, 369_000);
    }

    #[test]
    fn search_list_and_lookup() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&sample_track("/m/1.flac", "Get Lucky")).unwrap();
        lib.upsert_track(&sample_track("/m/2.flac", "Instant Crush"))
            .unwrap();
        let mut plain = sample_track("/m/3.wav", "Untitled");
        plain.artists.clear();
        plain.album = None;
        plain.canonical = None;
        plain.recording_mbid = None;
        plain.isrc = None;
        lib.upsert_track(&plain).unwrap();

        let hits = lib.search_tracks("lucky", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Get Lucky");
        let hits = lib.search_tracks("daft", 10).unwrap();
        assert_eq!(hits.len(), 2);
        let hits = lib.search_tracks("random inst", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(lib.search_tracks("", 10).unwrap().is_empty());

        let page1 = lib.list_tracks(None, 2).unwrap();
        assert_eq!(page1.len(), 2);
        let page2 = lib.list_tracks(page1.last().and_then(|t| t.id), 2).unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].title, "Untitled");
        assert!(page2[0].album.is_none());
        assert!(page2[0].artists.is_empty());

        let uri = MusicUri::Recording {
            mbid: Mbid::parse(MB).unwrap(),
        };
        assert_eq!(lib.tracks_by_canonical(&uri).unwrap().len(), 2);
        assert_eq!(lib.tracks_by_mbid(Mbid::parse(MB).unwrap()).unwrap().len(), 2);
        let albums = lib.list_albums(None, 10).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(lib.album_tracks(albums[0].id.unwrap()).unwrap().len(), 2);
        assert!(lib.remove_track(TrackId(1)).unwrap());
        assert_eq!(lib.stats().unwrap().tracks, 2);
        assert!(lib.search_tracks("lucky", 10).unwrap().is_empty());
    }

    #[test]
    fn folders_and_missing() {
        let lib = Library::open_in_memory().unwrap();
        lib.add_folder(Path::new("/music")).unwrap();
        lib.add_folder(Path::new("/music")).unwrap();
        assert_eq!(lib.list_folders().unwrap().len(), 1);
        lib.upsert_track(&sample_track("/music/a.flac", "A")).unwrap();
        lib.upsert_track(&sample_track("/music/b.flac", "B")).unwrap();
        lib.upsert_track(&sample_track("/other/c.flac", "C")).unwrap();
        let scan = lib.begin_scan(Path::new("/music")).unwrap();
        lib.mark_file_seen(Path::new("/music/a.flac"), scan).unwrap();
        assert_eq!(lib.mark_missing_under(Path::new("/music"), scan).unwrap(), 1);
        let s = lib.stats().unwrap();
        assert_eq!(s.missing_files, 1);
        assert_eq!(s.files, 2);
        let f = lib.file_by_path(Path::new("/music/b.flac")).unwrap().unwrap();
        assert!(f.missing);
        lib.finish_scan(
            scan,
            &ScanCounts {
                files_seen: 1,
                files_removed: 1,
                ..Default::default()
            },
        )
        .unwrap();
        // A re-upsert clears the flag.
        lib.upsert_track(&sample_track("/music/b.flac", "B")).unwrap();
        assert!(
            !lib.file_by_path(Path::new("/music/b.flac"))
                .unwrap()
                .unwrap()
                .missing
        );
    }

    #[test]
    fn fts_query_building() {
        assert_eq!(fts_query("daft punk"), "\"daft\"* \"punk\"*");
        assert_eq!(fts_query("  \"quoted\" "), "\"quoted\"*");
        assert_eq!(fts_query(""), "");
    }
}
