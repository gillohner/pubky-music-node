//! The first streaming source that needs no account: the Internet Archive's
//! audio collections (Live Music Archive, netlabels, public domain 78s).
//! Search goes through `advancedsearch.php`, an item's tracks come from the
//! `/metadata/<id>` document, and playback is plain HTTP streaming of the
//! item's files (FLAC when present, else MP3/Ogg) through the node's stream
//! source, so every zone and the browser can play them. Items are not
//! MusicBrainz entities, so they cannot be hearted or tagged (constraint 4).

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::ApiError;
use axum::http::StatusCode;
use pm_zones::QueueItem;

pub const SOURCE_ID: &str = "archive";

fn base(s: &NodeState) -> String {
    s.archive_base
        .read()
        .expect("archive base")
        .trim_end_matches('/')
        .to_string()
}

/// The Internet Archive as a [`StreamingService`].
pub struct InternetArchive;

#[async_trait::async_trait]
impl StreamingService for InternetArchive {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn info(&self, _s: &NodeState) -> SourceInfo {
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Internet Archive".into(),
            description: "Live Music Archive, netlabels and public-domain recordings; no account needed"
                .into(),
            status: "ready".into(),
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let query = format!("({}) AND mediatype:(audio)", lucene(term));
        let url = format!("{}/advancedsearch.php", base(s));
        let resp = s
            .http_client()
            .get(&url)
            .query(&[
                ("q", query.as_str()),
                ("fl[]", "identifier"),
                ("fl[]", "title"),
                ("fl[]", "creator"),
                ("fl[]", "year"),
                ("fl[]", "downloads"),
                ("sort[]", "downloads desc"),
                ("rows", &limit.to_string()),
                ("page", "1"),
                ("output", "json"),
            ])
            .send()
            .await
            .map_err(upstream)?;
        if !resp.status().is_success() {
            return Err(upstream(format!("search answered {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(upstream)?;
        Ok(parse_search(&v))
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        if id.trim().is_empty() || id.contains('/') {
            return Err(ApiError::bad_request("bad item id"));
        }
        let url = format!("{}/metadata/{}", base(s), id);
        let resp = s.http_client().get(&url).send().await.map_err(upstream)?;
        if !resp.status().is_success() {
            return Err(upstream(format!("metadata answered {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(upstream)?;
        parse_item(id, &v, &base(s)).ok_or_else(|| ApiError::not_found(format!("no audio in item {id}")))
    }
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("internet archive: {e}"))
}

/// Escape a user term for the archive's Lucene query.
fn lucene(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 8);
    for c in term.chars() {
        if "+-&|!(){}[]^\"~*?:\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn str_or_first(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => a.iter().find_map(|x| x.as_str().map(str::to_string)),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(crate) fn parse_search(v: &serde_json::Value) -> Vec<SourceHit> {
    v["response"]["docs"]
        .as_array()
        .map(|docs| {
            docs.iter()
                .filter_map(|d| {
                    let id = d["identifier"].as_str()?.to_string();
                    Some(SourceHit {
                        url: format!("https://archive.org/details/{id}"),
                        image: Some(format!("https://archive.org/services/img/{id}")),
                        kind: "item".into(),
                        duration_ms: None,
                        library_artist_id: None,
                        isrc: None,
                        upc: None,
                        library_track_id: None,
                        library_album_id: None,
                        uri: None,
                        title: str_or_first(&d["title"]).unwrap_or_else(|| id.clone()),
                        creator: str_or_first(&d["creator"]),
                        year: str_or_first(&d["year"])
                            .and_then(|y| y.get(..4).and_then(|p| p.parse::<i32>().ok())),
                        downloads: d["downloads"].as_u64(),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Rank of a file format for playback: lossless first, then the better lossy.
fn format_rank(format: &str) -> Option<u8> {
    let f = format.to_ascii_lowercase();
    Some(match f.as_str() {
        "flac" | "24bit flac" => 0,
        "apple lossless audio" => 1,
        "vbr mp3" => 2,
        "ogg vorbis" => 3,
        "mp3" | "128kbps mp3" | "64kbps mp3" => 4,
        _ => return None,
    })
}

fn parse_duration_secs(s: &str) -> Option<f64> {
    if let Ok(v) = s.trim().parse::<f64>() {
        return Some(v);
    }
    let parts: Vec<&str> = s.trim().split(':').collect();
    let mut secs = 0.0;
    for p in &parts {
        secs = secs * 60.0 + p.parse::<f64>().ok()?;
    }
    Some(secs)
}

pub(crate) fn parse_item(id: &str, v: &serde_json::Value, base: &str) -> Option<SourceItem> {
    let meta = &v["metadata"];
    let files = v["files"].as_array()?;
    // One entry per "original" (or per name stem): keep the best format.
    let mut best: std::collections::BTreeMap<String, (u8, &serde_json::Value)> = Default::default();
    for f in files {
        let Some(name) = f["name"].as_str() else { continue };
        let Some(rank) = f["format"].as_str().and_then(format_rank) else {
            continue;
        };
        let key = match f["original"].as_str() {
            Some(o) if f["source"].as_str() == Some("derivative") => o.to_string(),
            _ => name.to_string(),
        };
        let key = key
            .rsplit_once('.')
            .map(|(stem, _)| stem.to_string())
            .unwrap_or(key);
        match best.get(&key) {
            Some((r, _)) if *r <= rank => {}
            _ => {
                best.insert(key, (rank, f));
            }
        }
    }
    if best.is_empty() {
        return None;
    }
    let mut tracks: Vec<SourceTrack> = best
        .into_values()
        .map(|(_, f)| {
            let name = f["name"].as_str().unwrap_or("").to_string();
            let title = str_or_first(&f["title"]).unwrap_or_else(|| {
                name.rsplit_once('.')
                    .map(|(s, _)| s)
                    .unwrap_or(&name)
                    .replace('_', " ")
            });
            SourceTrack {
                title,
                track_no: str_or_first(&f["track"])
                    .and_then(|t| t.split('/').next()?.trim().parse::<u32>().ok()),
                duration_ms: str_or_first(&f["length"])
                    .and_then(|l| parse_duration_secs(&l))
                    .map(|s| (s * 1000.0) as u64),
                format: f["format"].as_str().unwrap_or("").to_string(),
                url: format!("{base}/download/{id}/{}", urlencode_path(&name)),
                artist: str_or_first(&f["creator"]).or_else(|| str_or_first(&meta["creator"])),
            }
        })
        .collect();
    tracks.sort_by(|a, b| {
        a.track_no
            .unwrap_or(u32::MAX)
            .cmp(&b.track_no.unwrap_or(u32::MAX))
            .then_with(|| a.title.cmp(&b.title))
    });
    Some(SourceItem {
        source: SOURCE_ID.into(),
        id: id.to_string(),
        image: Some(format!("https://archive.org/services/img/{id}")),
        title: str_or_first(&meta["title"]).unwrap_or_else(|| id.to_string()),
        creator: str_or_first(&meta["creator"]),
        year: str_or_first(&meta["year"])
            .or_else(|| str_or_first(&meta["date"]))
            .and_then(|y| y.get(..4).and_then(|p| p.parse::<i32>().ok())),
        description: str_or_first(&meta["description"]).map(|d| strip_tags(&d)),
        licence: str_or_first(&meta["licenseurl"]),
        url: format!("https://archive.org/details/{id}"),
        tracks,
        library_album_id: None,
    })
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn urlencode_path(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/'
            | b'('
            | b')'
            | b',' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl SourceTrack {
    /// The queue item that streams this track.
    pub fn queue_item(&self, item: &SourceItem) -> QueueItem {
        let mut q = QueueItem::new(self.url.clone());
        q.title = Some(self.title.clone());
        q.artist = self.artist.clone().or_else(|| item.creator.clone());
        q.album = Some(item.title.clone());
        q.duration_ms = self.duration_ms;
        q.image = item.image.clone();
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_and_item_documents_parse() {
        let search = serde_json::json!({"response": {"docs": [
            {"identifier": "gd1977-05-08.sbd", "title": "Grateful Dead Live at Barton Hall", "creator": "Grateful Dead", "year": "1977", "downloads": 123456},
            {"identifier": "noisy", "title": ["Noise"], "creator": ["A", "B"]}
        ]}});
        let hits = parse_search(&search);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].year, Some(1977));
        assert_eq!(hits[1].creator.as_deref(), Some("A"));
        let item = serde_json::json!({
            "metadata": {"title": "Barton Hall", "creator": "Grateful Dead", "date": "1977-05-08", "description": "<b>Soundboard</b> recording", "licenseurl": "http://creativecommons.org/licenses/by-nc-sa/3.0/"},
            "files": [
                {"name": "gd77-05-08d1t01.flac", "format": "Flac", "source": "original", "title": "New Minglewood Blues", "track": "01", "length": "283.5"},
                {"name": "gd77-05-08d1t01.mp3", "format": "VBR MP3", "source": "derivative", "original": "gd77-05-08d1t01.flac", "title": "New Minglewood Blues", "track": "01", "length": "04:43"},
                {"name": "gd77-05-08d1t02.mp3", "format": "VBR MP3", "source": "derivative", "original": "gd77-05-08d1t02.shn", "title": "Loser", "track": "02", "length": "07:40"},
                {"name": "gd77-05-08d1t02.shn", "format": "Shorten", "source": "original"},
                {"name": "cover.jpg", "format": "JPEG"}
            ]
        });
        let it = parse_item("gd1977-05-08.sbd", &item, "https://archive.org").unwrap();
        assert_eq!(it.year, Some(1977));
        assert_eq!(it.description.as_deref(), Some("Soundboard recording"));
        assert_eq!(it.tracks.len(), 2, "one playable file per recording, best format");
        assert_eq!(it.tracks[0].format, "Flac");
        assert_eq!(
            it.tracks[0].url,
            "https://archive.org/download/gd1977-05-08.sbd/gd77-05-08d1t01.flac"
        );
        assert_eq!(it.tracks[0].duration_ms, Some(283_500));
        assert_eq!(it.tracks[1].title, "Loser");
        assert_eq!(it.tracks[1].format, "VBR MP3");
        assert_eq!(it.tracks[1].duration_ms, Some(460_000));
        let q = it.tracks[1].queue_item(&it);
        assert_eq!(q.album.as_deref(), Some("Barton Hall"));
        assert_eq!(q.artist.as_deref(), Some("Grateful Dead"));
        assert_eq!(lucene("a+b (c)"), "a\\+b \\(c\\)");
        assert!(parse_item(
            "x",
            &serde_json::json!({"metadata": {}, "files": [{"name": "a.txt", "format": "Text"}]}),
            "b"
        )
        .is_none());
    }
}
