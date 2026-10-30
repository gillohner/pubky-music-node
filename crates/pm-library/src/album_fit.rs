//! The folder as an album (identify ladder, step 2): a folder whose files say they are one album
//! by one artist is looked up as a MusicBrainz release, once for the whole folder, and each file
//! is matched to the release's track at its disc and track number when the length (±3 s) and the
//! title agree. A folder that fits a release well is identified in one search and one lookup;
//! what doesn't fit (a bonus track, a compilation folder, a file whose length is off) is left for
//! the fingerprint.
//!
//! Pure: the answers are MusicBrainz ws/2 JSON (`release` search, and a `release/{id}` lookup
//! with `inc=recordings+artist-credits`).

use crate::matching::{bare_title, lucene_escape, plain_name, plain_title, LENGTH_SLACK_MS};
use pm_model::Mbid;
use serde_json::Value;

/// One file of a folder, as its tags and the decoder describe it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FolderFile {
    pub track: i64,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub disc_no: Option<u32>,
    pub track_no: Option<u32>,
    pub duration_ms: Option<u64>,
    /// A release id from the tags (the album's, without a recording id).
    pub release: Option<Mbid>,
}

/// What a folder says it is: one album by one artist.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderAlbum {
    pub title: String,
    pub artist: String,
    /// A release the tags name: looked up at once, no search.
    pub release: Option<Mbid>,
}

/// The album a folder is, when its files agree: all name the same album, and one album artist
/// (or, without one, one track artist). A folder of songs from several albums, or of several
/// artists with no album artist (a mix, a "Singles" folder), is no album: its files go to the
/// fingerprint.
pub fn folder_album(files: &[FolderFile]) -> Option<FolderAlbum> {
    let first = files.first()?;
    let title = first.album.as_deref().map(str::trim).filter(|t| !t.is_empty())?;
    if files
        .iter()
        .any(|f| f.album.as_deref().map(plain_title) != Some(plain_title(title)))
    {
        return None;
    }
    let one = |name: &dyn Fn(&FolderFile) -> Option<&str>| -> Option<String> {
        let n = name(first).map(str::trim).filter(|n| !n.is_empty())?;
        files
            .iter()
            .all(|f| name(f).map(plain_name) == Some(plain_name(n)))
            .then(|| n.to_string())
    };
    let artist = one(&|f| f.album_artist.as_deref()).or_else(|| {
        // Without an album artist, the files' artist must be one and the same
        files
            .iter()
            .all(|f| f.album_artist.is_none())
            .then(|| one(&|f| Some(f.artist.as_str())))
            .flatten()
    })?;
    let release = files.iter().find_map(|f| f.release);
    let release = release.filter(|r| files.iter().all(|f| f.release.is_none_or(|x| x == *r)));
    Some(FolderAlbum {
        title: title.to_string(),
        artist,
        release,
    })
}

/// The release search for a folder: its album without the edition, by its artist.
pub fn release_query(a: &FolderAlbum) -> String {
    format!(
        "release:\"{}\" AND artist:\"{}\"",
        lucene_escape(&bare_title(&a.title)),
        lucene_escape(&a.artist)
    )
}

/// Releases from a search answer worth a lookup, best first: the album's title and artist, with
/// as many tracks as the folder has files first, then the nearest counts. At most `max`.
pub fn release_candidates(a: &FolderAlbum, files: usize, answer: &Value, max: usize) -> Vec<Mbid> {
    let title = plain_title(&a.title);
    let artist = plain_name(&a.artist);
    let mut found: Vec<(u64, i64, Mbid)> = Vec::new();
    for r in answer["releases"].as_array().into_iter().flatten() {
        let Some(id) = r["id"].as_str().and_then(|s| Mbid::parse(s).ok()) else {
            continue;
        };
        if plain_title(r["title"].as_str().unwrap_or_default()) != title {
            continue;
        }
        let credit: String = r["artist-credit"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                format!(
                    "{}{}",
                    c["name"].as_str().unwrap_or_default(),
                    c["joinphrase"].as_str().unwrap_or_default()
                )
            })
            .collect();
        let names_one = r["artist-credit"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| plain_name(c["name"].as_str().unwrap_or_default()) == artist);
        if plain_name(&credit) != artist && !names_one {
            continue;
        }
        let count = r["track-count"].as_u64().unwrap_or_else(|| {
            r["media"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["track-count"].as_u64())
                .sum()
        });
        let off = count.abs_diff(files as u64);
        found.push((off, -r["score"].as_i64().unwrap_or(0), id));
    }
    // Nearest track count, then MusicBrainz's score, then its order (a stable sort)
    found.sort_by_key(|(off, score, _)| (*off, *score));
    let mut out: Vec<Mbid> = Vec::new();
    for (_, _, id) in found {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out.truncate(max);
    out
}

/// One track of a release.
#[derive(Debug, Clone, PartialEq)]
struct ReleaseTrack {
    disc: u32,
    position: u32,
    title: String,
    length_ms: Option<u64>,
    recording: Mbid,
}

fn release_tracks(release: &Value) -> Vec<ReleaseTrack> {
    let mut out = Vec::new();
    for m in release["media"].as_array().into_iter().flatten() {
        let disc = m["position"].as_u64().unwrap_or(1).max(1) as u32;
        for t in m["tracks"].as_array().into_iter().flatten() {
            let Some(recording) = t["recording"]["id"].as_str().and_then(|s| Mbid::parse(s).ok()) else {
                continue;
            };
            let Some(position) = t["position"].as_u64() else {
                continue;
            };
            out.push(ReleaseTrack {
                disc,
                position: position as u32,
                title: t["title"]
                    .as_str()
                    .or(t["recording"]["title"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                length_ms: t["length"].as_u64().or(t["recording"]["length"].as_u64()),
                recording,
            });
        }
    }
    out
}

/// Whether a file's title and a track's name the same song: equal once plain, or one is the
/// other with a few words more ("Song" and "Song (Live)" are plain-equal already). An empty
/// title on the file says nothing either way.
pub fn titles_agree(file: &str, track: &str) -> bool {
    let (a, b) = (plain_title(file), plain_title(track));
    if a.is_empty() {
        return true;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    !short.is_empty() && long.starts_with(short.as_str()) && short.len() * 10 >= long.len() * 6
}

fn lengths_agree(file: Option<u64>, track: Option<u64>) -> bool {
    match (file, track) {
        (Some(a), Some(b)) => a.abs_diff(b) <= LENGTH_SLACK_MS,
        _ => false,
    }
}

/// A file matched to a release's track.
#[derive(Debug, Clone, PartialEq)]
pub struct Matched {
    pub track: i64,
    pub recording: Mbid,
    /// The track's title on the release.
    pub title: String,
}

/// How a folder fits a release.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumFit {
    pub release: Mbid,
    pub release_title: Option<String>,
    pub matched: Vec<Matched>,
    /// How well the folder and the release cover each other (0–1): matched files over the
    /// larger of the folder and the release's discs the folder holds.
    pub fit: f32,
}

impl AlbumFit {
    /// The confidence a match from this fit carries: 0.95 for a folder that is the release, less
    /// as files are missing or extra; never above a fingerprint's sure answer.
    pub fn score(&self) -> f32 {
        0.75 + 0.2 * self.fit
    }
}

/// A fit below this leaves the whole folder to the fingerprint.
pub const MIN_FIT: f32 = 0.6;

/// Match a folder's files to a release (`inc=recordings`): a file matches the track at its disc
/// and number (disc 1 without a disc tag) when the length is within ±3 s and the title agrees;
/// a file without a number matches the one track whose title and length agree. `None` when
/// fewer than [`MIN_FIT`] of the folder (or of the release's discs it holds) match, or when the
/// same track would match two files.
pub fn fit_release(files: &[FolderFile], release: &Value) -> Option<AlbumFit> {
    let id = release["id"].as_str().and_then(|s| Mbid::parse(s).ok())?;
    let tracks = release_tracks(release);
    if tracks.is_empty() || files.is_empty() {
        return None;
    }
    let mut matched: Vec<Matched> = Vec::new();
    let mut used: Vec<(u32, u32)> = Vec::new();
    let mut discs: Vec<u32> = Vec::new();
    for f in files {
        let disc = f.disc_no.unwrap_or(1).max(1);
        let hit = match f.track_no {
            Some(n) => tracks.iter().find(|t| t.disc == disc && t.position == n),
            None => {
                let agree: Vec<&ReleaseTrack> = tracks
                    .iter()
                    .filter(|t| !f.title.trim().is_empty() && plain_title(&t.title) == plain_title(&f.title))
                    .filter(|t| lengths_agree(f.duration_ms, t.length_ms))
                    .collect();
                (agree.len() == 1).then(|| agree[0])
            }
        };
        let Some(t) =
            hit.filter(|t| lengths_agree(f.duration_ms, t.length_ms) && titles_agree(&f.title, &t.title))
        else {
            continue;
        };
        if used.contains(&(t.disc, t.position)) {
            // Two files claim one track: a folder with duplicates is not this release
            return None;
        }
        used.push((t.disc, t.position));
        if !discs.contains(&t.disc) {
            discs.push(t.disc);
        }
        matched.push(Matched {
            track: f.track,
            recording: t.recording,
            title: t.title.clone(),
        });
    }
    let on_discs = tracks.iter().filter(|t| discs.contains(&t.disc)).count();
    let fit = matched.len() as f32 / files.len().max(on_discs).max(1) as f32;
    (fit >= MIN_FIT).then(|| AlbumFit {
        release: id,
        release_title: release["title"].as_str().map(str::to_string),
        matched,
        fit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REL: &str = "1a5b6a49-4c4f-4d18-9e3d-1c4d1b0a7a3e";
    const REL2: &str = "2b6c7b5a-5d5a-4e29-8f4e-2d5e2c1b8b4f";

    fn rec(i: u32) -> String {
        format!("00000000-0000-4000-8000-{i:012}")
    }

    /// A release of `n` tracks "Song i", each 200 + i seconds.
    fn release(n: u32) -> Value {
        json!({
            "id": REL,
            "title": "Album",
            "media": [{"position": 1, "track-count": n, "tracks": (1..=n).map(|i| json!({
                "position": i, "number": i.to_string(), "title": format!("Song {i}"),
                "length": (200 + i as u64) * 1000, "recording": {"id": rec(i)}
            })).collect::<Vec<_>>()}]
        })
    }

    fn file(i: u32) -> FolderFile {
        FolderFile {
            track: i as i64,
            title: format!("Song {i}"),
            artist: "Band".into(),
            album: Some("Album (Remastered)".into()),
            album_artist: Some("Band".into()),
            disc_no: None,
            track_no: Some(i),
            // Ripped a second longer than MusicBrainz says
            duration_ms: Some((201 + i as u64) * 1000),
            release: None,
        }
    }

    #[test]
    fn a_folder_that_is_the_album_fits_it_whole() {
        let files: Vec<FolderFile> = (1..=10).map(file).collect();
        let a = folder_album(&files).unwrap();
        assert_eq!(a.artist, "Band");
        assert_eq!(release_query(&a), "release:\"Album\" AND artist:\"Band\"");
        let fit = fit_release(&files, &release(10)).unwrap();
        assert_eq!(fit.matched.len(), 10);
        assert_eq!(
            (fit.matched[3].track, fit.matched[3].recording),
            (4, Mbid::parse(&rec(4)).unwrap())
        );
        assert_eq!(fit.matched[3].title, "Song 4");
        assert_eq!(fit.fit, 1.0);
        assert!((fit.score() - 0.95).abs() < 1e-6);
    }

    #[test]
    fn a_folder_missing_a_track_still_fits() {
        let files: Vec<FolderFile> = (1..=12).filter(|i| *i != 7).map(file).collect();
        let fit = fit_release(&files, &release(12)).unwrap();
        assert_eq!(fit.matched.len(), 11);
        assert!(fit.score() > 0.9 && fit.score() < 0.95, "{}", fit.score());
    }

    #[test]
    fn a_bonus_track_is_left_for_the_fingerprint() {
        let mut files: Vec<FolderFile> = (1..=12).map(file).collect();
        let mut bonus = file(13);
        bonus.title = "Demo".into();
        files.push(bonus);
        let fit = fit_release(&files, &release(12)).unwrap();
        assert_eq!(fit.matched.len(), 12);
        assert!(!fit.matched.iter().any(|m| m.track == 13));
    }

    #[test]
    fn a_compilation_folder_of_mixed_artists_is_no_album() {
        // A local mix as the scanner stores it ("Various Artists"): searched for as one, but a
        // release of that name has other songs, so each goes on to the fingerprint
        let mix: Vec<FolderFile> = (1..=6)
            .map(|i| FolderFile {
                title: format!("Hit {i}"),
                artist: format!("Artist {i}"),
                album_artist: Some("Various Artists".into()),
                album: Some("Summer Mix".into()),
                duration_ms: Some(180_000),
                ..file(i)
            })
            .collect();
        let a = folder_album(&mix).unwrap();
        assert_eq!(
            release_query(&a),
            "release:\"Summer Mix\" AND artist:\"Various Artists\""
        );
        assert_eq!(fit_release(&mix, &release(6)), None);
        // A compilation MusicBrainz has (its songs, its lengths) fits like any album
        let various: Vec<FolderFile> = (1..=6)
            .map(|i| FolderFile {
                artist: format!("Artist {i}"),
                album_artist: Some("Various Artists".into()),
                ..file(i)
            })
            .collect();
        assert_eq!(fit_release(&various, &release(6)).unwrap().matched.len(), 6);
        // No album artist (a remote share, whose folders the scanner doesn't read), every song
        // by someone else: a mix, not an album
        let files: Vec<FolderFile> = (1..=6)
            .map(|i| FolderFile {
                artist: format!("Artist {i}"),
                album_artist: None,
                album: Some("Summer Mix".into()),
                ..file(i)
            })
            .collect();
        assert_eq!(folder_album(&files), None);
        // Songs from several albums in one folder: no album either
        let mut files: Vec<FolderFile> = (1..=4).map(file).collect();
        files[2].album = Some("Other Album".into());
        assert_eq!(folder_album(&files), None);
        // Without an album tag
        let mut files: Vec<FolderFile> = (1..=4).map(file).collect();
        files.iter_mut().for_each(|f| f.album = None);
        assert_eq!(folder_album(&files), None);
        // One artist without an album artist is an album
        let files: Vec<FolderFile> = (1..=4)
            .map(|i| FolderFile {
                album_artist: None,
                ..file(i)
            })
            .collect();
        assert_eq!(folder_album(&files).unwrap().artist, "Band");
    }

    #[test]
    fn wrong_lengths_titles_or_a_few_songs_of_a_big_release_do_not_fit() {
        // Another edition: every length is off by more than 3 s
        let mut files: Vec<FolderFile> = (1..=10).map(file).collect();
        files
            .iter_mut()
            .for_each(|f| f.duration_ms = f.duration_ms.map(|d| d + 5000));
        assert_eq!(fit_release(&files, &release(10)), None);
        // Other songs at those positions
        let files: Vec<FolderFile> = (1..=10)
            .map(|i| FolderFile {
                title: format!("Other {i}"),
                ..file(i)
            })
            .collect();
        assert_eq!(fit_release(&files, &release(10)), None);
        // Three songs of an 18-song best-of
        let files: Vec<FolderFile> = (1..=3).map(file).collect();
        assert_eq!(fit_release(&files, &release(18)), None);
        // Two files at one position
        let mut files: Vec<FolderFile> = (1..=4).map(file).collect();
        files.push(FolderFile { track: 99, ..file(2) });
        assert_eq!(fit_release(&files, &release(4)), None);
    }

    #[test]
    fn a_disc_folder_fits_its_disc_and_files_without_numbers_fit_by_title() {
        // Disc 2 of a two-disc release, in a folder of its own
        let mut rel = release(5);
        let mut d2 = rel["media"][0].clone();
        d2["position"] = json!(2);
        for (i, t) in d2["tracks"].as_array_mut().unwrap().iter_mut().enumerate() {
            t["recording"]["id"] = json!(rec(100 + i as u32));
        }
        rel["media"].as_array_mut().unwrap().push(d2);
        let files: Vec<FolderFile> = (1..=5)
            .map(|i| FolderFile {
                disc_no: Some(2),
                ..file(i)
            })
            .collect();
        let fit = fit_release(&files, &rel).unwrap();
        assert_eq!(fit.fit, 1.0);
        assert_eq!(fit.matched[0].recording, Mbid::parse(&rec(100)).unwrap());
        // No track numbers: the title and length decide
        let files: Vec<FolderFile> = (1..=5)
            .map(|i| FolderFile {
                track_no: None,
                ..file(i)
            })
            .collect();
        assert_eq!(fit_release(&files, &release(5)).unwrap().matched.len(), 5);
    }

    #[test]
    fn candidates_are_the_albums_releases_nearest_in_track_count_first() {
        let a = FolderAlbum {
            title: "Album (Deluxe)".into(),
            artist: "The Band".into(),
            release: None,
        };
        let answer = json!({"releases": [
            {"id": REL2, "score": 100, "title": "Album", "track-count": 14, "artist-credit": [{"name": "Band"}]},
            {"id": REL, "score": 90, "title": "Album", "media": [{"track-count": 6}, {"track-count": 6}], "artist-credit": [{"name": "Band", "joinphrase": " & "}, {"name": "Friends"}]},
            {"id": rec(1), "score": 100, "title": "Album", "track-count": 12, "artist-credit": [{"name": "Someone Else"}]},
            {"id": rec(2), "score": 100, "title": "Album Two", "track-count": 12, "artist-credit": [{"name": "Band"}]}
        ]});
        assert_eq!(
            release_candidates(&a, 12, &answer, 3),
            vec![Mbid::parse(REL).unwrap(), Mbid::parse(REL2).unwrap()]
        );
    }
}
