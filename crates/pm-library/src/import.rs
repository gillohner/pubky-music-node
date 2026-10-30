//! Playlist files from other players, for migration: M3U/M3U8 (most
//! players), PLS (Winamp, many radio players), CSV (Exportify, Soundiiz, TuneMyMusic, Roon's Excel export saved
//! as CSV) and Roon's own Excel export (`.xlsx`, Roon → Export → Excel).
//!
//! Parsing is deliberately tolerant: columns are found by header name, in any
//! order and case; unknown columns are ignored. What comes out is a list of
//! described tracks; [`crate::Library::resolve_import`] matches them to the
//! library by path, file name, then title and artist.

use calamine::{DataType, Reader, Xlsx};
use std::io::Cursor;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ImportedTrack {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// A file path or URL the playlist pointed at, as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportedList {
    /// From `#PLAYLIST:` or the sheet name; callers fall back to the file name.
    pub name: Option<String>,
    pub tracks: Vec<ImportedTrack>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    M3u,
    Pls,
    Csv,
    Xlsx,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("cannot tell the playlist format; name the file .m3u, .m3u8, .pls, .csv or .xlsx")]
    UnknownFormat,
    #[error("the file is not valid UTF-8 text")]
    NotText,
    #[error("no column that looks like a track title in {0}")]
    NoTitleColumn(&'static str),
    #[error("spreadsheet: {0}")]
    Xlsx(String),
}

const TITLE_KEYS: &[&str] = &[
    "title",
    "track name",
    "track title",
    "name",
    "track",
    "song",
    "song name",
];
const ARTIST_KEYS: &[&str] = &[
    "artist",
    "artists",
    "artist name",
    "artist name(s)",
    "artist(s)",
    "track artist",
    "performer",
    "album artist",
];
const ALBUM_KEYS: &[&str] = &["album", "album name", "album title", "release"];
const PATH_KEYS: &[&str] = &[
    "path",
    "file",
    "location",
    "filename",
    "file name",
    "file path",
    "url",
    "uri",
];
const DURATION_KEYS: &[&str] = &["duration", "duration (ms)", "duration_ms", "length", "time"];

/// Decide the format from the file name, else from the bytes.
pub fn detect_format(file_name: Option<&str>, bytes: &[u8]) -> Option<ImportFormat> {
    if let Some(ext) = file_name
        .and_then(|n| Path::new(n).extension())
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
    {
        match ext.as_str() {
            "m3u" | "m3u8" => return Some(ImportFormat::M3u),
            "pls" => return Some(ImportFormat::Pls),
            "csv" | "tsv" | "txt" => return Some(ImportFormat::Csv),
            "xlsx" | "xlsm" => return Some(ImportFormat::Xlsx),
            _ => {}
        }
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return Some(ImportFormat::Xlsx);
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let first = text.trim_start_matches('\u{feff}').lines().next()?.trim();
    if first.eq_ignore_ascii_case("#EXTM3U") {
        return Some(ImportFormat::M3u);
    }
    if first.eq_ignore_ascii_case("[playlist]") {
        return Some(ImportFormat::Pls);
    }
    let lower = first.to_ascii_lowercase();
    if (lower.contains(',') || lower.contains('\t') || lower.contains(';'))
        && TITLE_KEYS.iter().any(|k| lower.contains(k))
    {
        return Some(ImportFormat::Csv);
    }
    None
}

/// Parse `bytes` as `format`; relative M3U and PLS entries resolve against `base`.
pub fn parse(bytes: &[u8], format: ImportFormat, base: Option<&Path>) -> Result<ImportedList, ImportError> {
    match format {
        ImportFormat::M3u => {
            let text = std::str::from_utf8(bytes).map_err(|_| ImportError::NotText)?;
            Ok(parse_m3u(text, base))
        }
        ImportFormat::Pls => {
            let text = std::str::from_utf8(bytes).map_err(|_| ImportError::NotText)?;
            Ok(parse_pls(text, base))
        }
        ImportFormat::Csv => {
            let text = std::str::from_utf8(bytes).map_err(|_| ImportError::NotText)?;
            parse_csv(text)
        }
        ImportFormat::Xlsx => parse_xlsx(bytes),
    }
}

/// An entry's address without the parts that can carry a sign-in (a media server's stream link
/// names its token there): a URL loses its user and password, its query and its fragment; a file
/// path comes back as it is. Every address passes it before it becomes a title or search words.
pub fn without_secrets(address: &str) -> std::borrow::Cow<'_, str> {
    let Some((scheme, rest)) = address.split_once("://") else {
        return address.into();
    };
    let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{scheme}://{host}{path}").into()
}

/// A title from an entry's address: its file name without the extension and track number. A
/// URL's words come from its decoded path only ([`without_secrets`]), never its host or sign-in;
/// empty
/// when it has no path.
pub fn title_from_path(p: &str) -> String {
    let clean = match without_secrets(p).split_once("://") {
        Some((_, rest)) => rest
            .find('/')
            .map_or(String::new(), |i| percent_decode(&rest[i..])),
        None => p.to_string(),
    };
    let p = clean.as_str();
    let stem = Path::new(p)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
        .replace('_', " ");
    // "03 - Title" / "03. Title" / "03 Title"
    let trimmed = stem.trim_start_matches(|c: char| c.is_ascii_digit());
    let trimmed = trimmed.trim_start_matches([' ', '-', '.', '_']);
    if trimmed.is_empty() {
        stem
    } else {
        trimmed.to_string()
    }
}

fn resolve_entry(entry: &str, base: Option<&Path>) -> String {
    if let Some(rest) = entry.strip_prefix("file://") {
        return percent_decode(rest.trim_start_matches("localhost"));
    }
    if entry.contains("://") {
        return entry.to_string();
    }
    let p = Path::new(entry);
    match base {
        Some(b) if p.is_relative() => b.join(p).to_string_lossy().into_owned(),
        _ => entry.to_string(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `#EXTM3U` with `#EXTINF:<secs>,<Artist> - <Title>` lines; plain path lists work too.
pub fn parse_m3u(text: &str, base: Option<&Path>) -> ImportedList {
    let mut list = ImportedList::default();
    let mut pending: Option<(Option<u64>, Option<String>, Option<String>)> = None;
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#PLAYLIST:") {
            list.name = Some(rest.trim().to_string()).filter(|s| !s.is_empty());
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let (dur, rest) = rest.split_once(',').unwrap_or((rest, ""));
            let secs = dur
                .split_whitespace()
                .next()
                .and_then(|d| d.parse::<f64>().ok())
                .filter(|d| *d > 0.0);
            let (artist, title) = match rest.split_once(" - ") {
                Some((a, t)) => (Some(a.trim().to_string()), Some(t.trim().to_string())),
                None => (None, Some(rest.trim().to_string()).filter(|s| !s.is_empty())),
            };
            pending = Some((secs.map(|s| (s * 1000.0) as u64), artist, title));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let path = resolve_entry(line, base);
        let (duration_ms, artist, title) = pending.take().unwrap_or((None, None, None));
        list.tracks.push(ImportedTrack {
            title: title.unwrap_or_else(|| title_from_path(&path)),
            artist,
            album: None,
            path: Some(path),
            duration_ms,
        });
    }
    list
}

/// `[playlist]` with `FileN=`, `TitleN=` and `LengthN=` (seconds, `-1` for a stream) lines,
/// in any order; entries come out in N order. Titles written "Artist - Title" are split like
/// M3U's `#EXTINF`.
pub fn parse_pls(text: &str, base: Option<&Path>) -> ImportedList {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Entry {
        file: Option<String>,
        title: Option<String>,
        secs: Option<f64>,
    }
    let mut entries: BTreeMap<u32, Entry> = BTreeMap::new();
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let Some((key, value)) = raw.trim().split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        let Some(i) = key.find(|c: char| c.is_ascii_digit()) else {
            continue;
        };
        let Ok(n) = key[i..].parse::<u32>() else {
            continue;
        };
        let e = entries.entry(n).or_default();
        match &key[..i] {
            "file" if !value.is_empty() => e.file = Some(value.to_string()),
            "title" if !value.is_empty() => e.title = Some(value.to_string()),
            "length" => e.secs = value.parse::<f64>().ok().filter(|d| *d > 0.0),
            _ => {}
        }
    }
    let mut list = ImportedList::default();
    for e in entries.into_values() {
        let Some(file) = e.file else { continue };
        let path = resolve_entry(&file, base);
        let (artist, title) = match e.title.as_deref().and_then(|t| t.split_once(" - ")) {
            Some((a, t)) => (Some(a.trim().to_string()), Some(t.trim().to_string())),
            None => (None, e.title),
        };
        list.tracks.push(ImportedTrack {
            title: title.unwrap_or_else(|| title_from_path(&path)),
            artist,
            album: None,
            path: Some(path),
            duration_ms: e.secs.map(|s| (s * 1000.0) as u64),
        });
    }
    list
}

/// Split one CSV record; handles quotes and doubled quotes.
pub(crate) fn split_record(line: &str, delim: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == delim && !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.iter().map(|s| s.trim().to_string()).collect()
}

struct Columns {
    title: usize,
    artist: Option<usize>,
    album: Option<usize>,
    path: Option<usize>,
    duration: Option<(usize, bool)>,
}

fn find_columns(header: &[String]) -> Option<Columns> {
    let norm: Vec<String> = header.iter().map(|h| h.trim().to_lowercase()).collect();
    let find = |keys: &[&str]| keys.iter().find_map(|k| norm.iter().position(|h| h == k));
    let title = find(TITLE_KEYS)?;
    let duration = find(DURATION_KEYS).map(|i| (i, norm[i].contains("ms")));
    Some(Columns {
        title,
        artist: find(ARTIST_KEYS),
        album: find(ALBUM_KEYS),
        path: find(PATH_KEYS),
        duration,
    })
}

pub(crate) fn parse_duration(s: &str, is_ms: bool) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((m, sec)) = s.rsplit_once(':') {
        let (h, m) = match m.rsplit_once(':') {
            Some((h, m)) => (h.parse::<u64>().ok()?, m.parse::<u64>().ok()?),
            None => (0, m.parse::<u64>().ok()?),
        };
        let sec = sec.parse::<f64>().ok()?;
        return Some(((h * 3600 + m * 60) as f64 * 1000.0 + sec * 1000.0) as u64);
    }
    let v = s.parse::<f64>().ok()?;
    Some(if is_ms || v > 20_000.0 {
        v as u64
    } else {
        (v * 1000.0) as u64
    })
}

fn row_to_track(cells: &[String], cols: &Columns) -> Option<ImportedTrack> {
    let get = |i: Option<usize>| {
        i.and_then(|i| cells.get(i))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    };
    let title = get(Some(cols.title));
    let path = get(cols.path);
    let title = match (title, path) {
        (Some(t), _) => t.to_string(),
        (None, Some(p)) => title_from_path(p),
        (None, None) => return None,
    };
    Some(ImportedTrack {
        title,
        artist: get(cols.artist).map(str::to_string),
        album: get(cols.album).map(str::to_string),
        path: path.map(str::to_string),
        duration_ms: cols
            .duration
            .and_then(|(i, ms)| get(Some(i)).and_then(|d| parse_duration(d, ms))),
    })
}

/// Header-tolerant CSV/TSV.
pub fn parse_csv(text: &str) -> Result<ImportedList, ImportError> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header_line = lines.next().ok_or(ImportError::NoTitleColumn("the CSV"))?;
    let delim = [',', '\t', ';']
        .into_iter()
        .max_by_key(|d| header_line.matches(*d).count())
        .unwrap_or(',');
    let header = split_record(header_line, delim);
    let cols = find_columns(&header).ok_or(ImportError::NoTitleColumn("the CSV header"))?;
    let tracks = lines
        .filter_map(|l| row_to_track(&split_record(l, delim), &cols))
        .collect();
    Ok(ImportedList { name: None, tracks })
}

/// Roon's Excel export (and any sheet with a header row within its first rows).
pub fn parse_xlsx(bytes: &[u8]) -> Result<ImportedList, ImportError> {
    let mut wb = calamine::open_workbook_from_rs::<Xlsx<_>, _>(Cursor::new(bytes.to_vec()))
        .map_err(|e| ImportError::Xlsx(e.to_string()))?;
    let mut list = ImportedList::default();
    for (sheet, range) in wb.worksheets() {
        let rows: Vec<Vec<String>> = range
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
        let Some(h) = rows.iter().take(10).position(|r| find_columns(r).is_some()) else {
            continue;
        };
        let cols = find_columns(&rows[h]).expect("checked");
        for r in &rows[h + 1..] {
            if let Some(t) = row_to_track(r, &cols) {
                list.tracks.push(t);
            }
        }
        if list.name.is_none() && !sheet.to_lowercase().starts_with("sheet") {
            list.name = Some(sheet.clone());
        }
        if !list.tracks.is_empty() {
            break;
        }
    }
    if list.tracks.is_empty() {
        return Err(ImportError::NoTitleColumn("any sheet of the workbook"));
    }
    Ok(list)
}

/// A playlist name from a file name: `My Mix.m3u8` → `My Mix`.
pub fn name_from_file(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Imported playlist")
        .to_string()
}

/// Build a [`PathBuf`] for an entry when it looks like a local path.
pub fn local_path(entry: &str) -> Option<PathBuf> {
    if entry.contains("://") {
        return None;
    }
    Some(PathBuf::from(entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn pls_entries_in_order_with_titles_and_lengths() {
        let text = "[playlist]\r\nNumberOfEntries=3\r\nFile2=music/b.flac\r\nTitle2=Sefa - Gold\r\n\
                    Length2=241\r\nFile1=/abs/a.flac\r\nFile3=http://radio.example/stream\r\n\
                    Title3=A station\r\nLength3=-1\r\nVersion=2\r\n";
        assert_eq!(detect_format(None, text.as_bytes()), Some(ImportFormat::Pls));
        assert_eq!(detect_format(Some("x.PLS"), b""), Some(ImportFormat::Pls));
        let l = parse(text.as_bytes(), ImportFormat::Pls, Some(Path::new("/lists"))).unwrap();
        let got: Vec<_> = l
            .tracks
            .iter()
            .map(|t| {
                (
                    t.path.as_deref().unwrap(),
                    t.artist.as_deref(),
                    t.title.as_str(),
                    t.duration_ms,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("/abs/a.flac", None, "a", None),
                ("/lists/music/b.flac", Some("Sefa"), "Gold", Some(241_000)),
                ("http://radio.example/stream", None, "A station", None),
            ]
        );
    }

    #[test]
    fn m3u_with_extinf_and_relative_paths() {
        let text = "#EXTM3U\n#PLAYLIST:Road trip\n#EXTINF:369,Daft Punk - Get Lucky\nDaft Punk/RAM/08 Get Lucky.flac\n#EXTINF:-1,Untitled\n/abs/02_Track.flac\nfile:///m/03%20Three.flac\nhttps://example.com/stream.mp3\n";
        let l = parse_m3u(text, Some(Path::new("/music")));
        assert_eq!(l.name.as_deref(), Some("Road trip"));
        assert_eq!(l.tracks.len(), 4);
        assert_eq!(l.tracks[0].title, "Get Lucky");
        assert_eq!(l.tracks[0].artist.as_deref(), Some("Daft Punk"));
        assert_eq!(
            l.tracks[0].path.as_deref(),
            Some("/music/Daft Punk/RAM/08 Get Lucky.flac")
        );
        assert_eq!(l.tracks[0].duration_ms, Some(369_000));
        assert_eq!(l.tracks[1].title, "Untitled");
        assert_eq!(l.tracks[1].duration_ms, None);
        assert_eq!(l.tracks[2].title, "Three");
        assert_eq!(l.tracks[2].path.as_deref(), Some("/m/03 Three.flac"));
        assert_eq!(
            l.tracks[3].path.as_deref(),
            Some("https://example.com/stream.mp3")
        );
        assert_eq!(detect_format(None, text.as_bytes()), Some(ImportFormat::M3u));
        assert_eq!(detect_format(Some("x.M3U8"), b""), Some(ImportFormat::M3u));
    }

    #[test]
    fn csv_exportify_and_roon_shapes() {
        let exportify = "\u{feff}Track URI,Track Name,Artist Name(s),Album Name,Duration (ms)\nspotify:track:1,\"Get Lucky\",\"Daft Punk, Pharrell Williams\",Random Access Memories,369000\n";
        let l = parse_csv(exportify).unwrap();
        assert_eq!(l.tracks.len(), 1);
        assert_eq!(l.tracks[0].title, "Get Lucky");
        assert_eq!(
            l.tracks[0].artist.as_deref(),
            Some("Daft Punk, Pharrell Williams")
        );
        assert_eq!(l.tracks[0].album.as_deref(), Some("Random Access Memories"));
        assert_eq!(l.tracks[0].duration_ms, Some(369_000));
        let roon = "Title;Artist;Album;Length\nGet Lucky;Daft Punk;Random Access Memories;6:09\nInstant Crush;Daft Punk;Random Access Memories;5:37\n";
        let l = parse_csv(roon).unwrap();
        assert_eq!(l.tracks.len(), 2);
        assert_eq!(l.tracks[0].duration_ms, Some(369_000));
        assert_eq!(l.tracks[1].title, "Instant Crush");
        assert_eq!(detect_format(None, roon.as_bytes()), Some(ImportFormat::Csv));
        assert!(matches!(
            parse_csv("a,b\n1,2\n"),
            Err(ImportError::NoTitleColumn(_))
        ));
    }

    fn tiny_xlsx(sheet: &str, rows: &[&[&str]]) -> Vec<u8> {
        let mut cells = String::new();
        for (ri, r) in rows.iter().enumerate() {
            cells.push_str(&format!("<row r=\"{}\">", ri + 1));
            for (ci, v) in r.iter().enumerate() {
                let col = (b'A' + ci as u8) as char;
                cells.push_str(&format!(
                    "<c r=\"{col}{}\" t=\"inlineStr\"><is><t>{v}</t></is></c>",
                    ri + 1
                ));
            }
            cells.push_str("</row>");
        }
        let files = [
            ("[Content_Types].xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>{}"#, "")),
            ("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string()),
            ("xl/workbook.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="{sheet}" sheetId="1" r:id="rId1"/></sheets></workbook>"#)),
            ("xl/_rels/workbook.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.to_string()),
            ("xl/worksheets/sheet1.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{cells}</sheetData></worksheet>"#)),
        ];
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, body) in files {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn xlsx_roon_export() {
        // Roon writes a title row above the header; the header is found by name.
        let bytes = tiny_xlsx(
            "Favourites",
            &[
                &["Exported from Roon"],
                &["Title", "Artist", "Album", "Length", "Format"],
                &[
                    "Get Lucky",
                    "Daft Punk",
                    "Random Access Memories",
                    "6:09",
                    "FLAC 44.1kHz 24bit",
                ],
                &[
                    "Instant Crush",
                    "Daft Punk",
                    "Random Access Memories",
                    "5:37",
                    "FLAC 44.1kHz 24bit",
                ],
            ],
        );
        assert_eq!(detect_format(None, &bytes), Some(ImportFormat::Xlsx));
        let l = parse(&bytes, ImportFormat::Xlsx, None).unwrap();
        assert_eq!(l.name.as_deref(), Some("Favourites"));
        assert_eq!(l.tracks.len(), 2);
        assert_eq!(l.tracks[0].title, "Get Lucky");
        assert_eq!(l.tracks[0].artist.as_deref(), Some("Daft Punk"));
        assert_eq!(l.tracks[0].duration_ms, Some(369_000));
        assert_eq!(l.tracks[1].title, "Instant Crush");
        let none = tiny_xlsx("Sheet1", &[&["a", "b"], &["1", "2"]]);
        assert!(parse_xlsx(&none).is_err());
    }

    #[test]
    fn titles_from_paths() {
        assert_eq!(title_from_path("/m/08 - Get Lucky.flac"), "Get Lucky");
        assert_eq!(title_from_path("03.Instant_Crush.dsf"), "Instant Crush");
        assert_eq!(title_from_path("2001.flac"), "2001");
        // A stream's sign-in never becomes its title (review of #156)
        assert_eq!(
            title_from_path("http://jf.example.com/Audio/x/universal?UserId=u&api_key=SECRET"),
            "universal"
        );
        assert_eq!(title_from_path("http://admin:hunter2@radio.example.com:8000"), "");
        assert_eq!(
            title_from_path("https://u:p@cdn.example.com/03%20Song.mp3#token=SECRET"),
            "Song"
        );
    }

    #[test]
    fn addresses_lose_their_sign_in() {
        for (raw, clean) in [
            (
                "https://user:pass@host.example.com/a.mp3",
                "https://host.example.com/a.mp3",
            ),
            ("https://token@host.example.com", "https://host.example.com"),
            (
                "http://nas:8096/stream.mp3?token=SECRET",
                "http://nas:8096/stream.mp3",
            ),
            (
                "https://host.example.com/a#access_token=SECRET",
                "https://host.example.com/a",
            ),
            (
                "https://a:b@host.example.com/x?t=1#f",
                "https://host.example.com/x",
            ),
            ("/music/a@b?.flac", "/music/a@b?.flac"),
            ("Just words", "Just words"),
        ] {
            assert_eq!(without_secrets(raw), clean, "{raw}");
        }
    }
}
