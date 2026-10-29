//! The SQLite catalogue.
//!
//! One `Library` wraps one connection behind a mutex, so it is `Send + Sync`
//! and every call is synchronous. The node runs calls on a blocking thread.
//! Full-text search uses FTS5 (compiled into rusqlite's bundled SQLite).

use crate::error::{LibraryError, Result};
use crate::import::ImportedTrack;
use pm_model::AudioFormat;
use pm_model::{Album, Artist, AudioFile, Confidence, Isrc, Mbid, MusicUri, SampleKind, Track, TrackId, Upc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("migrations/0001_init.sql")),
    (2, include_str!("migrations/0002_music_posts.sql")),
    (3, include_str!("migrations/0003_album_art.sql")),
    (4, include_str!("migrations/0004_settings.sql")),
    (5, include_str!("migrations/0005_plays.sql")),
    (6, include_str!("migrations/0006_favourites.sql")),
    (7, include_str!("migrations/0007_tags.sql")),
    (8, include_str!("migrations/0008_genres.sql")),
    (9, include_str!("migrations/0009_lyrics.sql")),
    (10, include_str!("migrations/0010_identify.sql")),
    (11, include_str!("migrations/0011_identify_release.sql")),
    (12, include_str!("migrations/0012_fts_rowid.sql")),
    (13, include_str!("migrations/0013_file_hashes.sql")),
    (14, include_str!("migrations/0014_loudness.sql")),
];

pub struct Library {
    conn: Mutex<Connection>,
}

/// How far ahead of the node a device's clock may run for [`Library::record_play_at`] to keep a
/// play at the device's time (seconds).
pub const PLAY_CLOCK_SLACK: i64 = 3600;

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
/// Counts of what a backup holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackupCounts {
    pub tracks: u64,
    /// Tracks with a canonical identity (tagged, fingerprinted or confirmed).
    pub identified: u64,
    pub albums: u64,
    pub playlists: u64,
    pub favourites: u64,
    pub tags: u64,
    pub plays: u64,
    pub folders: u64,
}

/// The id of "Unknown artist", the songs without any artist ([`Library::unknown_artist`]).
pub const UNKNOWN_ARTIST_ID: i64 = 0;

/// An artist as the browse views list them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtistSummary {
    pub id: i64,
    pub name: String,
    pub mbid: Option<Mbid>,
    pub albums: u64,
    pub tracks: u64,
}

/// An identification suggestion waiting for the owner.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SuggestionRow {
    pub id: i64,
    pub track_id: i64,
    pub recording_mbid: String,
    pub release_mbid: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub release_title: Option<String>,
    pub score: f32,
    /// `fingerprint` or `search`.
    pub source: String,
}

/// How much of the library has a canonical identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdentityCounts {
    pub total: u64,
    pub identified: u64,
    /// Tracks with suggestions to confirm.
    pub suggested: u64,
    /// Tracks identification found nothing for (or failed on).
    pub unknown: u64,
    /// Tracks identification has not looked at yet.
    pub unchecked: u64,
}

/// A favourite as stored locally (a v1 bookmark of a canonical music URL).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FavouriteRow {
    pub id: String,
    pub uri: String,
    /// Microseconds.
    pub created_at: i64,
    pub synced_at: Option<i64>,
    pub deleted: bool,
}

/// Roon-style "Focus" on albums by what the files are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Focus {
    /// PCM above 16/48 in a lossless container.
    HiRes,
    Dsd,
    /// 16-bit PCM at 44.1 or 48 kHz in a lossless container.
    Cd,
    Lossless,
    Lossy,
}

const LOSSY_CONTAINERS: &str = "('mp3', 'aac', 'vorbis', 'opus')";

impl Focus {
    /// An SQL predicate over a `files` row aliased `f`.
    fn predicate(self) -> String {
        match self {
            Focus::HiRes => format!(
                "f.sample_kind = 'pcm' AND (f.rate_hz > 48000 OR f.bits > 16) AND f.container NOT IN {LOSSY_CONTAINERS}"
            ),
            Focus::Dsd => "f.sample_kind = 'dsd'".to_string(),
            Focus::Cd => format!(
                "f.sample_kind = 'pcm' AND f.rate_hz <= 48000 AND f.bits <= 16 AND f.container NOT IN {LOSSY_CONTAINERS}"
            ),
            Focus::Lossless => format!("f.container NOT IN {LOSSY_CONTAINERS}"),
            Focus::Lossy => format!("f.container IN {LOSSY_CONTAINERS}"),
        }
    }
}

/// The best file format on an album, for a badge and for Focus.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AlbumFormat {
    pub album_id: i64,
    /// `24/96 FLAC`, `DSD64`, `16/44.1 ALAC`, `MP3`.
    pub label: String,
    pub lossless: bool,
    pub hi_res: bool,
    pub dsd: bool,
}

/// A genre as the browse views list it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GenreSummary {
    pub id: i64,
    pub name: String,
    pub tracks: u64,
    pub albums: u64,
}

/// One of the user's own tags (a v1 PubkyAppTag) as stored locally.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TagRow {
    pub id: String,
    pub uri: String,
    pub label: String,
    /// Microseconds.
    pub created_at: i64,
    pub synced_at: Option<i64>,
    pub deleted: bool,
}

/// One entry of the play history.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlayEntry {
    pub id: i64,
    pub track: Track,
    pub zone: Option<String>,
    pub played_at: i64,
}

/// A track with how often it was played.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlayedTrack {
    pub track: Track,
    pub plays: u64,
    pub last_played_at: i64,
}

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

/// How long a write waits for another connection's transaction before "database is locked".
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// SQLite's busy handler: try again every millisecond, up to [`BUSY_TIMEOUT`]. The built-in
/// timeout sleeps up to 100 ms between tries, so a writer that commits and starts again within a
/// few milliseconds (a scan's batches) kept everyone else out until they gave up.
fn wait_for_writer(tries: i32) -> bool {
    if (tries as u128) >= BUSY_TIMEOUT.as_millis() {
        return false;
    }
    std::thread::sleep(std::time::Duration::from_millis(1));
    true
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
        // A scan writes from its own connection in batches of a few hundred files (and two scans
        // of large shares may run with the API writing too); a writer waits for the others instead
        // of failing with "database is locked".
        conn.busy_handler(Some(wait_for_writer))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;",
        )?;
        let lib = Library {
            conn: Mutex::new(conn),
        };
        lib.migrate()?;
        lib.tidy_file_name_titles()?;
        lib.split_artist_credits()?;
        Ok(lib)
    }

    /// Artists named by a whole credit ("Sefa, Dr. Peacock", scanned before
    /// [`crate::credits`]) become the artists it names; their tracks keep their order of artists.
    /// Artists with a MusicBrainz id are left alone. Idempotent. Returns how many were split.
    fn split_artist_credits(&self) -> Result<usize> {
        let conn = self.lock();
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT id, name FROM artists WHERE mbid IS NULL")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut split = 0;
        for (artist_id, name) in rows {
            let parts = crate::credits::split_credit(&name);
            if parts.len() < 2 {
                continue;
            }
            split += 1;
            let tracks: Vec<i64> = conn
                .prepare("SELECT DISTINCT track_id FROM track_artists WHERE artist_id = ?1")?
                .query_map(params![artist_id], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            for track in tracks {
                // The track's artists in order, the credit replaced by its parts
                let current: Vec<i64> = conn
                    .prepare("SELECT artist_id FROM track_artists WHERE track_id = ?1 ORDER BY position")?
                    .query_map(params![track], |r| r.get(0))?
                    .collect::<std::result::Result<_, _>>()?;
                let mut next: Vec<i64> = Vec::new();
                for a in current {
                    if a == artist_id {
                        for p in &parts {
                            let id = upsert_artist(
                                &conn,
                                &Artist {
                                    id: None,
                                    name: p.clone(),
                                    mbid: None,
                                },
                            )?;
                            if !next.contains(&id) {
                                next.push(id);
                            }
                        }
                    } else if !next.contains(&a) {
                        next.push(a);
                    }
                }
                conn.execute("DELETE FROM track_artists WHERE track_id = ?1", params![track])?;
                for (pos, a) in next.iter().enumerate() {
                    conn.execute(
                        "INSERT INTO track_artists(track_id, artist_id, position) VALUES (?1, ?2, ?3)",
                        params![track, a, pos as i64],
                    )?;
                }
                let names: Vec<String> = next
                    .iter()
                    .map(|a| {
                        conn.query_row("SELECT name FROM artists WHERE id = ?1", params![a], |r| r.get(0))
                    })
                    .collect::<std::result::Result<_, _>>()?;
                conn.execute(
                    "UPDATE tracks_fts SET artist = ?2 WHERE rowid = ?1",
                    params![track, names.join(" ")],
                )?;
            }
            conn.execute(
                "DELETE FROM artists WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM track_artists WHERE artist_id = ?1)",
                params![artist_id],
            )?;
        }
        if split > 0 {
            tracing::info!(split, "artist credits split into their artists");
        }
        Ok(split)
    }

    /// Tracks still titled by their raw file name (scanned before
    /// [`crate::scan::title_from_file_name`]) get the tidy title. Idempotent.
    fn tidy_file_name_titles(&self) -> Result<usize> {
        let conn = self.lock();
        let rows: Vec<(i64, String, String)> = {
            let mut st =
                conn.prepare("SELECT t.id, t.title, f.path FROM tracks t JOIN files f ON f.track_id = t.id")?;
            let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            it.collect::<std::result::Result<_, _>>()?
        };
        let mut n = 0;
        for (id, title, path) in rows {
            let stem = Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if title != stem {
                continue;
            }
            let tidy = crate::scan::title_from_file_name(&stem);
            if tidy.is_empty() || tidy == title {
                continue;
            }
            conn.execute("UPDATE tracks SET title = ?2 WHERE id = ?1", params![id, tidy])?;
            conn.execute(
                "UPDATE tracks_fts SET title = ?2 WHERE rowid = ?1",
                params![id, tidy],
            )?;
            n += 1;
        }
        Ok(n)
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
        // An older node on a library a newer one has migrated (a rollback) would run on tables
        // it doesn't know: stop instead, and the owner updates or restores the backup from before
        let known = MIGRATIONS.last().map_or(0, |(v, _)| *v);
        if current > known {
            return Err(LibraryError::NewerSchema {
                found: current,
                known,
            });
        }
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

    /// Start grouping writes into one commit (a scan writes thousands of rows; one commit per
    /// file made a first scan of 50,000 tracks wait on the disk for most of its time). Writes
    /// made meanwhile, by the scan or anything else, land in the batch; [`Self::commit_batch`]
    /// commits them. Keep a batch short: commit every few hundred files.
    ///
    /// The batch takes the write lock when it starts (`BEGIN IMMEDIATE`), waiting for other
    /// writers. A plain `BEGIN` reads first and asks for the lock at its first write; in WAL mode
    /// that write fails at once with "database is locked" when another connection committed
    /// since the read, without waiting, and the scan skipped the file.
    pub fn begin_batch(&self) -> Result<()> {
        let conn = self.lock();
        if conn.is_autocommit() {
            conn.execute_batch("BEGIN IMMEDIATE")?;
        }
        Ok(())
    }

    /// Commit what [`Self::begin_batch`] grouped (nothing to do without a batch).
    pub fn commit_batch(&self) -> Result<()> {
        let conn = self.lock();
        if !conn.is_autocommit() {
            conn.execute_batch("COMMIT")?;
        }
        Ok(())
    }

    // ----- tracks -------------------------------------------------------

    /// Insert or update a track, keyed by its file path. Returns the id.
    pub fn upsert_track(&self, track: &Track) -> Result<TrackId> {
        let mut conn = self.lock();
        let tx = conn.savepoint()?;
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
                    // A rescan of a file without IDs in its tags keeps what identification
                    // found (fingerprint, release match, the owner's confirmation).
                    "UPDATE tracks SET title=?2, album_id=?3, track_no=?4, disc_no=?5, duration_ms=?6, recording_mbid=?7, isrc=?8,
                     acoustid=COALESCE(?9, acoustid),
                     canonical_uri=CASE WHEN ?10 IS NULL AND canonical_confidence_source IN ('fingerprint','fingerprint+release','confirmed_by_user') THEN canonical_uri ELSE ?10 END,
                     canonical_confidence_score=CASE WHEN ?10 IS NULL AND canonical_confidence_source IN ('fingerprint','fingerprint+release','confirmed_by_user') THEN canonical_confidence_score ELSE ?12 END,
                     canonical_confidence_source=CASE WHEN ?10 IS NULL AND canonical_confidence_source IN ('fingerprint','fingerprint+release','confirmed_by_user') THEN canonical_confidence_source ELSE ?11 END,
                     replaygain_track_gain_db=?13, replaygain_album_gain_db=?14
                     WHERE id=?1",
                    params![
                        id, track.title, album_id, track.track_no, track.disc_no, track.duration_ms.map(|d| d as i64),
                        track.recording_mbid.map(|m| m.to_string()), track.isrc.as_ref().map(|i| i.to_string()), track.acoustid,
                        uri, src, score, track.replaygain_track_gain_db, track.replaygain_album_gain_db
                    ],
                )?;
                tx.execute("DELETE FROM track_artists WHERE track_id = ?1", params![id])?;
                tx.execute("DELETE FROM track_genres WHERE track_id = ?1", params![id])?;
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
        for g in &track.genres {
            tx.execute("INSERT OR IGNORE INTO genres(name) VALUES (?1)", params![g])?;
            let gid: i64 = tx.query_row(
                "SELECT id FROM genres WHERE name = ?1 COLLATE NOCASE",
                params![g],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO track_genres(track_id, genre_id) VALUES (?1, ?2)",
                params![track_id, gid],
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
        let mut album_text = track.album.as_ref().map(|a| a.title.clone()).unwrap_or_default();
        for g in &track.genres {
            album_text.push(' ');
            album_text.push_str(g);
        }
        tx.execute("DELETE FROM tracks_fts WHERE rowid = ?1", params![track_id])?;
        tx.execute(
            "INSERT INTO tracks_fts(rowid, track_id, title, artist, album) VALUES (?1, ?1, ?2, ?3, ?4)",
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

    /// Tracks by id, `limit` from `offset` (clients that page by offset: Subsonic `search3`).
    pub fn tracks_page(&self, offset: u32, limit: u32) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!("{TRACK_SELECT} ORDER BY t.id LIMIT ?1 OFFSET ?2"))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(params![limit as i64, offset as i64], row_to_track_raw)?
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
            "{TRACK_SELECT} JOIN tracks_fts f ON f.rowid = t.id WHERE tracks_fts MATCH ?1 ORDER BY bm25(tracks_fts) LIMIT ?2"
        ))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(params![q, limit as i64], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    pub fn tracks_by_canonical(&self, uri: &MusicUri) -> Result<Vec<Track>> {
        self.tracks_where("t.canonical_uri = ?1", uri.to_url())
    }

    /// Tracks whose tags carry this ISRC (whatever their canonical identity).
    pub fn tracks_by_isrc(&self, isrc: &str) -> Result<Vec<Track>> {
        self.tracks_where("t.isrc = ?1", isrc.to_ascii_uppercase())
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
        conn.execute("DELETE FROM tracks_fts WHERE rowid = ?1", params![id.0])?;
        Ok(conn.execute("DELETE FROM tracks WHERE id = ?1", params![id.0])? > 0)
    }

    // ----- albums -------------------------------------------------------

    pub fn list_albums(&self, after: Option<i64>, limit: u32) -> Result<Vec<Album>> {
        self.list_albums_focused(None, after, limit)
    }

    /// Albums with at least one present file matching `focus` (all albums
    /// when `None`).
    pub fn list_albums_focused(
        &self,
        focus: Option<Focus>,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Album>> {
        let conn = self.lock();
        let filter = match focus {
            Some(f) => format!(
                " AND EXISTS (SELECT 1 FROM tracks t JOIN files f ON f.track_id = t.id WHERE t.album_id = albums.id AND f.missing = 0 AND {})",
                f.predicate()
            ),
            None => String::new(),
        };
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT id, title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs FROM albums WHERE id > ?1{filter} ORDER BY id LIMIT ?2"
        ))?;
        let v = stmt
            .query_map(params![after.unwrap_or(0), limit as i64], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().collect()
    }

    /// The best present file per album (highest rate, then bits), labelled.
    pub fn album_formats(&self) -> Result<Vec<AlbumFormat>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT t.album_id, f.container, f.sample_kind, f.rate_hz, f.bits,
                    MAX(f.rate_hz * 100 + f.bits)
             FROM files f JOIN tracks t ON t.id = f.track_id
             WHERE t.album_id IS NOT NULL AND f.missing = 0
             GROUP BY t.album_id ORDER BY t.album_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (album_id, container, kind, rate, bits) = row?;
            out.push(describe_format(album_id, &container, &kind, rate, bits));
        }
        Ok(out)
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

    // ----- browsing (artists, albums by artist, random) ----------------

    /// Every artist with how many albums and tracks they appear on, by name.
    pub fn list_artists(&self) -> Result<Vec<ArtistSummary>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT a.id, a.name, a.mbid,
                    COUNT(DISTINCT t.album_id) AS albums, COUNT(DISTINCT t.id) AS tracks
             FROM artists a
             JOIN track_artists ta ON ta.artist_id = a.id
             JOIN tracks t ON t.id = ta.track_id
             GROUP BY a.id ORDER BY a.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, name, mbid, albums, tracks) = r?;
            out.push(ArtistSummary {
                id,
                name,
                mbid: opt_mbid(mbid, "artists.mbid")?,
                albums: albums as u64,
                tracks: tracks as u64,
            });
        }
        Ok(out)
    }

    pub fn get_artist(&self, id: i64) -> Result<Option<ArtistSummary>> {
        Ok(self.list_artists()?.into_iter().find(|a| a.id == id))
    }

    /// Songs with no artist at all, shown as "Unknown artist" (second review: they were in no
    /// artist's list). `None` when every song has one.
    pub fn unknown_artist(&self) -> Result<Option<ArtistSummary>> {
        let conn = self.lock();
        let (tracks, albums): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT t.album_id) FROM tracks t
             WHERE NOT EXISTS (SELECT 1 FROM track_artists ta WHERE ta.track_id = t.id)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((tracks > 0).then(|| ArtistSummary {
            id: UNKNOWN_ARTIST_ID,
            name: "Unknown artist".into(),
            mbid: None,
            albums: albums as u64,
            tracks: tracks as u64,
        }))
    }

    /// The songs behind [`Library::unknown_artist`], by album then track number.
    pub fn tracks_without_artist(&self) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "{TRACK_SELECT} WHERE NOT EXISTS (SELECT 1 FROM track_artists ta WHERE ta.track_id = t.id) ORDER BY al.title, t.disc_no, t.track_no, t.title"
        ))?;
        let raws: Vec<RawTrack> = stmt
            .query_map([], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    /// Albums an artist appears on, by year then title.
    pub fn artist_albums(&self, artist_id: i64) -> Result<Vec<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT al.id, al.title, al.album_artist, al.release_mbid, al.release_group_mbid, al.year, al.upc, al.total_discs
             FROM albums al
             JOIN tracks t ON t.album_id = al.id
             JOIN track_artists ta ON ta.track_id = t.id
             WHERE ta.artist_id = ?1 ORDER BY al.year, al.title COLLATE NOCASE",
        )?;
        let v = stmt
            .query_map(params![artist_id], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().collect()
    }

    pub fn get_album(&self, id: i64) -> Result<Option<Album>> {
        let conn = self.lock();
        let a = conn
            .query_row(
                "SELECT id, title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs FROM albums WHERE id = ?1",
                params![id],
                row_to_album,
            )
            .optional()?;
        a.transpose()
    }

    pub fn artist_tracks(&self, artist_id: i64) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "{TRACK_SELECT} JOIN track_artists ta ON ta.track_id = t.id WHERE ta.artist_id = ?1 ORDER BY al.year, al.title, t.disc_no, t.track_no"
        ))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(params![artist_id], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    pub fn random_tracks(&self, limit: u32) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!("{TRACK_SELECT} ORDER BY RANDOM() LIMIT ?1"))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(params![limit as i64], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    /// Albums newest-first by id (a proxy for "recently added").
    pub fn recent_albums(&self, offset: u32, limit: u32) -> Result<Vec<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs FROM albums ORDER BY id DESC LIMIT ?1 OFFSET ?2",
        )?;
        let v = stmt
            .query_map(params![limit as i64, offset as i64], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().collect()
    }

    // ----- favourites -------------------------------------------------------

    /// Add (or revive) a favourite; returns the row. Idempotent: a live favourite keeps its time,
    /// a revived one (cleared, not yet synced) takes the new time.
    pub fn add_favourite(&self, id: &str, uri: &str, created_at: i64) -> Result<FavouriteRow> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO favourites(id, uri, created_at, synced_at, deleted) VALUES (?1, ?2, ?3, NULL, 0)
             ON CONFLICT(uri) DO UPDATE SET deleted = 0,
                 created_at = CASE WHEN favourites.deleted = 1 THEN excluded.created_at ELSE favourites.created_at END,
                 synced_at = CASE WHEN favourites.deleted = 1 THEN NULL ELSE favourites.synced_at END",
            params![id, uri, created_at],
        )?;
        drop(conn);
        Ok(self.favourite_by_uri(uri)?.expect("just inserted"))
    }

    /// Tombstone a favourite until the remote delete succeeds. `false` when unknown.
    pub fn remove_favourite(&self, uri: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE favourites SET deleted = 1, synced_at = NULL WHERE uri = ?1 AND deleted = 0",
            params![uri],
        )?;
        Ok(n > 0)
    }

    pub fn favourite_by_uri(&self, uri: &str) -> Result<Option<FavouriteRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, uri, created_at, synced_at, deleted FROM favourites WHERE uri = ?1",
            params![uri],
            row_to_favourite,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Live favourites, newest first.
    pub fn list_favourites(&self) -> Result<Vec<FavouriteRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, uri, created_at, synced_at, deleted FROM favourites WHERE deleted = 0 ORDER BY created_at DESC",
        )?;
        let v = stmt
            .query_map([], row_to_favourite)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Rows to push: never synced, changed since, or tombstoned.
    pub fn unsynced_favourites(&self) -> Result<Vec<FavouriteRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, uri, created_at, synced_at, deleted FROM favourites WHERE synced_at IS NULL ORDER BY created_at",
        )?;
        let v = stmt
            .query_map([], row_to_favourite)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Queue a live favourite for another push (the v1 migration).
    pub fn mark_favourite_unsynced(&self, id: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE favourites SET synced_at = NULL WHERE id = ?1 AND deleted = 0",
            params![id],
        )?;
        Ok(())
    }

    /// Mark pushed; a pushed tombstone is purged.
    pub fn mark_favourite_synced(&self, id: &str, at_micros: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM favourites WHERE id = ?1 AND deleted = 1",
            params![id],
        )?;
        conn.execute(
            "UPDATE favourites SET synced_at = ?2 WHERE id = ?1",
            params![id, at_micros],
        )?;
        Ok(())
    }

    /// Tracks that go with `seed` for a library radio: same genres first, then
    /// the same artists, then anything; random within each tier, never the
    /// seed itself or anything in `exclude`. Up to `limit`.
    pub fn similar_tracks(&self, seed: &Track, exclude: &[TrackId], limit: usize) -> Result<Vec<Track>> {
        let mut out: Vec<Track> = Vec::new();
        let mut seen: std::collections::HashSet<i64> = exclude.iter().map(|t| t.0).collect();
        if let Some(id) = seed.id {
            seen.insert(id.0);
        }
        let mut push = |tracks: Vec<Track>, out: &mut Vec<Track>| {
            for t in tracks {
                if out.len() >= limit {
                    break;
                }
                if let Some(id) = t.id {
                    if seen.insert(id.0) {
                        out.push(t);
                    }
                }
            }
        };
        for g in &seed.genres {
            if let Some(genre) = self.genre_by_name(g)? {
                let mut tracks = self.genre_tracks(genre.id, 200)?;
                shuffle(&mut tracks);
                push(tracks, &mut out);
            }
        }
        for a in &seed.artists {
            if let Some(aid) = a.id {
                let mut tracks = self.artist_tracks(aid)?;
                shuffle(&mut tracks);
                push(tracks, &mut out);
            }
        }
        if out.len() < limit {
            push(self.random_tracks((limit * 2) as u32)?, &mut out);
        }
        Ok(out)
    }

    // ----- backup -----------------------------------------------------------

    /// Write a consistent snapshot of the whole database to `path` (SQLite
    /// `VACUUM INTO`): safe while the node runs, and the copy is a plain
    /// database file that `restore` or `Library::open` can use. The target
    /// must not exist.
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        if path.exists() {
            return Err(LibraryError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::AlreadyExists, "exists"),
            });
        }
        let conn = self.lock();
        conn.execute("VACUUM INTO ?1", params![path.to_string_lossy()])?;
        Ok(())
    }

    /// Rewrite the database file in place and fold the write-ahead log into it: afterwards the
    /// file alone holds everything, and rows deleted before (sign-ins taken out of a backup copy)
    /// are gone from it, not just marked free.
    pub fn vacuum_into_place(&self) -> Result<()> {
        self.lock()
            .execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// What a backup carries, counted (shown before a restore and compared after it).
    pub fn backup_counts(&self) -> Result<BackupCounts> {
        let conn = self.lock();
        let n = |sql: &str| -> Result<u64> { Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64) };
        Ok(BackupCounts {
            tracks: n("SELECT COUNT(*) FROM tracks")?,
            identified: n("SELECT COUNT(*) FROM tracks WHERE canonical_uri IS NOT NULL")?,
            albums: n("SELECT COUNT(*) FROM albums")?,
            playlists: n(
                "SELECT COUNT(*) FROM music_posts WHERE kind = 'pubky.music/playlist' AND deleted = 0",
            )?,
            favourites: n("SELECT COUNT(*) FROM favourites WHERE deleted = 0")?,
            tags: n("SELECT COUNT(*) FROM tags WHERE deleted = 0")?,
            plays: n("SELECT COUNT(*) FROM plays")?,
            folders: n("SELECT COUNT(*) FROM folders")?,
        })
    }

    /// Replace the whole database with the one at `path` while the node runs (SQLite's backup
    /// API copies it into this connection), then bring it up to this version's schema. Check the
    /// file first by opening it with [`Library::open`]: a broken one must never get this far.
    pub fn restore_from(&self, path: &Path) -> Result<()> {
        {
            let mut conn = self.lock();
            conn.restore(rusqlite::MAIN_DB, path, None::<fn(rusqlite::backup::Progress)>)?;
        }
        self.migrate()?;
        self.tidy_file_name_titles()?;
        Ok(())
    }

    /// Music that moved (a new disk, another machine): every file under `from` now lives under
    /// `to`. Tracks keep their ids, so matches, playlists, history and tags stay with them.
    /// Returns the number of files moved.
    pub fn move_folder(&self, from: &Path, to: &Path) -> Result<u64> {
        let from = from.to_string_lossy().trim_end_matches('/').to_string();
        let to = to.to_string_lossy().trim_end_matches('/').to_string();
        let mut conn = self.lock();
        let tx = conn.savepoint()?;
        let n = tx.execute(
            "UPDATE files SET path = ?2 || substr(path, length(?1) + 1)
             WHERE path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/'",
            params![from, to],
        )?;
        tx.execute(
            "UPDATE album_art SET file = ?2 || substr(file, length(?1) + 1)
             WHERE source = 'folder' AND substr(file, 1, length(?1) + 1) = ?1 || '/'",
            params![from, to],
        )?;
        tx.execute("DELETE FROM folders WHERE path = ?1", params![from])?;
        tx.execute(
            "INSERT OR IGNORE INTO folders(path, enabled) VALUES (?1, 1)",
            params![to],
        )?;
        tx.commit()?;
        Ok(n as u64)
    }

    /// Forget covers whose image file is gone (a restore on another machine: the extracted
    /// covers live in the node's data folder, which a backup leaves out). Returns how many.
    pub fn drop_missing_art(&self) -> Result<u64> {
        let conn = self.lock();
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT album_id, file FROM album_art")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut n = 0;
        for (id, file) in rows {
            if !Path::new(&file).exists() {
                conn.execute("DELETE FROM album_art WHERE album_id = ?1", params![id])?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Albums without a cover, each with one of its files that is present.
    pub fn albums_without_art(&self) -> Result<Vec<(i64, PathBuf)>> {
        let conn = self.lock();
        let v = conn
            .prepare(
                "SELECT al.id, MIN(f.path) FROM albums al
                 JOIN tracks t ON t.album_id = al.id
                 JOIN files f ON f.track_id = t.id AND f.missing = 0
                 LEFT JOIN album_art aa ON aa.album_id = al.id
                 WHERE aa.album_id IS NULL GROUP BY al.id",
            )?
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, PathBuf::from(r.get::<_, String>(1)?)))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(v)
    }

    /// Every present file under `root` with its album's id and title. The scanner uses it to
    /// find albums that one folder's files were split into.
    pub fn album_files_under(&self, root: &Path) -> Result<Vec<(PathBuf, i64, String)>> {
        let mut prefix = root.to_string_lossy().to_string();
        if !prefix.ends_with(std::path::MAIN_SEPARATOR) {
            prefix.push(std::path::MAIN_SEPARATOR);
        }
        let conn = self.lock();
        let v = conn
            .prepare(
                "SELECT f.path, al.id, al.title FROM files f
                 JOIN tracks t ON t.id = f.track_id
                 JOIN albums al ON al.id = t.album_id
                 WHERE f.missing = 0 AND f.path LIKE ?1 ESCAPE '\\'",
            )?
            .query_map(params![format!("{}%", like_escape(&prefix))], |r| {
                Ok((PathBuf::from(r.get::<_, String>(0)?), r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(v)
    }

    /// Drop albums no track belongs to any more (their tracks moved to another album when
    /// re-read), with their covers. Returns how many albums went.
    pub fn prune_empty_albums(&self) -> Result<u64> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM album_art WHERE album_id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)",
            [],
        )?;
        Ok(conn.execute(
            "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)",
            [],
        )? as u64)
    }

    // ----- lyrics -----------------------------------------------------------

    /// Store (or clear, with `None`) the lyrics of a track; `source` is
    /// `"lrc"` for a sidecar file or `"tag"` for embedded text.
    pub fn set_lyrics(&self, track: TrackId, lyrics: Option<(&str, &str)>) -> Result<()> {
        let conn = self.lock();
        match lyrics {
            Some((text, source)) => {
                conn.execute(
                    "INSERT INTO lyrics(track_id, text, source) VALUES (?1, ?2, ?3)
                     ON CONFLICT(track_id) DO UPDATE SET text = excluded.text, source = excluded.source",
                    params![track.0, text, source],
                )?;
            }
            None => {
                conn.execute("DELETE FROM lyrics WHERE track_id = ?1", params![track.0])?;
            }
        }
        Ok(())
    }

    /// The stored lyrics of a track as `(text, source)`.
    pub fn lyrics(&self, track: TrackId) -> Result<Option<(String, String)>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT text, source FROM lyrics WHERE track_id = ?1",
                params![track.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    // ----- measured loudness ------------------------------------------------

    /// Local files of tracks whose tags have no ReplayGain and that have no current
    /// measurement (none, or the file changed since): `(track, path, size, mtime)`.
    pub fn tracks_needing_loudness(&self, limit: u32) -> Result<Vec<(TrackId, PathBuf, i64, i64)>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT t.id, f.path, f.size_bytes, f.mtime_unix FROM tracks t
             JOIN files f ON f.track_id = t.id AND f.missing = 0 AND f.sample_kind = 'pcm'
             LEFT JOIN track_loudness l ON l.track_id = t.id
             WHERE t.replaygain_track_gain_db IS NULL
               AND f.path NOT LIKE '%://%'
               AND (l.track_id IS NULL OR l.size_bytes != f.size_bytes OR l.mtime_unix != f.mtime_unix)
             ORDER BY t.id LIMIT ?1",
        )?;
        let rows = st
            .query_map(params![limit], |r| {
                Ok((
                    TrackId(r.get(0)?),
                    PathBuf::from(r.get::<_, String>(1)?),
                    r.get(2)?,
                    r.get(3)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Record a measurement of `track`'s file as it was (`size`, `mtime`).
    pub fn set_loudness(&self, track: TrackId, lufs: f64, peak: f64, size: i64, mtime: i64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO track_loudness(track_id, lufs, peak, size_bytes, mtime_unix, measured_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(track_id) DO UPDATE SET lufs = excluded.lufs, peak = excluded.peak,
               size_bytes = excluded.size_bytes, mtime_unix = excluded.mtime_unix,
               measured_at = excluded.measured_at",
            params![track.0, lufs, peak, size, mtime, now_unix()],
        )?;
        Ok(())
    }

    /// `track`'s current measurement: `(lufs, peak)`, while its file is unchanged. (A file that
    /// could not be measured is kept with a negative peak, so it isn't tried again; it has none.)
    pub fn loudness(&self, track: TrackId) -> Result<Option<(f64, f64)>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT l.lufs, l.peak FROM track_loudness l
                 JOIN files f ON f.track_id = l.track_id
                 WHERE l.track_id = ?1 AND f.size_bytes = l.size_bytes AND f.mtime_unix = l.mtime_unix
                   AND l.peak >= 0",
                params![track.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// The album's loudness from its tracks' measurements, `(lufs, peak)`: the duration-weighted
    /// energy mean and the highest peak. `None` unless every track on it is measured (and none
    /// has ReplayGain tags, which would mix two references).
    pub fn album_loudness(&self, album_id: i64) -> Result<Option<(f64, f64)>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT t.replaygain_track_gain_db, l.lufs, l.peak, COALESCE(t.duration_ms, 1)
             FROM tracks t
             LEFT JOIN files f ON f.track_id = t.id
             LEFT JOIN track_loudness l ON l.track_id = t.id
               AND l.size_bytes = f.size_bytes AND l.mtime_unix = f.mtime_unix
             WHERE t.album_id = ?1",
        )?;
        /// A track's ReplayGain tag, measured loudness and peak, and duration.
        type Row = (Option<f64>, Option<f64>, Option<f64>, i64);
        let rows: Vec<Row> = st
            .query_map(params![album_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        if rows.is_empty() {
            return Ok(None);
        }
        let (mut energy, mut weight, mut peak) = (0f64, 0f64, 0f64);
        for (rg, lufs, p, ms) in rows {
            let (None, Some(lufs), Some(p)) = (rg, lufs, p) else {
                return Ok(None);
            };
            if p < 0.0 {
                return Ok(None);
            }
            let w = ms.max(1) as f64;
            energy += w * 10f64.powf(lufs / 10.0);
            weight += w;
            peak = peak.max(p);
        }
        Ok(Some((10.0 * (energy / weight).log10(), peak)))
    }

    /// Local tracks without ReplayGain tags: `(measured, could not be measured, still to measure)`.
    pub fn loudness_counts(&self) -> Result<(u64, u64, u64)> {
        let conn = self.lock();
        let (measured, failed): (i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(l.peak >= 0), 0), COALESCE(SUM(l.peak < 0), 0)
             FROM track_loudness l JOIN files f ON f.track_id = l.track_id
             WHERE f.size_bytes = l.size_bytes AND f.mtime_unix = l.mtime_unix",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let todo: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tracks t
             JOIN files f ON f.track_id = t.id AND f.missing = 0 AND f.sample_kind = 'pcm'
             LEFT JOIN track_loudness l ON l.track_id = t.id
             WHERE t.replaygain_track_gain_db IS NULL AND f.path NOT LIKE '%://%'
               AND (l.track_id IS NULL OR l.size_bytes != f.size_bytes OR l.mtime_unix != f.mtime_unix)",
            [],
            |r| r.get(0),
        )?;
        Ok((measured as u64, failed as u64, todo as u64))
    }

    // ----- genres -----------------------------------------------------------

    /// Every genre with track and album counts, by name.
    pub fn list_genres(&self) -> Result<Vec<GenreSummary>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT g.id, g.name, COUNT(DISTINCT t.id), COUNT(DISTINCT t.album_id)
             FROM genres g JOIN track_genres tg ON tg.genre_id = g.id JOIN tracks t ON t.id = tg.track_id
             GROUP BY g.id ORDER BY g.name COLLATE NOCASE",
        )?;
        let v = stmt
            .query_map([], |r| {
                Ok(GenreSummary {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    tracks: r.get::<_, i64>(2)?.max(0) as u64,
                    albums: r.get::<_, i64>(3)?.max(0) as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn get_genre(&self, id: i64) -> Result<Option<GenreSummary>> {
        Ok(self.list_genres()?.into_iter().find(|g| g.id == id))
    }

    pub fn genre_by_name(&self, name: &str) -> Result<Option<GenreSummary>> {
        Ok(self
            .list_genres()?
            .into_iter()
            .find(|g| g.name.eq_ignore_ascii_case(name.trim())))
    }

    /// Albums with at least one track of the genre, by artist then title.
    pub fn genre_albums(&self, genre_id: i64) -> Result<Vec<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT al.id, al.title, al.album_artist, al.release_mbid, al.release_group_mbid, al.year, al.upc, al.total_discs
             FROM albums al JOIN tracks t ON t.album_id = al.id JOIN track_genres tg ON tg.track_id = t.id
             WHERE tg.genre_id = ?1 ORDER BY al.album_artist COLLATE NOCASE, al.year, al.title COLLATE NOCASE",
        )?;
        let v = stmt
            .query_map(params![genre_id], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().collect()
    }

    pub fn genre_tracks(&self, genre_id: i64, limit: u32) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "{TRACK_SELECT} JOIN track_genres tg ON tg.track_id = t.id WHERE tg.genre_id = ?1
             ORDER BY al.album_artist COLLATE NOCASE, al.title COLLATE NOCASE, t.disc_no, t.track_no LIMIT ?2"
        ))?;
        let raws: Vec<RawTrack> = stmt
            .query_map(params![genre_id, limit as i64], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    // ----- tags -------------------------------------------------------------

    /// Add (or revive) a tag; idempotent on `(uri, label)`.
    pub fn add_tag(&self, id: &str, uri: &str, label: &str, created_at: i64) -> Result<TagRow> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO tags(id, uri, label, created_at, synced_at, deleted) VALUES (?1, ?2, ?3, ?4, NULL, 0)
             ON CONFLICT(uri, label) DO UPDATE SET deleted = 0,
                 synced_at = CASE WHEN tags.deleted = 1 THEN NULL ELSE tags.synced_at END",
            params![id, uri, label, created_at],
        )?;
        conn.query_row(
            "SELECT id, uri, label, created_at, synced_at, deleted FROM tags WHERE uri = ?1 AND label = ?2",
            params![uri, label],
            row_to_tag,
        )
        .map_err(Into::into)
    }

    /// Tombstone a tag until the remote delete succeeds. `false` when unknown.
    pub fn remove_tag(&self, uri: &str, label: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE tags SET deleted = 1, synced_at = NULL WHERE uri = ?1 AND label = ?2 AND deleted = 0",
            params![uri, label],
        )?;
        Ok(n > 0)
    }

    /// Live tags, newest first; for one URI when given.
    pub fn list_tags(&self, uri: Option<&str>) -> Result<Vec<TagRow>> {
        let conn = self.lock();
        let v = match uri {
            Some(u) => {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, uri, label, created_at, synced_at, deleted FROM tags WHERE deleted = 0 AND uri = ?1 ORDER BY created_at DESC",
                )?;
                let v = stmt
                    .query_map(params![u], row_to_tag)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                v
            }
            None => {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, uri, label, created_at, synced_at, deleted FROM tags WHERE deleted = 0 ORDER BY created_at DESC",
                )?;
                let v = stmt
                    .query_map([], row_to_tag)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                v
            }
        };
        Ok(v)
    }

    pub fn unsynced_tags(&self) -> Result<Vec<TagRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, uri, label, created_at, synced_at, deleted FROM tags WHERE synced_at IS NULL ORDER BY created_at",
        )?;
        let v = stmt
            .query_map([], row_to_tag)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Mark pushed; a pushed tombstone is purged.
    pub fn mark_tag_synced(&self, id: &str, at_micros: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM tags WHERE id = ?1 AND deleted = 1", params![id])?;
        conn.execute(
            "UPDATE tags SET synced_at = ?2 WHERE id = ?1",
            params![id, at_micros],
        )?;
        Ok(())
    }

    /// Every tag id the node holds, removed ones included (a sync skips downloading these).
    pub fn tag_ids(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT id FROM tags")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(ids)
    }

    pub fn tag_by_id(&self, id: &str) -> Result<Option<TagRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, uri, label, created_at, synced_at, deleted FROM tags WHERE id = ?1",
            params![id],
            row_to_tag,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The album with this release MBID, if any.
    pub fn album_by_release_mbid(&self, mbid: Mbid) -> Result<Option<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs FROM albums WHERE release_mbid = ?1 LIMIT 1",
        )?;
        let v = stmt
            .query_map(params![mbid.to_string()], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().next().transpose()
    }

    /// The album with this UPC / barcode, if any.
    pub fn album_by_upc(&self, upc: &Upc) -> Result<Option<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs FROM albums WHERE upc = ?1 LIMIT 1",
        )?;
        let v = stmt
            .query_map(params![upc.to_string()], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().next().transpose()
    }

    /// The album with this release-group MBID, if any.
    pub fn album_by_release_group_mbid(&self, mbid: Mbid) -> Result<Option<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs FROM albums WHERE release_group_mbid = ?1 LIMIT 1",
        )?;
        let v = stmt
            .query_map(params![mbid.to_string()], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().next().transpose()
    }

    /// The artist with this MBID, if any.
    pub fn artist_by_mbid(&self, mbid: Mbid) -> Result<Option<ArtistSummary>> {
        let id: Option<i64> = {
            let conn = self.lock();
            conn.query_row(
                "SELECT id FROM artists WHERE mbid = ?1 LIMIT 1",
                params![mbid.to_string()],
                |r| r.get(0),
            )
            .optional()?
        };
        match id {
            Some(id) => self.get_artist(id),
            None => Ok(None),
        }
    }

    /// Tracks whose file name (last path segment) is `name`.
    pub fn tracks_by_file_name(&self, name: &str) -> Result<Vec<Track>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "{TRACK_SELECT} WHERE fi.path LIKE ?1 ESCAPE '\\' AND fi.missing = 0 LIMIT 20"
        ))?;
        let escaped = name.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let raws: Vec<RawTrack> = stmt
            .query_map(params![format!("%/{escaped}")], row_to_track_raw)?
            .collect::<std::result::Result<_, _>>()?;
        raws.into_iter().map(|raw| finish_track(&conn, raw)).collect()
    }

    /// The library track for a title and (optional) artist: the first search
    /// hit whose title matches exactly, preferring one by that artist.
    pub fn find_by_title_artist(&self, title: &str, artist: Option<&str>) -> Result<Option<Track>> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(None);
        }
        let artist = artist.unwrap_or("").trim();
        let q = if artist.is_empty() {
            title.to_string()
        } else {
            format!("{title} {artist}")
        };
        let mut hits = self.search_tracks(&q, 20)?;
        if hits.is_empty() && !artist.is_empty() {
            hits = self.search_tracks(title, 20)?;
        }
        let norm = |s: &str| s.trim().to_lowercase();
        let first_artist = artist.split([',', ';', '&']).next().unwrap_or("").trim();
        Ok(hits
            .iter()
            .find(|t| {
                norm(&t.title) == norm(title)
                    && (artist.is_empty()
                        || t.artists
                            .iter()
                            .any(|a| norm(&a.name) == norm(artist) || norm(&a.name) == norm(first_artist)))
            })
            .or_else(|| hits.iter().find(|t| norm(&t.title) == norm(title)))
            .cloned())
    }

    /// Match an imported playlist entry: exact path, then file name, then
    /// title and artist.
    pub fn resolve_import(&self, t: &ImportedTrack) -> Result<Option<Track>> {
        if let Some(p) = t.path.as_deref().and_then(crate::import::local_path) {
            if let Some(row) = self.file_by_path(&p)? {
                if let Some(track) = self.get_track(row.track_id)? {
                    return Ok(Some(track));
                }
            }
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                let hits = self.tracks_by_file_name(name)?;
                if hits.len() == 1 {
                    return Ok(hits.into_iter().next());
                }
                if let Some(h) = hits.into_iter().find(|h| h.title.eq_ignore_ascii_case(&t.title)) {
                    return Ok(Some(h));
                }
            }
        }
        self.find_by_title_artist(&t.title, t.artist.as_deref())
    }

    /// Record that `track` started playing on `zone`; returns the entry id.
    /// Every played track's play count and last start (unix seconds).
    pub fn play_stats(&self) -> Result<std::collections::HashMap<i64, (u64, i64)>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT track_id, COUNT(*), MAX(played_at) FROM plays GROUP BY track_id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)?),
            ))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (id, v) = row?;
            out.insert(id, v);
        }
        Ok(out)
    }

    /// When a track last started (unix seconds), if ever.
    pub fn last_played(&self, track: TrackId) -> Result<Option<i64>> {
        Ok(self.lock().query_row(
            "SELECT MAX(played_at) FROM plays WHERE track_id = ?1",
            params![track.0],
            |r| r.get::<_, Option<i64>>(0),
        )?)
    }

    pub fn record_play(&self, track: TrackId, zone: Option<&str>) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO plays(track_id, zone, played_at) VALUES (?1, ?2, ?3)",
            params![track.0, zone, now_unix()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// A play at a given time (a device that played offline reports it later). The same track,
    /// zone and time again is not recorded twice (a resent report), checked on the device's own
    /// time before anything is changed; returns whether it was new. A device clock up to
    /// [`PLAY_CLOCK_SLACK`] ahead is kept as it is, so its resent reports still match; a time
    /// further in the future counts as now.
    pub fn record_play_at(&self, track: TrackId, zone: Option<&str>, at: i64) -> Result<bool> {
        let conn = self.lock();
        let seen: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM plays WHERE track_id = ?1 AND zone IS ?2 AND played_at = ?3)",
            params![track.0, zone, at],
            |r| r.get(0),
        )?;
        if seen {
            return Ok(false);
        }
        let now = now_unix();
        let at = if at > now + PLAY_CLOCK_SLACK { now } else { at };
        let n = conn.execute(
            "INSERT INTO plays(track_id, zone, played_at) SELECT ?1, ?2, ?3
             WHERE NOT EXISTS (SELECT 1 FROM plays WHERE track_id = ?1 AND zone IS ?2 AND played_at = ?3)",
            params![track.0, zone, at],
        )?;
        Ok(n > 0)
    }

    /// Newest plays first.
    pub fn history(&self, limit: u32) -> Result<Vec<PlayEntry>> {
        let conn = self.lock();
        let sql = format!(
            "{} JOIN plays p ON p.track_id = t.id ORDER BY p.played_at DESC, p.id DESC LIMIT ?1",
            track_select_with("p.id, p.zone, p.played_at")
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows: Vec<(RawTrack, i64, Option<String>, i64)> = stmt
            .query_map(params![limit as i64], |r| {
                Ok((
                    row_to_track_raw(r)?,
                    r.get(TRACK_COLUMNS)?,
                    r.get(TRACK_COLUMNS + 1)?,
                    r.get(TRACK_COLUMNS + 2)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        rows.into_iter()
            .map(|(raw, id, zone, played_at)| {
                Ok(PlayEntry {
                    id,
                    track: finish_track(&conn, raw)?,
                    zone,
                    played_at,
                })
            })
            .collect()
    }

    /// Tracks by play count, most played first.
    pub fn most_played_tracks(&self, limit: u32) -> Result<Vec<PlayedTrack>> {
        let conn = self.lock();
        let sql = format!(
            "{} JOIN (SELECT track_id, COUNT(*) AS n, MAX(played_at) AS last FROM plays GROUP BY track_id) p
             ON p.track_id = t.id ORDER BY p.n DESC, p.last DESC LIMIT ?1",
            track_select_with("p.n, p.last")
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows: Vec<(RawTrack, i64, i64)> = stmt
            .query_map(params![limit as i64], |r| {
                Ok((
                    row_to_track_raw(r)?,
                    r.get(TRACK_COLUMNS)?,
                    r.get(TRACK_COLUMNS + 1)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        rows.into_iter()
            .map(|(raw, n, last)| {
                Ok(PlayedTrack {
                    track: finish_track(&conn, raw)?,
                    plays: n.max(0) as u64,
                    last_played_at: last,
                })
            })
            .collect()
    }

    /// Albums whose tracks played most recently, newest first.
    pub fn recently_played_albums(&self, limit: u32) -> Result<Vec<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT al.id, al.title, al.album_artist, al.release_mbid, al.release_group_mbid, al.year, al.upc, al.total_discs
             FROM albums al JOIN (SELECT t.album_id AS album_id, MAX(p.played_at) AS last FROM plays p
                 JOIN tracks t ON t.id = p.track_id WHERE t.album_id IS NOT NULL GROUP BY t.album_id) r
             ON r.album_id = al.id ORDER BY r.last DESC LIMIT ?1",
        )?;
        let v = stmt
            .query_map(params![limit as i64], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().collect()
    }

    /// Albums by total plays of their tracks, most played first.
    pub fn most_played_albums(&self, limit: u32) -> Result<Vec<Album>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT al.id, al.title, al.album_artist, al.release_mbid, al.release_group_mbid, al.year, al.upc, al.total_discs
             FROM albums al JOIN (SELECT t.album_id AS album_id, COUNT(*) AS n, MAX(p.played_at) AS last FROM plays p
                 JOIN tracks t ON t.id = p.track_id WHERE t.album_id IS NOT NULL GROUP BY t.album_id) r
             ON r.album_id = al.id ORDER BY r.n DESC, r.last DESC LIMIT ?1",
        )?;
        let v = stmt
            .query_map(params![limit as i64], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().collect()
    }

    /// Plays of one track.
    pub fn play_count(&self, track: TrackId) -> Result<u64> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM plays WHERE track_id = ?1",
            params![track.0],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    pub fn search_albums(&self, query: &str, limit: u32) -> Result<Vec<Album>> {
        let like = format!("%{}%", query.trim());
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, title, album_artist, release_mbid, release_group_mbid, year, upc, total_discs FROM albums
             WHERE title LIKE ?1 OR COALESCE(album_artist, '') LIKE ?1 ORDER BY title COLLATE NOCASE LIMIT ?2",
        )?;
        let v = stmt
            .query_map(params![like, limit as i64], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        v.into_iter().collect()
    }

    // ----- settings (JSON key/value) ---------------------------------------

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    // ---- identification (crate::identify) ----

    /// A cheap content key for the fingerprint cache: size plus BLAKE3 of
    /// the first and last MiB. Renames and retags of the same audio keep it
    /// only when the bytes at both ends are unchanged; good enough to avoid
    /// a second AcoustID lookup for a moved file.
    pub fn file_key(path: &std::path::Path) -> std::io::Result<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        let mut h = blake3::Hasher::new();
        h.update(&len.to_le_bytes());
        let mut buf = vec![0u8; 1 << 20];
        let n = f.read(&mut buf)?;
        h.update(&buf[..n]);
        if len > (2 << 20) {
            f.seek(SeekFrom::End(-(1 << 20)))?;
            let n = f.read(&mut buf)?;
            h.update(&buf[..n]);
        }
        Ok(h.finalize().to_hex()[..32].to_string())
    }

    /// The cached fingerprint and AcoustID answer for a file key.
    pub fn cached_fingerprint(&self, key: &str) -> Result<Option<(u64, String, Option<String>)>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT duration_secs, fingerprint, lookup_json FROM fingerprints WHERE file_key = ?1",
                params![key],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    pub fn save_fingerprint(&self, key: &str, duration_secs: u64, fingerprint: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO fingerprints(file_key, duration_secs, fingerprint) VALUES (?1, ?2, ?3)
             ON CONFLICT(file_key) DO UPDATE SET duration_secs = excluded.duration_secs, fingerprint = excluded.fingerprint",
            params![key, duration_secs as i64, fingerprint],
        )?;
        Ok(())
    }

    pub fn save_lookup(&self, key: &str, json: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE fingerprints SET lookup_json = ?2, looked_up_at = ?3 WHERE file_key = ?1",
            params![key, json, now_unix()],
        )?;
        Ok(())
    }

    /// Set a track's canonical identity found after the scan (fingerprint,
    /// release match, the owner's confirmation). Tag-derived identities are
    /// set by the scanner; this never overwrites one of those.
    pub fn set_identity(
        &self,
        track: i64,
        uri: &MusicUri,
        confidence: Confidence,
        acoustid: Option<&str>,
    ) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE tracks SET canonical_uri = ?2, canonical_confidence_source = ?3, canonical_confidence_score = ?4,
             acoustid = COALESCE(?5, acoustid)
             WHERE id = ?1 AND (canonical_confidence_source IS NULL OR canonical_confidence_source NOT IN ('tagged_mbid','tagged_isrc'))",
            params![track, uri.to_url(), confidence_source(&confidence), confidence.score() as f64, acoustid],
        )?;
        Ok(n > 0)
    }

    pub fn set_identify_state(&self, track: i64, state: &str, detail: Option<&str>) -> Result<()> {
        self.lock().execute(
            "INSERT INTO identify_state(track_id, state, detail, checked_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(track_id) DO UPDATE SET state = excluded.state, detail = excluded.detail, checked_at = excluded.checked_at",
            params![track, state, detail, now_unix()],
        )?;
        Ok(())
    }

    /// Remember the release a match was disambiguated to.
    pub fn set_identify_release(&self, track: i64, release: &Mbid) -> Result<()> {
        self.lock().execute(
            "UPDATE identify_state SET release_mbid = ?2 WHERE track_id = ?1",
            params![track, release.to_string()],
        )?;
        Ok(())
    }

    /// Tracks whose identity identification found (not their tags), with the
    /// release when known: candidates for "write IDs into my files".
    /// Tracks credited to more than one artist (a file whose tags may name them as one).
    pub fn tracks_with_several_artists(&self) -> Result<Vec<Track>> {
        let ids: Vec<i64> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT ta.track_id FROM track_artists ta JOIN files f ON f.track_id = ta.track_id AND f.missing = 0
                 GROUP BY ta.track_id HAVING COUNT(*) > 1 ORDER BY ta.track_id",
            )?;
            let v = stmt
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            v
        };
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(t) = self.get_track(TrackId(id))? {
                out.push(t);
            }
        }
        Ok(out)
    }

    pub fn identified_not_tagged(&self) -> Result<Vec<(Track, Option<Mbid>)>> {
        let tracks = self.tracks_where(
            "t.canonical_confidence_source IN ('fingerprint','fingerprint+release','confirmed_by_user') AND t.canonical_uri LIKE ?1 || '%'",
            "https://musicbrainz.org/recording/".into(),
        )?;
        let conn = self.lock();
        Ok(tracks
            .into_iter()
            .map(|t| {
                let rel: Option<String> = t.id.and_then(|id| {
                    conn.query_row(
                        "SELECT release_mbid FROM identify_state WHERE track_id = ?1",
                        params![id.0],
                        |r| r.get(0),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .flatten()
                });
                let rel = rel.and_then(|r| Mbid::parse(&r).ok());
                (t, rel)
            })
            .collect())
    }

    /// Tracks without a canonical identity that identification has not
    /// looked at yet (with `retry`, also those it found nothing for or
    /// failed on; suggestions wait for the owner either way).
    pub fn tracks_to_identify(&self, retry: bool) -> Result<Vec<Track>> {
        let clause = if retry {
            "t.canonical_uri IS NULL AND ?1 = '' AND COALESCE((SELECT state FROM identify_state s WHERE s.track_id = t.id), '') <> 'suggested'"
        } else {
            "t.canonical_uri IS NULL AND ?1 = '' AND NOT EXISTS (SELECT 1 FROM identify_state s WHERE s.track_id = t.id)"
        };
        self.tracks_where(clause, String::new())
    }

    /// Audio files in the folder of `path` that the library knows.
    pub fn tracks_in_folder(&self, dir: &std::path::Path) -> Result<Vec<Track>> {
        let prefix = format!("{}/", dir.to_string_lossy().trim_end_matches('/'));
        let all = self.tracks_where("fi.path LIKE ?1 || '%'", prefix)?;
        Ok(all
            .into_iter()
            .filter(|t| t.file.path.parent() == Some(dir))
            .collect())
    }

    pub fn add_suggestions(&self, track: i64, candidates: &[crate::identify::Candidate]) -> Result<()> {
        let conn = self.lock();
        for c in candidates {
            conn.execute(
                "INSERT INTO identify_suggestions(track_id, recording_mbid, release_mbid, title, artist, release_title, score, source, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(track_id, recording_mbid) DO UPDATE SET score = MAX(score, excluded.score),
                   release_mbid = COALESCE(excluded.release_mbid, release_mbid),
                   title = COALESCE(excluded.title, title), artist = COALESCE(excluded.artist, artist),
                   release_title = COALESCE(excluded.release_title, release_title)",
                params![
                    track,
                    c.recording.to_string(),
                    c.release.map(|r| r.to_string()),
                    c.title,
                    c.artist,
                    c.release_title,
                    c.score as f64,
                    c.source,
                    now_unix()
                ],
            )?;
        }
        Ok(())
    }

    /// Open suggestions, grouped by track (best first within a track).
    /// One track's open suggestions, best first.
    pub fn suggestions_for(&self, track: i64) -> Result<Vec<SuggestionRow>> {
        Ok(self
            .suggestions()?
            .into_iter()
            .filter(|r| r.track_id == track)
            .collect())
    }

    pub fn suggestions(&self) -> Result<Vec<SuggestionRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.track_id, s.recording_mbid, s.release_mbid, s.title, s.artist, s.release_title, s.score, s.source
             FROM identify_suggestions s JOIN tracks t ON t.id = s.track_id
             WHERE t.canonical_uri IS NULL
             ORDER BY t.album_id, t.disc_no, t.track_no, s.track_id, s.score DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SuggestionRow {
                    id: r.get(0)?,
                    track_id: r.get(1)?,
                    recording_mbid: r.get(2)?,
                    release_mbid: r.get(3)?,
                    title: r.get(4)?,
                    artist: r.get(5)?,
                    release_title: r.get(6)?,
                    score: r.get::<_, f64>(7)? as f32,
                    source: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The owner confirms a suggestion: the track gets that recording as its
    /// identity (`confirmed_by_user`) and its other suggestions go.
    pub fn accept_suggestion(&self, id: i64) -> Result<Option<(i64, MusicUri)>> {
        let row: Option<(i64, String)> = self
            .lock()
            .query_row(
                "SELECT track_id, recording_mbid FROM identify_suggestions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((track, mbid)) = row else { return Ok(None) };
        let mbid = pm_model::Mbid::parse(&mbid).map_err(|_| LibraryError::Corrupt {
            column: "identify_suggestions.recording_mbid",
            value: mbid.clone(),
        })?;
        let uri = MusicUri::Recording { mbid };
        self.set_identity(track, &uri, Confidence::ConfirmedByUser, None)?;
        self.lock().execute(
            "DELETE FROM identify_suggestions WHERE track_id = ?1",
            params![track],
        )?;
        self.set_identify_state(track, "identified", Some("confirmed_by_user"))?;
        Ok(Some((track, uri)))
    }

    /// Drop one suggestion; when it was the last for its track, the track is
    /// "unknown" until the next retry.
    pub fn reject_suggestion(&self, id: i64) -> Result<bool> {
        let conn = self.lock();
        let track: Option<i64> = conn
            .query_row(
                "SELECT track_id FROM identify_suggestions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(track) = track else { return Ok(false) };
        conn.execute("DELETE FROM identify_suggestions WHERE id = ?1", params![id])?;
        let left: i64 = conn.query_row(
            "SELECT COUNT(*) FROM identify_suggestions WHERE track_id = ?1",
            params![track],
            |r| r.get(0),
        )?;
        drop(conn);
        if left == 0 {
            self.set_identify_state(track, "unknown", Some("suggestions rejected"))?;
        }
        Ok(true)
    }

    /// How identified the library is.
    pub fn identity_counts(&self) -> Result<IdentityCounts> {
        let conn = self.lock();
        let q = |sql: &str| -> rusqlite::Result<u64> {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0)).map(|n| n as u64)
        };
        Ok(IdentityCounts {
            total: q("SELECT COUNT(*) FROM tracks")?,
            identified: q("SELECT COUNT(*) FROM tracks WHERE canonical_uri IS NOT NULL")?,
            suggested: q(
                "SELECT COUNT(DISTINCT s.track_id) FROM identify_suggestions s JOIN tracks t ON t.id = s.track_id WHERE t.canonical_uri IS NULL",
            )?,
            unknown: q(
                "SELECT COUNT(*) FROM tracks t JOIN identify_state s ON s.track_id = t.id WHERE t.canonical_uri IS NULL AND s.state IN ('unknown','error')",
            )?,
            unchecked: q(
                "SELECT COUNT(*) FROM tracks t WHERE t.canonical_uri IS NULL AND NOT EXISTS (SELECT 1 FROM identify_state s WHERE s.track_id = t.id)",
            )?,
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- content hashes (files on the homeserver) ----

    /// Library files present on disk whose size is one of `sizes`: (path, size, mtime).
    pub fn files_of_sizes(&self, sizes: &std::collections::HashSet<u64>) -> Result<Vec<(PathBuf, u64, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT path, size_bytes, mtime_unix FROM files WHERE missing = 0")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            if sizes.contains(&row.1) {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// The kept hash of a file, when its size and mtime are unchanged.
    pub fn cached_file_hash(&self, path: &Path, size: u64, mtime: i64) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT blake3 FROM file_hashes WHERE path = ?1 AND size_bytes = ?2 AND mtime_unix = ?3",
                params![path.to_string_lossy(), size as i64, mtime],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn store_file_hash(&self, path: &Path, size: u64, mtime: i64, hash: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO file_hashes(path, size_bytes, mtime_unix, blake3) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET size_bytes = excluded.size_bytes,
             mtime_unix = excluded.mtime_unix, blake3 = excluded.blake3",
            params![path.to_string_lossy(), size as i64, mtime, hash],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> Result<bool> {
        Ok(self
            .lock()
            .execute("DELETE FROM settings WHERE key = ?1", [key])?
            > 0)
    }

    /// All settings whose key starts with `prefix`.
    pub fn settings_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT key, value FROM settings WHERE key LIKE ?1 || '%' ORDER BY key")?;
        let rows = st.query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
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

    /// Forget a library folder: its files, the tracks that lived only there,
    /// and the albums and artists left empty. Returns `(files, tracks)` removed.
    pub fn remove_folder(&self, root: &Path) -> Result<(u64, u64)> {
        let mut conn = self.lock();
        let tx = conn.savepoint()?;
        let root_s = root.to_string_lossy().into_owned();
        let prefix = format!("{}/", root_s.trim_end_matches('/'));
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let files = tx.execute(
            "DELETE FROM files WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            params![root_s, format!("{escaped}%")],
        )? as u64;
        tx.execute(
            "DELETE FROM plays WHERE track_id IN (SELECT id FROM tracks WHERE id NOT IN (SELECT track_id FROM files))",
            [],
        )?;
        tx.execute(
            "DELETE FROM track_artists WHERE track_id IN (SELECT id FROM tracks WHERE id NOT IN (SELECT track_id FROM files))",
            [],
        )?;
        tx.execute(
            "DELETE FROM tracks_fts WHERE rowid IN (SELECT id FROM tracks WHERE id NOT IN (SELECT track_id FROM files))",
            [],
        )?;
        let tracks = tx.execute(
            "DELETE FROM tracks WHERE id NOT IN (SELECT track_id FROM files)",
            [],
        )? as u64;
        tx.execute(
            "DELETE FROM album_art WHERE album_id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)",
            [],
        )?;
        tx.execute(
            "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)",
            [],
        )?;
        tx.execute(
            "DELETE FROM artists WHERE id NOT IN (SELECT DISTINCT artist_id FROM track_artists)",
            [],
        )?;
        tx.execute(
            "DELETE FROM genres WHERE id NOT IN (SELECT DISTINCT genre_id FROM track_genres)",
            [],
        )?;
        tx.execute("DELETE FROM folders WHERE path = ?1", params![root_s])?;
        tx.commit()?;
        Ok((files, tracks))
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

    /// Tracks and bytes of the present files under `root` (a library folder), for the storage
    /// status line.
    pub fn folder_totals(&self, root: &Path) -> Result<(u64, u64)> {
        let mut prefix = root.to_string_lossy().to_string();
        if !prefix.ends_with(std::path::MAIN_SEPARATOR) {
            prefix.push(std::path::MAIN_SEPARATOR);
        }
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT COUNT(DISTINCT track_id), COALESCE(SUM(size_bytes), 0) FROM files WHERE missing = 0 AND path LIKE ?1 ESCAPE '\\'",
            params![format!("{}%", like_escape(&prefix))],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?)
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
        Confidence::FingerprintRelease { .. } => "fingerprint+release",
        Confidence::ConfirmedByUser => "confirmed_by_user",
        Confidence::Fuzzy { .. } => "fuzzy",
    }
}

fn confidence_from(source: &str, score: f64) -> Result<Confidence> {
    Ok(match source {
        "tagged_mbid" => Confidence::TaggedMbid,
        "tagged_isrc" => Confidence::TaggedIsrc,
        "fingerprint" => Confidence::Fingerprint { score: score as f32 },
        "fingerprint+release" => Confidence::FingerprintRelease { score: score as f32 },
        "confirmed_by_user" => Confidence::ConfirmedByUser,
        "fuzzy" => Confidence::Fuzzy { score: score as f32 },
        other => {
            return Err(LibraryError::Corrupt {
                column: "canonical_confidence_source",
                value: other.to_string(),
            })
        }
    })
}

fn describe_format(album_id: i64, container: &str, kind: &str, rate: i64, bits: i64) -> AlbumFormat {
    let lossy = crate::tags::is_lossy_container(container);
    let dsd = kind == "dsd";
    let container_label = match container {
        "flac" => "FLAC",
        "alac" | "m4a" | "mp4" | "isomp4" => "ALAC",
        "wav" | "wave" | "riff" => "WAV",
        "aiff" | "aif" => "AIFF",
        "dsf" => "DSF",
        "dff" => "DFF",
        "mp3" => "MP3",
        "aac" => "AAC",
        "vorbis" | "ogg" => "Vorbis",
        "opus" => "Opus",
        other => other,
    };
    let label = if dsd {
        let mult = (rate as f64 / 44_100.0).round().max(1.0) as i64;
        format!("DSD{mult}")
    } else if lossy {
        container_label.to_string()
    } else {
        let khz = rate as f64 / 1000.0;
        let khz = if khz.fract() == 0.0 {
            format!("{khz:.0}")
        } else {
            format!("{khz:.1}")
        };
        format!("{bits}/{khz} {container_label}")
    };
    AlbumFormat {
        album_id,
        label,
        lossless: !lossy,
        hi_res: !lossy && !dsd && (rate > 48_000 || bits > 16),
        dsd,
    }
}

fn sample_kind_str(k: SampleKind) -> &'static str {
    match k {
        SampleKind::Pcm => "pcm",
        SampleKind::Dsd => "dsd",
    }
}

fn upsert_artist(tx: &Connection, a: &Artist) -> Result<i64> {
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

fn upsert_album(tx: &Connection, a: &Album) -> Result<i64> {
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

/// Number of columns `TRACK_SELECT` yields (track 14, file 9, album 8).
const TRACK_COLUMNS: usize = 31;

/// `TRACK_SELECT` with extra columns appended to the select list.
fn track_select_with(extra: &str) -> String {
    TRACK_SELECT.replacen("\n    FROM tracks t", &format!(", {extra}\n    FROM tracks t"), 1)
}

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
        genres: Vec::new(),
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
    let mut stmt = conn.prepare_cached(
        "SELECT g.name FROM track_genres tg JOIN genres g ON g.id = tg.genre_id WHERE tg.track_id = ?1 ORDER BY g.name COLLATE NOCASE",
    )?;
    track.genres = stmt
        .query_map(params![id], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(track)
}

/// Fisher–Yates with a time-seeded xorshift; good enough for a radio shuffle.
fn shuffle<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9)
        | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

fn row_to_tag(r: &Row<'_>) -> rusqlite::Result<TagRow> {
    Ok(TagRow {
        id: r.get(0)?,
        uri: r.get(1)?,
        label: r.get(2)?,
        created_at: r.get(3)?,
        synced_at: r.get(4)?,
        deleted: r.get::<_, i64>(5)? != 0,
    })
}

fn row_to_favourite(r: &Row<'_>) -> rusqlite::Result<FavouriteRow> {
    Ok(FavouriteRow {
        id: r.get(0)?,
        uri: r.get(1)?,
        created_at: r.get(2)?,
        synced_at: r.get(3)?,
        deleted: r.get::<_, i64>(4)? != 0,
    })
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
pub(crate) mod tests_support {
    use super::*;

    pub(crate) const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";

    pub(crate) fn sample_track(path: &str, title: &str) -> Track {
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
            genres: vec!["Disco".into(), "Electronic".into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{sample_track, MB};
    use super::*;

    #[test]
    fn format_labels() {
        let f = describe_format(1, "flac", "pcm", 96_000, 24);
        assert_eq!(
            (f.label.as_str(), f.hi_res, f.lossless, f.dsd),
            ("24/96 FLAC", true, true, false)
        );
        let f = describe_format(1, "flac", "pcm", 44_100, 16);
        assert_eq!((f.label.as_str(), f.hi_res), ("16/44.1 FLAC", false));
        let f = describe_format(1, "dsf", "dsd", 5_644_800, 1);
        assert_eq!((f.label.as_str(), f.dsd, f.hi_res), ("DSD128", true, false));
        let f = describe_format(1, "mp3", "pcm", 44_100, 16);
        assert_eq!((f.label.as_str(), f.lossless), ("MP3", false));
    }

    #[test]
    fn songs_without_an_artist_are_unknown_artist() {
        let lib = Library::open_in_memory().unwrap();
        assert!(lib.unknown_artist().unwrap().is_none());
        lib.upsert_track(&sample_track("/music/a.flac", "With")).unwrap();
        let mut bare = sample_track("/music/b.flac", "Without");
        bare.artists.clear();
        lib.upsert_track(&bare).unwrap();
        let u = lib.unknown_artist().unwrap().expect("one song without an artist");
        assert_eq!((u.id, u.tracks, u.albums), (UNKNOWN_ARTIST_ID, 1, 1));
        let t = lib.tracks_without_artist().unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].title, "Without");
        // The shared list stays the real artists only
        assert!(lib
            .list_artists()
            .unwrap()
            .iter()
            .all(|a| a.id != UNKNOWN_ARTIST_ID));
    }

    #[test]
    fn focus_filters_albums_by_their_files() {
        let lib = Library::open_in_memory().unwrap();
        let mut cd = sample_track("/music/cd.flac", "CD");
        cd.album.as_mut().unwrap().title = "CD album".into();
        let mut hires = sample_track("/music/hires.flac", "Hi");
        hires.album.as_mut().unwrap().title = "Hi-res album".into();
        hires.file.format.rate_hz = 96_000;
        hires.file.format.bits = 24;
        let mut lossy = sample_track("/music/l.mp3", "Lo");
        lossy.album.as_mut().unwrap().title = "MP3 album".into();
        lossy.file.container = "mp3".into();
        for t in [&cd, &hires, &lossy] {
            lib.upsert_track(t).unwrap();
        }
        let titles = |focus| -> Vec<String> {
            lib.list_albums_focused(focus, None, 100)
                .unwrap()
                .into_iter()
                .map(|a| a.title)
                .collect()
        };
        assert_eq!(titles(None).len(), 3);
        assert_eq!(titles(Some(Focus::HiRes)), ["Hi-res album"]);
        assert_eq!(titles(Some(Focus::Cd)), ["CD album"]);
        assert_eq!(titles(Some(Focus::Lossy)), ["MP3 album"]);
        assert_eq!(titles(Some(Focus::Lossless)).len(), 2);
        assert!(titles(Some(Focus::Dsd)).is_empty());
        let formats = lib.album_formats().unwrap();
        assert_eq!(formats.len(), 3);
        assert!(formats.iter().any(|f| f.label == "24/96 FLAC" && f.hi_res));
    }

    #[test]
    fn backup_is_a_usable_copy() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("library.db")).unwrap();
        let id = lib
            .upsert_track(&sample_track("/music/a.flac", "Get Lucky"))
            .unwrap();
        lib.set_lyrics(id, Some(("la la", "tag"))).unwrap();
        let copy = dir.path().join("backup.db");
        lib.backup_to(&copy).unwrap();
        assert!(lib.backup_to(&copy).is_err(), "never overwrites");
        let restored = Library::open(&copy).unwrap();
        assert_eq!(restored.stats().unwrap().tracks, 1);
        assert_eq!(restored.get_track(id).unwrap().unwrap().title, "Get Lucky");
        assert_eq!(restored.lyrics(id).unwrap().unwrap().0, "la la");
    }

    /// The search index is keyed by track id: a rescan replaces the entry (one hit, new title),
    /// and a batch commits everything written inside it.
    #[test]
    fn search_index_follows_rescans_inside_a_batch() {
        let lib = Library::open_in_memory().unwrap();
        lib.begin_batch().unwrap();
        let id = lib
            .upsert_track(&sample_track("/music/a.flac", "Get Lucky"))
            .unwrap();
        let again = lib
            .upsert_track(&sample_track("/music/a.flac", "Get Luckier"))
            .unwrap();
        lib.commit_batch().unwrap();
        assert_eq!(id, again);
        assert!(lib.search_tracks("lucky", 10).unwrap().is_empty());
        let hits = lib.search_tracks("luckier", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, Some(id));
        lib.commit_batch().unwrap(); // nothing open: nothing to do
    }

    /// A library scanned before credits were split: "Sefa, Dr. Peacock" becomes two artists,
    /// "Dr. Peacock" is shared with the track that named him alone, and nothing is left behind.
    #[test]
    fn old_credits_split_into_artists() {
        let lib = Library::open_in_memory().unwrap();
        let mut a = sample_track("/m/a.flac", "Flowing River");
        a.artists = vec![Artist {
            id: None,
            name: "Sefa, Dr. Peacock".into(),
            mbid: None,
        }];
        let mut b = sample_track("/m/b.flac", "Solo");
        b.artists = vec![Artist {
            id: None,
            name: "Dr. Peacock".into(),
            mbid: None,
        }];
        let ta = lib.upsert_track(&a).unwrap();
        lib.upsert_track(&b).unwrap();
        assert_eq!(lib.split_artist_credits().unwrap(), 1);
        let names: Vec<String> = lib.list_artists().unwrap().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["Dr. Peacock", "Sefa"]);
        let t = lib.get_track(ta).unwrap().unwrap();
        assert_eq!(
            t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["Sefa", "Dr. Peacock"]
        );
        assert_eq!(lib.split_artist_credits().unwrap(), 0, "once is enough");
        assert_eq!(lib.search_tracks("peacock", 10).unwrap().len(), 2);
    }

    #[test]
    fn a_library_from_a_newer_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let known = {
            let lib = Library::open(&path).unwrap();
            let v = lib.schema_version().unwrap();
            lib.lock()
                .execute(
                    "INSERT INTO schema_version(version, applied_at) VALUES (?1, 0)",
                    params![v + 1],
                )
                .unwrap();
            v
        };
        match Library::open(&path) {
            Err(LibraryError::NewerSchema { found, known: k }) => {
                assert_eq!((found, k), (known + 1, known));
            }
            other => panic!("expected NewerSchema, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn migrate_and_roundtrip() {
        let lib = Library::open_in_memory().unwrap();
        assert_eq!(lib.schema_version().unwrap(), 14);
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

#[cfg(test)]
mod play_history_tests {
    use super::tests_support::sample_track;
    use super::*;

    #[test]
    fn plays_made_offline_keep_their_time_and_count_once() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.upsert_track(&sample_track("/m/a.flac", "A")).unwrap();
        let then = now_unix() - 3600;
        assert!(lib.record_play_at(a, Some("app"), then).unwrap());
        // The same report again (the answer got lost): not a second play
        assert!(!lib.record_play_at(a, Some("app"), then).unwrap());
        // From a device whose clock runs a few minutes ahead: its time, so a resend still matches
        let ahead = now_unix() + 300;
        assert!(lib.record_play_at(a, Some("app"), ahead).unwrap());
        assert!(!lib.record_play_at(a, Some("app"), ahead).unwrap());
        // Far in the future: now
        assert!(lib.record_play_at(a, None, now_unix() + 86_400).unwrap());
        let h = lib.history(10).unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].played_at, ahead);
        assert!(h[1].played_at <= now_unix());
        assert_eq!(h[2].played_at, then);
        assert_eq!(lib.play_count(a).unwrap(), 3);
    }

    #[test]
    fn plays_feed_history_and_counts() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.upsert_track(&sample_track("/m/a.flac", "A")).unwrap();
        let b = lib.upsert_track(&sample_track("/m/b.flac", "B")).unwrap();
        assert!(lib.history(10).unwrap().is_empty());
        assert!(lib.recently_played_albums(10).unwrap().is_empty());
        lib.record_play(a, Some("zone-1")).unwrap();
        lib.record_play(b, None).unwrap();
        lib.record_play(a, Some("zone-1")).unwrap();
        let h = lib.history(10).unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].track.id, Some(a));
        assert_eq!(h[0].zone.as_deref(), Some("zone-1"));
        assert_eq!(h[1].track.id, Some(b));
        assert_eq!(h[1].zone, None);
        assert_eq!(lib.history(1).unwrap().len(), 1);
        let most = lib.most_played_tracks(10).unwrap();
        assert_eq!(most[0].track.id, Some(a));
        assert_eq!(most[0].plays, 2);
        assert_eq!(most[1].plays, 1);
        assert_eq!(lib.play_count(a).unwrap(), 2);
        let albums = lib.recently_played_albums(10).unwrap();
        assert_eq!(albums.len(), 1, "both tracks share one album");
        assert_eq!(albums[0].title, "Random Access Memories");
        assert_eq!(lib.most_played_albums(10).unwrap().len(), 1);
        // Removing the track removes its plays.
        assert!(lib.remove_track(a).unwrap());
        assert_eq!(lib.history(10).unwrap().len(), 1);
    }

    #[test]
    fn favourites_lifecycle() {
        let lib = Library::open_in_memory().unwrap();
        let t = lib.upsert_track(&sample_track("/m/a.flac", "A")).unwrap();
        let track = lib.get_track(t).unwrap().unwrap();
        let uri = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69";
        assert!(lib.list_favourites().unwrap().is_empty());
        let row = lib.add_favourite("id1", uri, 1_000).unwrap();
        assert_eq!(row.synced_at, None);
        assert_eq!(lib.unsynced_favourites().unwrap().len(), 1);
        lib.mark_favourite_synced("id1", 2_000).unwrap();
        assert!(lib.unsynced_favourites().unwrap().is_empty());
        // Adding again is a no-op that keeps the sync state.
        let again = lib.add_favourite("id1", uri, 5_000).unwrap();
        assert_eq!(again.created_at, 1_000);
        assert_eq!(again.synced_at, Some(2_000));
        assert_eq!(lib.list_favourites().unwrap().len(), 1);
        // Remove → tombstone → push → purged.
        assert!(lib.remove_favourite(uri).unwrap());
        assert!(!lib.remove_favourite(uri).unwrap());
        assert!(lib.list_favourites().unwrap().is_empty());
        let pending = lib.unsynced_favourites().unwrap();
        assert!(pending[0].deleted);
        lib.mark_favourite_synced("id1", 3_000).unwrap();
        assert!(lib.favourite_by_uri(uri).unwrap().is_none());
        // Re-adding after a purge starts fresh.
        let fresh = lib.add_favourite("id1", uri, 9_000).unwrap();
        assert_eq!(fresh.created_at, 9_000);
        // Hearted again while the clear waits to be pushed: the new time
        assert!(lib.remove_favourite(uri).unwrap());
        let revived = lib.add_favourite("id1", uri, 12_000).unwrap();
        assert_eq!(revived.created_at, 12_000);
        assert!(!revived.deleted);
        // Album / artist lookups by MBID.
        let album = track.album.as_ref().unwrap();
        let found = lib
            .album_by_release_mbid(album.release_mbid.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(found.title, album.title);
        let artist = lib
            .artist_by_mbid(track.artists[0].mbid.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(artist.name, "Daft Punk");
    }
}

#[cfg(test)]
mod tag_tests {
    use super::*;

    #[test]
    fn tags_lifecycle() {
        let lib = Library::open_in_memory().unwrap();
        let uri = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69";
        let t = lib.add_tag("t1", uri, "disco", 1_000).unwrap();
        assert_eq!(t.label, "disco");
        lib.add_tag("t2", uri, "funk", 2_000).unwrap();
        lib.add_tag(
            "t3",
            "https://musicbrainz.org/release/833f00e1-781f-4edd-90e4-e52712618862",
            "disco",
            3_000,
        )
        .unwrap();
        assert_eq!(lib.list_tags(Some(uri)).unwrap().len(), 2);
        assert_eq!(lib.list_tags(None).unwrap().len(), 3);
        assert_eq!(lib.unsynced_tags().unwrap().len(), 3);
        lib.mark_tag_synced("t1", 5_000).unwrap();
        let again = lib.add_tag("t1", uri, "disco", 9_000).unwrap();
        assert_eq!(again.synced_at, Some(5_000), "re-adding keeps the sync state");
        assert!(lib.remove_tag(uri, "disco").unwrap());
        assert!(!lib.remove_tag(uri, "disco").unwrap());
        assert_eq!(lib.list_tags(Some(uri)).unwrap().len(), 1);
        let pending = lib.unsynced_tags().unwrap();
        assert!(pending.iter().any(|t| t.id == "t1" && t.deleted));
        lib.mark_tag_synced("t1", 6_000).unwrap();
        assert!(
            lib.tag_by_id("t1").unwrap().is_none(),
            "pushed tombstones are purged"
        );
    }
}

#[cfg(test)]
mod folder_tests {
    use super::tests_support::sample_track;
    use super::*;

    #[test]
    fn remove_folder_drops_its_tracks_and_empty_albums() {
        let lib = Library::open_in_memory().unwrap();
        lib.add_folder(Path::new("/m/a")).unwrap();
        lib.add_folder(Path::new("/m/b")).unwrap();
        let a = lib.upsert_track(&sample_track("/m/a/one.flac", "One")).unwrap();
        let b = lib.upsert_track(&sample_track("/m/b/two.flac", "Two")).unwrap();
        let mut other = sample_track("/m/b/three.flac", "Three");
        other.album.as_mut().unwrap().title = "Other".into();
        other.album.as_mut().unwrap().release_mbid = None;
        let c = lib.upsert_track(&other).unwrap();
        lib.record_play(a, None).unwrap();
        assert_eq!(lib.stats().unwrap().albums, 2);
        let (files, tracks) = lib.remove_folder(Path::new("/m/b")).unwrap();
        assert_eq!((files, tracks), (2, 2));
        assert!(lib.get_track(a).unwrap().is_some());
        assert!(lib.get_track(b).unwrap().is_none());
        assert!(lib.get_track(c).unwrap().is_none());
        assert_eq!(
            lib.stats().unwrap().albums,
            1,
            "the album only track three used is gone"
        );
        assert_eq!(lib.list_folders().unwrap().len(), 1);
        assert!(
            lib.search_tracks("Three", 10).unwrap().is_empty(),
            "FTS rows go too"
        );
        assert_eq!(lib.history(10).unwrap().len(), 1);
        // A prefix that is not a path boundary is left alone.
        lib.add_folder(Path::new("/m")).unwrap();
        let (files, _) = lib.remove_folder(Path::new("/m/a_other")).unwrap();
        assert_eq!(files, 0);
        assert!(lib.get_track(a).unwrap().is_some());
    }
}

#[cfg(test)]
mod genre_tests {
    use super::tests_support::sample_track;
    use super::*;

    #[test]
    fn genres_are_stored_browsable_and_searchable() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.upsert_track(&sample_track("/m/a.flac", "A")).unwrap();
        let mut jazz = sample_track("/m/b.flac", "B");
        jazz.genres = vec!["jazz".into()];
        jazz.album.as_mut().unwrap().title = "Blue".into();
        jazz.album.as_mut().unwrap().release_mbid = None;
        let b = lib.upsert_track(&jazz).unwrap();
        let t = lib.get_track(a).unwrap().unwrap();
        assert_eq!(t.genres, vec!["Disco", "Electronic"]);
        let genres = lib.list_genres().unwrap();
        let names: Vec<&str> = genres.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["Disco", "Electronic", "jazz"]);
        let disco = genres.iter().find(|g| g.name == "Disco").unwrap();
        assert_eq!((disco.tracks, disco.albums), (1, 1));
        assert_eq!(
            lib.genre_albums(disco.id).unwrap()[0].title,
            "Random Access Memories"
        );
        assert!(lib.genre_tracks(disco.id, 0).unwrap().is_empty(), "limit 0");
        assert_eq!(lib.genre_tracks(disco.id, 10).unwrap()[0].id, Some(a));
        assert!(
            lib.search_tracks("jazz", 10)
                .unwrap()
                .iter()
                .any(|t| t.id == Some(b)),
            "genre words match in search"
        );
        // Re-upserting with another genre replaces, case-insensitively de-duplicated.
        let mut again = sample_track("/m/a.flac", "A");
        again.genres = vec!["DISCO".into()];
        lib.upsert_track(&again).unwrap();
        let genres = lib.list_genres().unwrap();
        assert_eq!(
            genres.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            vec!["Disco", "jazz"]
        );
        assert!(lib.genre_by_name("disco").unwrap().is_some());
        lib.add_folder(Path::new("/m")).unwrap();
        lib.remove_folder(Path::new("/m")).unwrap();
        assert!(
            lib.list_genres().unwrap().is_empty(),
            "orphan genres go with their tracks"
        );
    }
}

#[cfg(test)]
mod identify_tests {
    use super::tests_support::sample_track;
    use super::*;
    use crate::identify::Candidate;

    const BOHEMIAN: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";

    /// A scanned file whose tags carry no IDs.
    fn untagged(path: &str, title: &str) -> Track {
        let mut t = sample_track(path, title);
        t.recording_mbid = None;
        t.isrc = None;
        t.canonical = None;
        t
    }

    #[test]
    fn suggestions_need_confirmation_and_identities_survive_rescans() {
        let lib = Library::open_in_memory().unwrap();
        let t = untagged("/music/Queen/11.flac", "Bohemian Rhapsody");
        let id = lib.upsert_track(&t).unwrap().0;
        assert_eq!(lib.tracks_to_identify(false).unwrap().len(), 1);
        let c = |mbid: &str, score: f32| Candidate {
            recording: Mbid::parse(mbid).unwrap(),
            release: None,
            title: Some("Bohemian Rhapsody".into()),
            artist: Some("Queen".into()),
            release_title: None,
            score,
            source: "fingerprint".into(),
        };
        lib.add_suggestions(
            id,
            &[c(BOHEMIAN, 0.95), c("ebf79ba5-085e-48d2-9eb8-2d992fbf0f6d", 0.93)],
        )
        .unwrap();
        lib.set_identify_state(id, "suggested", None).unwrap();
        // A suggestion is not an identity.
        assert!(lib.get_track(TrackId(id)).unwrap().unwrap().canonical.is_none());
        let counts = lib.identity_counts().unwrap();
        assert_eq!((counts.identified, counts.suggested, counts.unchecked), (0, 1, 0));
        assert!(
            lib.tracks_to_identify(true).unwrap().is_empty(),
            "suggestions wait for the owner"
        );
        let rows = lib.suggestions().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].recording_mbid, BOHEMIAN, "best first");
        let (tid, uri) = lib.accept_suggestion(rows[0].id).unwrap().unwrap();
        assert_eq!(tid, id);
        assert_eq!(
            uri.to_url(),
            format!("https://musicbrainz.org/recording/{BOHEMIAN}")
        );
        let tr = lib.get_track(TrackId(id)).unwrap().unwrap();
        assert_eq!(tr.canonical.as_ref().unwrap().1, Confidence::ConfirmedByUser);
        assert!(lib.suggestions().unwrap().is_empty());
        // A rescan of the same untagged file keeps the confirmed identity.
        lib.upsert_track(&t).unwrap();
        assert_eq!(
            lib.get_track(TrackId(id)).unwrap().unwrap().canonical.unwrap().1,
            Confidence::ConfirmedByUser
        );
        assert_eq!(lib.tracks_by_canonical(&uri).unwrap().len(), 1);
        // Tags with an MBID win over identification and are never overwritten by it.
        let mut tagged = t.clone();
        let other = Mbid::parse("ebf79ba5-085e-48d2-9eb8-2d992fbf0f6d").unwrap();
        tagged.canonical = Some((MusicUri::Recording { mbid: other }, Confidence::TaggedMbid));
        lib.upsert_track(&tagged).unwrap();
        assert!(!lib
            .set_identity(id, &uri, Confidence::Fingerprint { score: 0.99 }, Some("a"))
            .unwrap());
        assert_eq!(
            lib.get_track(TrackId(id)).unwrap().unwrap().canonical.unwrap().1,
            Confidence::TaggedMbid
        );
    }

    #[test]
    fn rejecting_the_last_suggestion_leaves_the_track_unknown() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib.upsert_track(&untagged("/m/a.flac", "A")).unwrap().0;
        lib.add_suggestions(
            id,
            &[Candidate {
                recording: Mbid::parse(BOHEMIAN).unwrap(),
                release: None,
                title: None,
                artist: None,
                release_title: None,
                score: 0.6,
                source: "search".into(),
            }],
        )
        .unwrap();
        let s = lib.suggestions().unwrap();
        assert!(lib.reject_suggestion(s[0].id).unwrap());
        assert_eq!(lib.identity_counts().unwrap().unknown, 1);
        assert_eq!(
            lib.tracks_to_identify(true).unwrap().len(),
            1,
            "retry looks again"
        );
        assert!(lib.tracks_to_identify(false).unwrap().is_empty());
    }

    #[test]
    fn folder_tracks_and_file_keys() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let p = |n: &str| dir.path().join(n).to_string_lossy().to_string();
        lib.upsert_track(&sample_track(&p("a.flac"), "A")).unwrap();
        lib.upsert_track(&sample_track(&p("b.flac"), "B")).unwrap();
        lib.upsert_track(&sample_track(&p("sub/c.flac"), "C")).unwrap();
        assert_eq!(lib.tracks_in_folder(dir.path()).unwrap().len(), 2);
        std::fs::write(dir.path().join("x.bin"), vec![7u8; 3 << 20]).unwrap();
        let k1 = Library::file_key(&dir.path().join("x.bin")).unwrap();
        std::fs::write(dir.path().join("y.bin"), vec![7u8; 3 << 20]).unwrap();
        assert_eq!(k1, Library::file_key(&dir.path().join("y.bin")).unwrap());
        std::fs::write(dir.path().join("y.bin"), vec![8u8; 3 << 20]).unwrap();
        assert_ne!(k1, Library::file_key(&dir.path().join("y.bin")).unwrap());
    }
}
