//! `pm-node scan`, `tracks`, `search`, `stats`, `resolve`: the library side of M1.

use crate::config::NodeConfig;
use anyhow::Context;
use pm_library::{Library, Scanner};
use pm_model::{MusicUri, Track};
use std::path::Path;

fn open(cfg: &NodeConfig) -> anyhow::Result<Library> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let path = cfg.db_path();
    Library::open(&path).with_context(|| format!("opening library {}", path.display()))
}

/// Snapshot the library database to `file` (works while the node runs).
pub fn backup(cfg: &NodeConfig, file: &Path, force: bool) -> anyhow::Result<()> {
    if force && file.exists() {
        std::fs::remove_file(file)?;
    }
    let lib = open(cfg)?;
    lib.backup_to(file)
        .with_context(|| format!("writing backup {}", file.display()))?;
    let s = lib.stats()?;
    let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    println!(
        "backup written: {} ({} tracks, {} albums, {:.1} MB)",
        file.display(),
        s.tracks,
        s.albums,
        size as f64 / 1e6
    );
    Ok(())
}

/// Replace the library database with a backup. Stop the node first: the
/// previous database is kept as `library.db.bak`, and stale WAL files are
/// removed so SQLite cannot replay them into the restored copy.
pub fn restore(cfg: &NodeConfig, file: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let db = cfg.db_path();
    // Validate (and migrate) a scratch copy first so a broken file never
    // replaces a working library.
    let staged = db.with_extension("db.restoring");
    let _ = std::fs::remove_file(&staged);
    std::fs::copy(file, &staged).with_context(|| format!("reading {}", file.display()))?;
    let stats = {
        let lib =
            Library::open(&staged).with_context(|| format!("{} is not a library backup", file.display()))?;
        lib.stats()?
    };
    for suffix in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(db.with_extension(suffix));
    }
    if db.exists() {
        std::fs::rename(&db, db.with_extension("db.bak"))?;
    }
    std::fs::rename(&staged, &db)?;
    println!(
        "restored {} into {} ({} tracks, {} albums); the previous database is {}",
        file.display(),
        db.display(),
        stats.tracks,
        stats.albums,
        db.with_extension("db.bak").display()
    );
    Ok(())
}

/// Scan the given folders, or the configured ones when none are given.
pub fn scan(cfg: &NodeConfig, folders: &[std::path::PathBuf]) -> anyhow::Result<()> {
    let lib = open(cfg)?;
    let folders: Vec<&Path> = if folders.is_empty() {
        cfg.library.folders.iter().map(|p| p.as_path()).collect()
    } else {
        folders.iter().map(|p| p.as_path()).collect()
    };
    anyhow::ensure!(
        !folders.is_empty(),
        "no folders to scan: pass paths or set [library] folders"
    );
    let scanner = Scanner::new(&lib).with_art_dir(cfg.data_dir.join("art"));
    for root in folders {
        lib.add_folder(root)?;
        let r = scanner
            .scan(root)
            .with_context(|| format!("scanning {}", root.display()))?;
        println!(
            "{}: seen {} added {} updated {} unchanged {} removed {} errors {}",
            root.display(),
            r.files_seen,
            r.files_added,
            r.files_updated,
            r.files_unchanged,
            r.files_removed,
            r.errors.len()
        );
        for (p, e) in r.errors.iter().take(20) {
            println!("  ! {}: {e}", p.display());
        }
    }
    print_stats(&lib)
}

pub fn stats(cfg: &NodeConfig) -> anyhow::Result<()> {
    print_stats(&open(cfg)?)
}

fn print_stats(lib: &Library) -> anyhow::Result<()> {
    let s = lib.stats()?;
    println!(
        "library: {} tracks, {} albums, {} artists, {} files ({} missing), {:.1} h",
        s.tracks,
        s.albums,
        s.artists,
        s.files,
        s.missing_files,
        s.total_duration_ms as f64 / 3_600_000.0
    );
    Ok(())
}

pub fn tracks(cfg: &NodeConfig, limit: u32) -> anyhow::Result<()> {
    let lib = open(cfg)?;
    print_tracks(&lib.list_tracks(None, limit)?);
    Ok(())
}

pub fn search(cfg: &NodeConfig, q: &str, limit: u32) -> anyhow::Result<()> {
    let lib = open(cfg)?;
    print_tracks(&lib.search_tracks(q, limit)?);
    Ok(())
}

fn print_tracks(tracks: &[Track]) {
    for t in tracks {
        let artist = t
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let album = t.album.as_ref().map(|a| a.title.as_str()).unwrap_or("-");
        let canon = t
            .canonical
            .as_ref()
            .map(|(u, c)| format!("{} ({:.2})", u.to_url(), c.score()))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:>6}  {:<32} {:<24} {:<24} {}  {}",
            t.id.map(|i| i.0).unwrap_or(0),
            t.title.chars().take(32).collect::<String>(),
            artist.chars().take(24).collect::<String>(),
            album.chars().take(24).collect::<String>(),
            t.file.format,
            canon
        );
    }
}

/// Resolve a music URL to its canonical MusicBrainz form via the MusicBrainz API.
pub async fn resolve(cfg: &NodeConfig, input: &str) -> anyhow::Result<()> {
    let uri = MusicUri::parse(input).with_context(|| format!("parsing {input}"))?;
    if uri.is_canonical() {
        println!("already canonical: {}", uri.to_url());
        return Ok(());
    }
    let contact = cfg.pubky_config()?.user_agent_contact;
    let mb = pm_library::musicbrainz::MusicBrainzClient::new(
        pm_library::musicbrainz::MusicBrainzConfig::new(contact),
    )?;
    match mb.canonicalize(&uri).await? {
        Some(c) => println!("{} -> {}", uri.to_url(), c.to_url()),
        None => println!("{}: MusicBrainz has no link for this URL yet", uri.to_url()),
    }
    Ok(())
}

/// `pm-node import-playlist`: parse the file, resolve against the library and
/// store a private v1 playlist post (the same object the API and sync use).
pub fn import_playlist(cfg: &NodeConfig, file: &Path, name: Option<&str>) -> anyhow::Result<()> {
    use pm_library::import::{detect_format, name_from_file, parse};
    use pm_pubky::{MusicContent, MusicPost, Playlist, PlaylistItem};

    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let format = detect_format(file.to_str(), &bytes).context("cannot tell the playlist format")?;
    let list = parse(&bytes, format, file.parent())?;
    let lib = open(cfg)?;
    let mut items = Vec::new();
    let mut unresolved = Vec::new();
    for t in &list.tracks {
        match lib.resolve_import(t)? {
            Some(track) => match track.publishable_uri() {
                Some(u) => items.push(PlaylistItem::new(u)),
                None => unresolved.push(format!("{} (in library, no canonical id)", t.title)),
            },
            None => unresolved.push(match (&t.artist, &t.path) {
                (Some(a), _) => format!("{} — {a}", t.title),
                (None, Some(p)) => format!("{} ({p})", t.title),
                _ => t.title.clone(),
            }),
        }
    }
    let name = name
        .map(str::to_string)
        .or(list.name.clone())
        .unwrap_or_else(|| name_from_file(file));
    println!(
        "{}: {} entries, {} matched, {} unmatched",
        name,
        list.tracks.len(),
        items.len(),
        unresolved.len()
    );
    for u in &unresolved {
        println!("  unmatched: {u}");
    }
    anyhow::ensure!(
        !items.is_empty(),
        "nothing matched the library; scan the folders first"
    );
    let playlist = Playlist::new(name, None, None, items);
    let post = MusicPost::new(MusicContent::Playlist(playlist));
    let id = post.create_id();
    post.validate(Some(&id))?;
    let json = serde_json::to_string(&post.post)?;
    let (created_at, updated_at) = match &post.content {
        MusicContent::Playlist(p) => (p.created_at, p.updated_at),
        other => (other.created_micros(), other.created_micros()),
    };
    lib.upsert_music_post(&pm_library::MusicPostRow {
        id: id.clone(),
        kind: post.content.type_name().to_string(),
        visibility: "private".into(),
        json,
        created_at,
        updated_at,
        synced_at: None,
        deleted: false,
    })?;
    println!("stored playlist {id} (private; `pm-node serve` syncs it to the homeserver when signed in)");
    Ok(())
}
