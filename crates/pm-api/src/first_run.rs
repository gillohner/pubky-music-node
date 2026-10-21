//! A new install's first steps (steward next-3, item 2): where the music probably is, so the
//! owner picks a folder instead of typing a path they have to look up.
//!
//! `GET /v1/library/folders/suggest` looks in the usual places (the home Music folder, `/music`
//! as the container image mounts it, drives under `/media`, `/mnt` and `/Volumes`) and lists
//! the ones holding audio files the node can play, with a count. It reads at most a few thousand
//! directory entries per place, so it answers quickly on a big disk.

use crate::state::NodeState;
use crate::ApiResult;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A folder that holds music.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSuggestion {
    pub path: String,
    /// Audio files found (counting stops at 1,000).
    pub audio_files: usize,
    /// More than 1,000, or the look was cut short: "at least".
    pub more: bool,
}

/// Counting stops here.
const COUNT_MAX: usize = 1000;
/// Directory entries read per place at most.
const ENTRIES_MAX: usize = 20_000;
/// How deep a place is searched.
const DEPTH_MAX: usize = 5;

/// The places to look, most likely first. `home` is the user's home folder, `user` their login.
pub fn places(home: Option<&Path>, user: Option<&str>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = home {
        if let Some(xdg) = xdg_music_dir(h) {
            out.push(xdg);
        }
        out.extend(["Music", "music", "Musik", "Musique", "Música"].map(|d| h.join(d)));
    }
    out.extend(
        [
            "/music",
            "/srv/music",
            "/mnt/music",
            "/data/music",
            "/media/music",
        ]
        .map(PathBuf::from),
    );
    // Drives: /media/<user>/<drive>, /media/<drive>, /mnt/<drive>, /Volumes/<drive>
    let mut drives = Vec::new();
    if let Some(u) = user {
        drives.push(PathBuf::from("/media").join(u));
        drives.push(PathBuf::from("/run/media").join(u));
    }
    drives.extend(["/media", "/mnt", "/Volumes"].map(PathBuf::from));
    for d in drives {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut found: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        found.sort();
        for drive in found {
            out.push(drive.join("Music"));
            out.push(drive.join("music"));
            out.push(drive);
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// `XDG_MUSIC_DIR` from `~/.config/user-dirs.dirs` (a desktop's own name for Music).
fn xdg_music_dir(home: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).ok()?;
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("XDG_MUSIC_DIR="))?;
    let value = line.split_once('=')?.1.trim().trim_matches('"');
    let path = value.replace("$HOME", &home.to_string_lossy());
    (path != home.to_string_lossy()).then(|| PathBuf::from(path))
}

/// Audio files under `dir`, breadth first, within the limits. `None` when it holds none.
pub fn count_audio(dir: &Path) -> Option<(usize, bool)> {
    let mut queue = std::collections::VecDeque::from([(dir.to_path_buf(), 0usize)]);
    let (mut files, mut entries) = (0usize, 0usize);
    while let Some((d, depth)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            entries += 1;
            if entries > ENTRIES_MAX || files >= COUNT_MAX {
                return (files > 0).then_some((files, true));
            }
            let p = e.path();
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                let hidden = p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'));
                if depth < DEPTH_MAX && !hidden {
                    queue.push_back((p, depth + 1));
                }
            } else if pm_library::tags::is_supported(&p) {
                files += 1;
            }
        }
    }
    (files > 0).then_some((files, false))
}

/// Places with music, the folders already in the library left out, and a folder inside one
/// already suggested left out too (a drive and its Music folder are one suggestion).
pub fn suggest(places: &[PathBuf], known: &[PathBuf]) -> Vec<FolderSuggestion> {
    let mut out: Vec<FolderSuggestion> = Vec::new();
    for p in places {
        let Ok(p) = p.canonicalize() else { continue };
        if !p.is_dir()
            || known.iter().any(|k| p.starts_with(k) || k.starts_with(&p))
            || out
                .iter()
                .any(|s| p.starts_with(&s.path) || Path::new(&s.path).starts_with(&p))
        {
            continue;
        }
        if let Some((audio_files, more)) = count_audio(&p) {
            out.push(FolderSuggestion {
                path: p.to_string_lossy().into_owned(),
                audio_files,
                more,
            });
        }
        if out.len() >= 6 {
            break;
        }
    }
    out
}

/// `GET /v1/library/folders/suggest`.
pub(crate) async fn suggest_folders(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<FolderSuggestion>> {
    let known: Vec<PathBuf> = s
        .library
        .lock()
        .expect("library poisoned")
        .list_folders()?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).ok();
    let found =
        tokio::task::spawn_blocking(move || suggest(&places(home.as_deref(), user.as_deref()), &known))
            .await
            .unwrap_or_default();
    Ok(Json(found))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_folders_are_found_and_counted() {
        let home = tempfile::tempdir().unwrap();
        let music = home.path().join("Music");
        std::fs::create_dir_all(music.join("Artist/Album")).unwrap();
        for i in 0..3 {
            std::fs::write(music.join(format!("Artist/Album/{i:02}.flac")), b"x").unwrap();
        }
        std::fs::write(music.join("cover.jpg"), b"x").unwrap();
        let empty = home.path().join("music-empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(home.path().join(".config")).unwrap();
        std::fs::write(
            home.path().join(".config/user-dirs.dirs"),
            "XDG_MUSIC_DIR=\"$HOME/Music\"\n",
        )
        .unwrap();

        let ps = places(Some(home.path()), Some("nobody"));
        assert_eq!(ps[0], music, "the desktop's Music folder first");
        let found = suggest(&[music.clone(), empty.clone(), music.join("Artist")], &[]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].audio_files, 3);
        assert!(!found[0].more);
        // A folder already in the library is not suggested again
        assert!(suggest(std::slice::from_ref(&music), &[music.canonicalize().unwrap()]).is_empty());
    }
}
