//! Roon's library exports, for moving from Roon (handoff: "supports Roon migration").
//!
//! Roon exports what a browser shows (Albums, Tracks, a playlist) as an Excel workbook or a CSV
//! file: select all, then Export. The columns follow what that browser lists, so they are found
//! by name, in any order and case, with the spellings below; unknown columns are ignored. The
//! three kinds are told apart by their columns:
//!
//! - **Tracks:** a title column and library columns (album artist, tags, favourite, file path…).
//! - **Albums:** an album column and no title column.
//! - **Playlist:** a title column only with track artist, album and length (the sheet is named
//!   after the playlist).
//!
//! The column names come from Roon's documentation of its export and have to be checked against a
//! real export when one is available (`~/roon-export/`).

use crate::import::{parse_duration, split_record, ImportError};
use calamine::{DataType, Reader, Xlsx};
use std::io::Cursor;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoonKind {
    #[default]
    Tracks,
    Albums,
    Playlist,
}

/// One row of an export.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RoonRow {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub path: Option<String>,
    pub duration_ms: Option<u64>,
    pub year: Option<i32>,
    /// Roon's heart.
    pub favorite: bool,
    /// Roon's tags (the user's own labels).
    pub tags: Vec<String>,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoonExport {
    pub kind: RoonKind,
    /// The sheet name (a playlist's name), when it is not "Sheet1".
    pub name: Option<String>,
    pub rows: Vec<RoonRow>,
}

const TITLE: &[&str] = &["title", "track title", "track", "track name", "name", "song"];
const ARTIST: &[&str] = &[
    "track artist",
    "track artists",
    "artist",
    "artists",
    "performer",
    "performers",
];
const ALBUM: &[&str] = &["album", "album title", "album name"];
const ALBUM_ARTIST: &[&str] = &["album artist", "album artists"];
const PATH: &[&str] = &["file path", "path", "file", "location", "source path"];
const LENGTH: &[&str] = &["length", "duration", "time"];
const YEAR: &[&str] = &["year", "release year", "release date"];
const FAVORITE: &[&str] = &[
    "favorite",
    "favourite",
    "is favorite",
    "is favourite",
    "loved",
    "liked",
    "heart",
];
const TAGS: &[&str] = &["tags", "tag", "roon tags"];
const GENRES: &[&str] = &["genres", "genre"];
/// Columns only a library (not a playlist) export carries.
const LIBRARY_ONLY: &[&[&str]] = &[ALBUM_ARTIST, PATH, FAVORITE, TAGS, &["date added", "added"]];

struct Cols {
    title: Option<usize>,
    artist: Option<usize>,
    album: Option<usize>,
    album_artist: Option<usize>,
    path: Option<usize>,
    length: Option<usize>,
    year: Option<usize>,
    favorite: Option<usize>,
    tags: Option<usize>,
    genres: Option<usize>,
    library: bool,
}

fn columns(header: &[String]) -> Option<Cols> {
    let norm: Vec<String> = header.iter().map(|h| h.trim().to_lowercase()).collect();
    let find = |keys: &[&str]| keys.iter().find_map(|k| norm.iter().position(|h| h == k));
    let c = Cols {
        title: find(TITLE),
        artist: find(ARTIST),
        album: find(ALBUM),
        album_artist: find(ALBUM_ARTIST),
        path: find(PATH),
        length: find(LENGTH),
        year: find(YEAR),
        favorite: find(FAVORITE),
        tags: find(TAGS),
        genres: find(GENRES),
        library: LIBRARY_ONLY.iter().any(|keys| find(keys).is_some()),
    };
    (c.title.is_some() || c.album.is_some()).then_some(c)
}

fn truthy(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "yes" | "true" | "1" | "x" | "y" | "♥" | "❤" | "❤️" | "loved" | "favorite" | "favourite"
    )
}

fn list(s: &str) -> Vec<String> {
    s.split([';', ',', '|'])
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn row(cells: &[String], c: &Cols) -> Option<RoonRow> {
    let get = |i: Option<usize>| {
        i.and_then(|i| cells.get(i))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let r = RoonRow {
        title: get(c.title),
        artist: get(c.artist).or_else(|| {
            if c.title.is_none() {
                get(c.album_artist)
            } else {
                None
            }
        }),
        album: get(c.album),
        album_artist: get(c.album_artist),
        path: get(c.path),
        duration_ms: get(c.length).and_then(|d| parse_duration(&d, false)),
        year: get(c.year).and_then(|y| y.get(..4).and_then(|y| y.parse().ok())),
        favorite: get(c.favorite).is_some_and(|f| truthy(&f)),
        tags: get(c.tags).map(|t| list(&t)).unwrap_or_default(),
        genres: get(c.genres).map(|g| list(&g)).unwrap_or_default(),
    };
    (r.title.is_some() || r.album.is_some()).then_some(r)
}

/// A sheet's name (when it is not "Sheet1") and its rows as text.
type Table = (Option<String>, Vec<Vec<String>>);

/// The sheets of a workbook, or the one table of a CSV, as text rows.
fn tables(bytes: &[u8], file_name: Option<&str>) -> Result<Vec<Table>, ImportError> {
    let is_xlsx =
        bytes.starts_with(b"PK\x03\x04") || file_name.is_some_and(|n| n.to_lowercase().ends_with(".xlsx"));
    if is_xlsx {
        let mut wb = calamine::open_workbook_from_rs::<Xlsx<_>, _>(Cursor::new(bytes.to_vec()))
            .map_err(|e| ImportError::Xlsx(e.to_string()))?;
        return Ok(wb
            .worksheets()
            .into_iter()
            .map(|(sheet, range)| {
                let rows = range
                    .rows()
                    .map(|r| {
                        r.iter()
                            .map(|c| {
                                if c.is_empty() {
                                    String::new()
                                } else if let Some(s) = c.as_string() {
                                    s
                                } else {
                                    c.to_string()
                                }
                            })
                            .collect()
                    })
                    .collect();
                let name = (!sheet.to_lowercase().starts_with("sheet")).then_some(sheet);
                (name, rows)
            })
            .collect());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ImportError::NotText)?;
    let text = text.trim_start_matches('\u{feff}');
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let delim = lines
        .iter()
        .take(10)
        .flat_map(|l| [',', '\t', ';'].map(|d| (d, l.matches(d).count())))
        .max_by_key(|(_, n)| *n)
        .map(|(d, _)| d)
        .unwrap_or(',');
    Ok(vec![(
        None,
        lines.iter().map(|l| split_record(l, delim)).collect(),
    )])
}

/// Read a Roon export (Excel or CSV) and tell which kind it is.
pub fn parse_roon(bytes: &[u8], file_name: Option<&str>) -> Result<RoonExport, ImportError> {
    for (name, rows) in tables(bytes, file_name)? {
        // Roon writes a title row above the header: the header is the first row with known columns
        let Some(h) = rows.iter().take(10).position(|r| columns(r).is_some()) else {
            continue;
        };
        let c = columns(&rows[h]).expect("checked");
        let out: Vec<RoonRow> = rows[h + 1..].iter().filter_map(|r| row(r, &c)).collect();
        if out.is_empty() {
            continue;
        }
        let kind = match (c.title.is_some(), c.library) {
            (false, _) => RoonKind::Albums,
            (true, true) => RoonKind::Tracks,
            (true, false) => RoonKind::Playlist,
        };
        return Ok(RoonExport {
            kind,
            name,
            rows: out,
        });
    }
    Err(ImportError::NoTitleColumn(
        "the export (no title or album column)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_kinds_are_told_apart() {
        let tracks = "Roon Library Export\nTitle,Track Artist,Album,Album Artist,Length,Year,Genres,Tags,Favorite,Date Added,File Path\n\
            Bohemian Rhapsody,Queen,A Night at the Opera,Queen,5:55,1975,Rock;Progressive Rock,road trip; classics,Yes,2021-03-01,/music/Queen/11 Bohemian Rhapsody.flac\n\
            Love of My Life,Queen,A Night at the Opera,Queen,3:39,1975,Rock,,No,2021-03-01,\n";
        let e = parse_roon(tracks.as_bytes(), Some("Tracks.csv")).unwrap();
        assert_eq!(e.kind, RoonKind::Tracks);
        assert_eq!(e.rows.len(), 2);
        let r = &e.rows[0];
        assert_eq!(r.title.as_deref(), Some("Bohemian Rhapsody"));
        assert_eq!(
            (r.duration_ms, r.year, r.favorite),
            (Some(355_000), Some(1975), true)
        );
        assert_eq!(r.tags, vec!["road trip", "classics"]);
        assert_eq!(r.genres, vec!["Rock", "Progressive Rock"]);
        assert!(!e.rows[1].favorite && e.rows[1].path.is_none());

        let albums = "Album;Album Artist;Year;Tags;Favorite\nA Night at the Opera;Queen;1975;classics;x\nDiscovery;Daft Punk;2001;;\n";
        let e = parse_roon(albums.as_bytes(), Some("Albums.csv")).unwrap();
        assert_eq!(e.kind, RoonKind::Albums);
        assert_eq!(
            e.rows[0].artist.as_deref(),
            Some("Queen"),
            "the album artist names an album"
        );
        assert!(e.rows[0].favorite && !e.rows[1].favorite);

        let playlist = "Title,Track Artist,Album,Length\nGet Lucky,Daft Punk,Random Access Memories,6:09\n";
        assert_eq!(
            parse_roon(playlist.as_bytes(), Some("Road trip.csv"))
                .unwrap()
                .kind,
            RoonKind::Playlist
        );
        assert!(parse_roon(b"Foo,Bar\n1,2\n", Some("x.csv")).is_err());
    }
}
