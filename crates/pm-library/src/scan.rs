//! The folder scanner: walks a root, probes new or changed files, upserts
//! them into the catalogue, and flags files that disappeared.
//!
//! Change detection is by `(size_bytes, mtime_unix)` where the mtime is the
//! newer of the audio file and its sidecar `.lrc`; an unchanged file is
//! skipped without opening it. Files that vanish are **kept** with
//! `files.missing = 1` (see the schema) so that playlists, history and
//! canonical-URI lookups still resolve; a later scan that finds the path
//! again clears the flag.
//!
//! Albums: disc folders (`CD1`, `Disc 2`) belong to the album folder above them, and an album
//! without an album artist whose songs name different artists is "Various Artists". Both
//! follow Music Assistant (github.com/music-assistant/server, Apache-2.0, Copyright The Music
//! Assistant Authors): `helpers/util.py` and `providers/filesystem_local`. See NOTICE.

use crate::canonical;
use crate::db::{Library, ScanCounts};
use crate::error::{LibraryError, Result};
use crate::tags::{self, TagSet};
use pm_model::{Album, Artist, AudioFile, Track, TrackId};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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
    /// Per album folder: for each album title (lower case) among its files without an album
    /// artist, their main artists (lower case). Read once per folder in a scan.
    folders: Mutex<HashMap<PathBuf, HashMap<String, HashSet<String>>>>,
    /// Per folder: its cover image, looked for once in a scan.
    covers: Mutex<HashMap<PathBuf, Option<(PathBuf, String)>>>,
}

/// Folder image names tried, in order, when a file has no embedded picture.
const FOLDER_COVERS: &[&str] = &["cover", "folder", "front", "album", "artwork"];

/// The album artist of an album whose songs have different artists and no album artist tag.
pub const VARIOUS_ARTISTS: &str = "Various Artists";

/// The library setting, per root, saying its albums were regrouped by the folder rules
/// (`scan.album_rules:<root>`); its value is [`ALBUM_RULES`].
const ALBUM_RULES_KEY: &str = "scan.album_rules:";
const ALBUM_RULES: &str = "1";

impl<'a> Scanner<'a> {
    pub fn new(lib: &'a Library) -> Self {
        Self {
            lib,
            art_dir: None,
            folders: Mutex::new(HashMap::new()),
            covers: Mutex::new(HashMap::new()),
        }
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
        self.cover_from(album_id, tags, || folder_cover(path))
    }

    /// [`Self::cover_for`] with the folder's image (`folder`) looked for already.
    fn cover_from(
        &self,
        album_id: i64,
        tags: &TagSet,
        folder: impl FnOnce() -> Option<(PathBuf, String)>,
    ) -> Result<()> {
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
        if let Some((file, mime)) = folder() {
            return self.lib.set_album_art(&crate::db::AlbumArt {
                album_id,
                file,
                mime,
                source: "folder".into(),
            });
        }
        Ok(())
    }

    /// Give albums without a cover the folder image next to (or above) one of their files, if
    /// there is one now: a cover added later, or one in the album folder above `CD1/`.
    fn fill_folder_covers(&self) -> Result<()> {
        if self.art_dir.is_none() {
            return Ok(());
        }
        for (album_id, path) in self.lib.albums_without_art()? {
            if let Some((file, mime)) = folder_cover(&path) {
                self.lib.set_album_art(&crate::db::AlbumArt {
                    album_id,
                    file,
                    mime,
                    source: "folder".into(),
                })?;
            }
        }
        Ok(())
    }

    /// The number of different main artists among the files of `path`'s album folder (disc
    /// folders included) that have the album title `album` and no album artist tag.
    fn folder_artist_count(&self, path: &Path, album: &str) -> usize {
        let Some(folder) = album_folder(path) else {
            return 0;
        };
        let mut folders = self.folders.lock().unwrap_or_else(|e| e.into_inner());
        let albums = folders
            .entry(folder.clone())
            .or_insert_with(|| read_folder_albums(&folder));
        albums.get(&album.to_lowercase()).map_or(0, HashSet::len)
    }

    /// Give every album without a cover one again, from one of its files (after a restore on
    /// another machine, where the extracted covers were not carried over). Returns how many.
    pub fn fill_missing_art(&self) -> Result<u64> {
        if self.art_dir.is_none() {
            return Ok(0);
        }
        let mut n = 0;
        for (album_id, path) in self.lib.albums_without_art()? {
            let Ok((_, tagset)) = tags::probe_and_read(&path) else {
                continue;
            };
            if self.cover_for(&path, album_id, &tagset).is_ok() && self.lib.album_art(album_id)?.is_some() {
                n += 1;
            }
        }
        Ok(n)
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
        // Once per root: re-read the files of albums that one folder was split into (before
        // the "Various Artists" rule), so existing libraries regroup without a full re-read.
        let rules_key = format!("{ALBUM_RULES_KEY}{}", root.display());
        let regroup = if self.lib.get_setting(&rules_key)?.as_deref() == Some(ALBUM_RULES) {
            HashSet::new()
        } else {
            split_album_files(self.lib.album_files_under(&root)?)
        };
        // Files are read with no transaction open (over a share the reading is the slow part);
        // their writes are queued and made in short transactions (see [`Writes`]).
        let mut writes = Writes::default();

        for entry in WalkDir::new(&root).follow_links(true).into_iter() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    let p = e.path().map(Path::to_path_buf).unwrap_or_else(|| root.clone());
                    report.errors.push((p, e.to_string()));
                    continue;
                }
            };
            if !entry.file_type().is_file()
                || !tags::is_supported(entry.path())
                || is_apple_double(entry.path())
            {
                continue;
            }
            report.files_seen += 1;
            self.flush(&mut writes, scan_id, &mut report, false)?;
            let path = entry.path().to_path_buf();
            // Taken out of the library: not read, and its row stays missing
            if self.lib.is_hidden(&path)? {
                continue;
            }
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
                if row.size_bytes == size && row.mtime_unix == mtime && !regroup.contains(&path) {
                    writes.seen(path);
                    report.files_unchanged += 1;
                    continue;
                }
            }
            let read = tags::probe_and_read(&path).map(|p| self.prepare(&path, p));
            writes.read(ReadFile {
                path,
                size,
                mtime,
                existing: existing.map(|r| r.track_id),
                read,
            });
        }
        self.flush(&mut writes, scan_id, &mut report, true)?;
        report.files_removed = self.lib.mark_missing_under(&root, scan_id)?;
        self.lib.prune_empty_albums()?;
        self.lib.set_setting(&rules_key, ALBUM_RULES)?;
        if let Err(e) = self.fill_folder_covers() {
            tracing::warn!("folder covers: {e}");
        }
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

    /// What a file's write needs from disk besides its tags, read before the write (with no
    /// transaction open): whether its folder is a mix of artists, its `.lrc`, the folder's cover.
    fn prepare(&self, path: &Path, (probe, mut tags): (tags::FileProbe, TagSet)) -> Prepared {
        // No album artist, and the album's other files name other artists: "Various Artists",
        // so the album stays one album (local files; a remote share's folders aren't read).
        if tags.album_artist.is_none() && !tags.compilation {
            if let Some(album) = &tags.album {
                if path.is_absolute() && self.folder_artist_count(path, album) > 1 {
                    tags.album_artist = Some(VARIOUS_ARTISTS.into());
                }
            }
        }
        // A sidecar .lrc (usually synchronised) wins over embedded text.
        let lyrics = crate::lyrics::sidecar(path)
            .map(|t| (t, "lrc"))
            .or_else(|| tags.lyrics.clone().map(|t| (t, "tag")));
        let folder_cover = match (path.parent(), &tags.album, &tags.picture, &self.art_dir) {
            (Some(dir), Some(_), None, Some(_)) => self
                .covers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(dir.to_path_buf())
                .or_insert_with(|| folder_cover(path))
                .clone(),
            _ => None,
        };
        Prepared {
            probe,
            tags,
            lyrics,
            folder_cover,
        }
    }

    /// Writes a file read by [`Self::prepare`] (local or remote).
    fn write_file(
        &self,
        path: &Path,
        size: u64,
        mtime: i64,
        existing: Option<TrackId>,
        p: Prepared,
    ) -> Result<TrackId> {
        let file = AudioFile {
            path: path.to_path_buf(),
            size_bytes: size,
            mtime_unix: mtime,
            container: p.probe.container,
            format: p.probe.format,
            duration_ms: p.probe.duration_ms,
        };
        let mut track = track_from_tags(file, &p.tags);
        track.id = existing;
        // The owner's edit of this file's tags stays on top of what the file says
        if let Some(o) = self.lib.refresh_override(&track)? {
            o.apply(&mut track);
        }
        let id = self.lib.upsert_track(&track)?;
        self.lib
            .set_lyrics(id, p.lyrics.as_ref().map(|(t, s)| (t.as_str(), *s)))?;
        if p.tags.album.is_some() && self.art_dir.is_some() {
            if let Some(album_id) = self.lib.get_track(id)?.and_then(|t| t.album.and_then(|a| a.id)) {
                if let Err(e) = self.cover_from(album_id, &p.tags, || p.folder_cover.clone()) {
                    tracing::warn!(path = %path.display(), "cover art: {e}");
                }
            }
        }
        Ok(id)
    }

    /// Makes the queued writes in one transaction when they are due ([`Writes::due`]) or `now`.
    fn flush(&self, w: &mut Writes, scan_id: i64, report: &mut ScanReport, now: bool) -> Result<()> {
        if w.is_empty() || !(now || w.due()) {
            return Ok(());
        }
        begin_waiting(self.lib)?;
        let written = self.write_queued(w, scan_id, report);
        // What was written stays, also when a write failed
        let committed = self.lib.commit_batch();
        written.and(committed)
    }

    fn write_queued(&self, w: &mut Writes, scan_id: i64, report: &mut ScanReport) -> Result<()> {
        let (seen, read) = w.take();
        for path in seen {
            self.lib.mark_file_seen(&path, scan_id)?;
        }
        for f in read {
            let existing = f.existing;
            match f
                .read
                .and_then(|p| self.write_file(&f.path, f.size, f.mtime, existing, p))
            {
                Ok(_) => {
                    self.lib.mark_file_seen(&f.path, scan_id)?;
                    if existing.is_some() {
                        report.files_updated += 1;
                    } else {
                        report.files_added += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %f.path.display(), "skipping file: {e}");
                    report.errors.push((f.path, e.to_string()));
                }
            }
        }
        Ok(())
    }
}

/// A file read for the scan, with what its write needs.
struct Prepared {
    probe: tags::FileProbe,
    tags: TagSet,
    lyrics: Option<(String, &'static str)>,
    folder_cover: Option<(PathBuf, String)>,
}

/// A changed file the scan read (or failed to), waiting for its write.
struct ReadFile {
    path: PathBuf,
    size: u64,
    mtime: i64,
    existing: Option<TrackId>,
    read: Result<Prepared>,
}

/// A scan's writes, queued while it reads files and made in one short transaction (see
/// [`Scanner::flush`]). Reading a file, over a network share above all, can take seconds; a
/// transaction held meanwhile kept every other writer (Identify, the API) waiting until it gave
/// up with "database is locked".
#[derive(Default)]
struct Writes {
    seen: Vec<PathBuf>,
    read: Vec<ReadFile>,
    /// Bytes of embedded pictures the queued files hold.
    picture_bytes: usize,
    since: Option<std::time::Instant>,
}

impl Writes {
    /// Unchanged files (a rescan): one commit per this many.
    const FILES: usize = 250;
    /// Changed files read, held with their tags (and pictures) until written.
    const READ: usize = 64;
    const PICTURE_BYTES: usize = 32 << 20;
    /// New files appear in the library at least this often.
    const MAX_AGE: std::time::Duration = std::time::Duration::from_millis(500);

    fn seen(&mut self, path: PathBuf) {
        self.since.get_or_insert_with(std::time::Instant::now);
        self.seen.push(path);
    }

    fn read(&mut self, f: ReadFile) {
        self.since.get_or_insert_with(std::time::Instant::now);
        if let Ok(p) = &f.read {
            self.picture_bytes += p.tags.picture.as_ref().map_or(0, |pic| pic.data.len());
        }
        self.read.push(f);
    }

    fn is_empty(&self) -> bool {
        self.seen.is_empty() && self.read.is_empty()
    }

    fn due(&self) -> bool {
        self.seen.len() + self.read.len() >= Self::FILES
            || self.read.len() >= Self::READ
            || self.picture_bytes >= Self::PICTURE_BYTES
            || self.since.is_some_and(|t| t.elapsed() >= Self::MAX_AGE)
    }

    fn take(&mut self) -> (Vec<PathBuf>, Vec<ReadFile>) {
        self.picture_bytes = 0;
        self.since = None;
        (std::mem::take(&mut self.seen), std::mem::take(&mut self.read))
    }
}

/// A file in a remote library root (a WebDAV share): its key in the library (`webdav://<share>/…`),
/// size and modification time from the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
}

impl<'a> Scanner<'a> {
    /// [`Scanner::scan`] for a remote root: `entries` is its listing, `root` the key prefix
    /// (`webdav://<share>`), and `open` reads one file (ranged, so only the tags are fetched).
    /// Unchanged files (same size and time) are skipped; files gone from the listing are
    /// marked missing. Covers come from embedded art; folder covers and `.lrc` sidecars are
    /// local-only for now.
    pub fn scan_remote(
        &self,
        root: &Path,
        entries: Vec<RemoteEntry>,
        open: &dyn Fn(&RemoteEntry) -> std::io::Result<Box<dyn tags::RemoteRead>>,
    ) -> Result<ScanReport> {
        let scan_id = self.lib.begin_scan(root)?;
        let mut report = ScanReport {
            scan_id,
            ..Default::default()
        };
        let mut writes = Writes::default();
        for e in entries {
            if !tags::is_supported(&e.path) {
                continue;
            }
            report.files_seen += 1;
            self.flush(&mut writes, scan_id, &mut report, false)?;
            if self.lib.is_hidden(&e.path)? {
                continue;
            }
            let existing = self.lib.file_by_path(&e.path)?;
            if let Some(row) = &existing {
                if row.size_bytes == e.size && row.mtime_unix == e.mtime {
                    writes.seen(e.path.clone());
                    report.files_unchanged += 1;
                    continue;
                }
            }
            let read = tags::probe_and_read_from(&e.path, &|| open(&e)).map(|p| self.prepare(&e.path, p));
            writes.read(ReadFile {
                path: e.path.clone(),
                size: e.size,
                mtime: e.mtime,
                existing: existing.map(|r| r.track_id),
                read,
            });
        }
        self.flush(&mut writes, scan_id, &mut report, true)?;
        report.files_removed = self.lib.mark_missing_under(root, scan_id)?;
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
}

/// `._name.flac`: the resource fork a Mac writes next to each file on a share (AppleDouble), not
/// audio. Reading them failed every one and filled the scan's errors.
fn is_apple_double(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("._"))
}

/// How often a batch is tried again while another connection keeps the database busy; each try
/// waits up to the connection's busy timeout.
const BUSY_RETRIES: u32 = 4;

/// Begin a batch, waiting through a busy database (each try waits the busy timeout).
fn begin_waiting(lib: &Library) -> Result<()> {
    let mut tries = 0;
    loop {
        match lib.begin_batch() {
            Err(e) if e.is_busy() && tries < BUSY_RETRIES => {
                tries += 1;
                tracing::info!("scan waits for the database (another writer holds it): {e}");
            }
            other => return other,
        }
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

/// The cover image next to `audio_path`, else, in a disc folder (`CD1`, `Disc 2`), in the album
/// folder above it.
pub fn folder_cover(audio_path: &Path) -> Option<(PathBuf, String)> {
    let dir = audio_path.parent()?;
    cover_in(dir).or_else(|| {
        if is_disc_folder(dir) {
            cover_in(dir.parent()?)
        } else {
            None
        }
    })
}

/// A disc folder of a multi-disc album: `CD1`, `CD 2`, `Disc 01`, `disk-3`, `Vol. 2`,
/// `Volume 4`, `DVD 1`, optionally followed by a subtitle (`CD2 - Live`). As in Music
/// Assistant's `helpers/util.py` (Apache-2.0).
pub fn is_disc_folder(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.trim().to_lowercase();
    let Some(rest) = ["disc", "disk", "cd", "dvd", "volume", "vol"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
    else {
        return false;
    };
    let rest = rest.trim_start_matches([' ', '.', '_', '-']);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && !rest[digits..].starts_with(|c: char| c.is_alphanumeric())
}

/// The folder an album lives in: the file's folder, or the one above a disc folder.
pub fn album_folder(audio_path: &Path) -> Option<PathBuf> {
    let dir = audio_path.parent()?;
    if is_disc_folder(dir) {
        dir.parent().map(Path::to_path_buf)
    } else {
        Some(dir.to_path_buf())
    }
}

/// The audio files of an album folder and its disc folders.
fn album_folder_files(folder: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(folder) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if is_disc_folder(&p) {
                if let Ok(inner) = std::fs::read_dir(&p) {
                    out.extend(
                        inner
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| tags::is_supported(p)),
                    );
                }
            }
        } else if tags::is_supported(&p) {
            out.push(p);
        }
    }
    out
}

/// For each album title (lower case) among the folder's files without an album artist tag or a
/// compilation flag, their main artists: the first artist of the first credit, lower case.
fn read_folder_albums(folder: &Path) -> HashMap<String, HashSet<String>> {
    let mut albums: HashMap<String, HashSet<String>> = HashMap::new();
    for p in album_folder_files(folder) {
        let Ok((_, t)) = tags::probe_and_read(&p) else {
            continue;
        };
        let (Some(album), None, false) = (&t.album, &t.album_artist, t.compilation) else {
            continue;
        };
        let main = if let Some(a) = t.artist_list.first() {
            a.clone()
        } else if let Some(credit) = t.artists.first() {
            crate::credits::split_all(std::slice::from_ref(credit))
                .into_iter()
                .next()
                .unwrap_or_else(|| credit.clone())
        } else {
            continue;
        };
        albums
            .entry(album.to_lowercase())
            .or_default()
            .insert(main.trim().to_lowercase());
    }
    albums
}

/// Files whose album shares its title and album folder with another album: one album split by
/// track artist before the "Various Artists" rule.
fn split_album_files(rows: Vec<(PathBuf, i64, String)>) -> HashSet<PathBuf> {
    let mut groups: HashMap<(PathBuf, String), (HashSet<i64>, Vec<PathBuf>)> = HashMap::new();
    for (path, album_id, title) in rows {
        let Some(folder) = album_folder(&path) else {
            continue;
        };
        let g = groups.entry((folder, title.to_lowercase())).or_default();
        g.0.insert(album_id);
        g.1.push(path);
    }
    groups
        .into_values()
        .filter(|(ids, _)| ids.len() > 1)
        .flat_map(|(_, paths)| paths)
        .collect()
}

fn cover_in(dir: &Path) -> Option<(PathBuf, String)> {
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
    // "Sefa, Dr. Peacock" names two artists; a file whose MusicBrainz ids say one artist stays
    // whole (crate::credits)
    let names: Vec<String> = if !t.artist_list.is_empty() {
        t.artist_list.clone()
    } else if t.artist_mbids.len() == 1 && t.artists.len() == 1 {
        t.artists.clone()
    } else {
        crate::credits::split_all(&t.artists)
    };
    let ids_line_up = names.len() == t.artist_mbids.len() || names.len() == t.artists.len();
    let artists = names
        .iter()
        .enumerate()
        .map(|(i, name)| Artist {
            id: None,
            name: name.clone(),
            mbid: if ids_line_up {
                t.artist_mbids.get(i).copied()
            } else {
                None
            },
        })
        .collect();
    let album = t.album.as_ref().map(|title| Album {
        id: None,
        title: title.clone(),
        album_artist: t.album_artist.clone().or_else(|| {
            // The main artist, not the whole credit: "D feat. E" on D's album stays D's.
            if t.compilation {
                Some(VARIOUS_ARTISTS.into())
            } else {
                names.first().or(t.artists.first()).cloned()
            }
        }),
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

    /// A WAV with `artist`, `album` and, when given, the ID3 compilation flag.
    fn write_album_wav(path: &Path, title: &str, artist: &str, album: &str, compilation: bool) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_wav(path, 441, title, false);
        let mut t = id3::Tag::read_from_path(path).unwrap();
        t.set_artist(artist);
        t.set_album(album);
        if compilation {
            t.set_text("TCMP", "1");
        }
        t.write_to_path(path, id3::Version::Id3v24).unwrap();
    }

    #[test]
    fn various_artists_compilations_and_disc_folders() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // A two-disc mix without album artist tags: one album, "Various Artists", the cover
        // in the folder above the disc folders.
        write_album_wav(&d.join("Mix/CD1/01.wav"), "One", "Sefa", "Mix", false);
        write_album_wav(&d.join("Mix/CD1/02.wav"), "Two", "Dr. Peacock", "Mix", false);
        write_album_wav(&d.join("Mix/CD 2/01.wav"), "Three", "Cyclon", "Mix", false);
        fs::write(d.join("Mix/cover.jpg"), b"\xff\xd8jpeg").unwrap();
        // A compilation flag alone is enough.
        write_album_wav(&d.join("Comp/01.wav"), "Four", "Angerfist", "Comp", true);
        // One artist with a guest: the album stays the artist's.
        write_album_wav(&d.join("Solo/01.wav"), "Five", "Daft Punk", "Solo", false);
        write_album_wav(
            &d.join("Solo/02.wav"),
            "Six",
            "Daft Punk feat. Pharrell Williams",
            "Solo",
            false,
        );

        let lib = Library::open_in_memory().unwrap();
        let art = d.join("art");
        Scanner::new(&lib).with_art_dir(&art).scan(d).unwrap();
        let albums = lib.list_albums(None, 20).unwrap();
        let by = |title: &str| -> Vec<Option<String>> {
            albums
                .iter()
                .filter(|a| a.title == title)
                .map(|a| a.album_artist.clone())
                .collect()
        };
        assert_eq!(by("Mix"), vec![Some(VARIOUS_ARTISTS.to_string())]);
        assert_eq!(by("Comp"), vec![Some(VARIOUS_ARTISTS.to_string())]);
        assert_eq!(by("Solo"), vec![Some("Daft Punk".to_string())]);
        let mix = albums.iter().find(|a| a.title == "Mix").unwrap().id.unwrap();
        let cover = lib.album_art(mix).unwrap().unwrap();
        assert_eq!(cover.source, "folder");
        assert!(cover.file.ends_with("Mix/cover.jpg"));
        // A rescan changes nothing.
        let r = Scanner::new(&lib).with_art_dir(&art).scan(d).unwrap();
        assert_eq!(r.files_unchanged, 6);
        assert_eq!(lib.list_albums(None, 20).unwrap().len(), 3);
    }

    #[test]
    fn disc_folder_names() {
        for yes in [
            "CD1",
            "cd 2",
            "Disc 01",
            "disk-3",
            "Vol. 2",
            "Volume 4",
            "DVD1",
            "CD2 - Live",
        ] {
            assert!(is_disc_folder(Path::new(yes)), "{yes}");
        }
        for no in ["Disco 2", "CDs", "cd", "Volume", "2 CD", "Discovery", "CD1a"] {
            assert!(!is_disc_folder(Path::new(no)), "{no}");
        }
        assert_eq!(
            album_folder(Path::new("/m/Mix/CD1/01.flac")),
            Some(PathBuf::from("/m/Mix"))
        );
        assert_eq!(
            album_folder(Path::new("/m/Mix/01.flac")),
            Some(PathBuf::from("/m/Mix"))
        );
    }

    #[test]
    fn albums_split_in_one_folder_are_found() {
        let rows = vec![
            (PathBuf::from("/m/Mix/CD1/01.flac"), 1, "Mix".to_string()),
            (PathBuf::from("/m/Mix/CD2/01.flac"), 2, "mix".to_string()),
            (PathBuf::from("/m/Solo/01.flac"), 3, "Solo".to_string()),
            (PathBuf::from("/m/Solo/02.flac"), 3, "Solo".to_string()),
            (PathBuf::from("/m/Other/01.flac"), 4, "Mix".to_string()),
        ];
        let got = split_album_files(rows);
        let want: HashSet<PathBuf> = ["/m/Mix/CD1/01.flac", "/m/Mix/CD2/01.flac"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got, want);
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
