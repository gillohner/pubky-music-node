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
    let scanner = Scanner::new(&lib);
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
    let contact = cfg.pubky_config().user_agent_contact;
    let mb = pm_library::musicbrainz::MusicBrainzClient::new(
        pm_library::musicbrainz::MusicBrainzConfig::new(contact),
    )?;
    match mb.canonicalize(&uri).await? {
        Some(c) => println!("{} -> {}", uri.to_url(), c.to_url()),
        None => println!("{}: MusicBrainz has no link for this URL yet", uri.to_url()),
    }
    Ok(())
}
