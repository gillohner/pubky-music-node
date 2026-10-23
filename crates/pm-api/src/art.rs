//! Cover art for albums whose files carry none: the Cover Art Archive
//! (coverartarchive.org) by release MBID, then release-group MBID, then Apple's
//! iTunes Search API (official, keyless) by album title and album artist, taking
//! a result only when both match. Fetched lazily on the first cover request,
//! stored in the art directory like embedded art, and remembered as a miss for a
//! week so a library of untagged albums does not hammer either service. The
//! iTunes fallback follows Music Assistant's `itunes_artwork` provider
//! (Apache-2.0; see NOTICE).

use crate::state::NodeState;
use pm_library::AlbumArt;
use pm_model::Mbid;
use std::collections::HashSet;
use std::sync::{Mutex, RwLock};

const MISS_TTL_SECS: i64 = 7 * 24 * 3600;

fn itunes_base() -> &'static RwLock<String> {
    static B: std::sync::OnceLock<RwLock<String>> = std::sync::OnceLock::new();
    B.get_or_init(|| RwLock::new("https://itunes.apple.com".into()))
}

/// Point the iTunes lookups somewhere else (tests).
pub fn set_itunes_base(url: &str) {
    *itunes_base().write().expect("itunes base") = url.trim_end_matches('/').to_string();
}

fn client(s: &NodeState) -> reqwest::Client {
    s.http_client()
}

/// Albums whose cover is being fetched, by art directory and album (one node has one art
/// directory; tests run several nodes in one process).
fn in_flight() -> &'static Mutex<HashSet<(std::path::PathBuf, i64)>> {
    static SET: std::sync::OnceLock<Mutex<HashSet<(std::path::PathBuf, i64)>>> = std::sync::OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

fn ext_for(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim() {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

/// Try the archive for `album_id`; `Ok(None)` when the album has no MBIDs,
/// the archive has nothing, a recent miss is remembered, or another fetch
/// for the same album is running.
pub(crate) async fn fetch_missing_cover(
    s: &NodeState,
    album_id: i64,
) -> Result<Option<AlbumArt>, pm_library::LibraryError> {
    let (album, art_dir) = {
        let lib = s.library.lock().expect("library poisoned");
        let Some(album) = lib.get_album(album_id)? else {
            return Ok(None);
        };
        let key = format!("art.caa.miss.{album_id}");
        if let Some(at) = lib.get_setting(&key)?.and_then(|v| v.parse::<i64>().ok()) {
            if pm_library::db::now_unix() - at < MISS_TTL_SECS {
                return Ok(None);
            }
        }
        (album, s.art_dir.clone())
    };
    let flight = (art_dir.clone(), album_id);
    if !in_flight().lock().expect("in-flight lock").insert(flight.clone()) {
        return Ok(None);
    }
    let mut result = fetch(
        s,
        album_id,
        album.release_mbid,
        album.release_group_mbid,
        &art_dir,
    )
    .await;
    if result.is_none() {
        if let Some(artist) = album.album_artist.as_deref() {
            result = from_itunes(s, album_id, &album.title, artist, &art_dir).await;
        }
    }
    in_flight().lock().expect("in-flight lock").remove(&flight);
    let lib = s.library.lock().expect("library poisoned");
    match result {
        Some(art) => {
            lib.set_album_art(&art)?;
            Ok(Some(art))
        }
        None => {
            lib.set_setting(
                &format!("art.caa.miss.{album_id}"),
                &pm_library::db::now_unix().to_string(),
            )?;
            Ok(None)
        }
    }
}

async fn fetch(
    s: &NodeState,
    album_id: i64,
    release: Option<Mbid>,
    group: Option<Mbid>,
    art_dir: &std::path::Path,
) -> Option<AlbumArt> {
    let base = s
        .caa_base
        .read()
        .expect("caa base")
        .trim_end_matches('/')
        .to_string();
    let mut urls = Vec::new();
    if let Some(m) = release {
        urls.push(format!("{base}/release/{m}/front-500"));
    }
    if let Some(m) = group {
        urls.push(format!("{base}/release-group/{m}/front-500"));
    }
    let http = client(s);
    for url in urls {
        if let Some(art) = download(&http, &url, album_id, "caa", "coverartarchive", art_dir).await {
            return Some(art);
        }
    }
    None
}

/// Lower case letters and digits only, with bracketed parts ("(Remastered)", "[Deluxe]") left
/// out, so "Discovery (Remastered 2011)" and "Discovery" compare equal.
fn loose(s: &str) -> String {
    let mut depth = 0u32;
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            c if depth == 0 && c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => {}
        }
    }
    out
}

/// Apple's iTunes Search API: the first album whose title and artist match ours, at 600 px.
/// Albums by "Various Artists" are skipped: a title alone picks wrong covers too often.
async fn from_itunes(
    s: &NodeState,
    album_id: i64,
    title: &str,
    artist: &str,
    art_dir: &std::path::Path,
) -> Option<AlbumArt> {
    let (want_title, want_artist) = (loose(title), loose(artist));
    if want_title.is_empty() || want_artist.is_empty() || artist.eq_ignore_ascii_case("Various Artists") {
        return None;
    }
    let base = itunes_base().read().expect("itunes base").clone();
    let http = client(s);
    let resp = http
        .get(format!("{base}/search"))
        .query(&[
            ("term", format!("{artist} {title}").as_str()),
            ("media", "music"),
            ("entity", "album"),
            ("limit", "10"),
        ])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), "itunes search");
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let art = body["results"].as_array()?.iter().find_map(|r| {
        let ok = loose(r["collectionName"].as_str()?) == want_title
            && loose(r["artistName"].as_str()?) == want_artist;
        ok.then(|| {
            r["artworkUrl100"]
                .as_str()
                .map(|u| u.replace("100x100bb", "600x600bb"))
        })
        .flatten()
    })?;
    download(&http, &art, album_id, "itunes", "itunes", art_dir).await
}

/// Fetch one image into the art directory as `<prefix>-<album>.<ext>`.
async fn download(
    http: &reqwest::Client,
    url: &str,
    album_id: i64,
    prefix: &str,
    source: &str,
    art_dir: &std::path::Path,
) -> Option<AlbumArt> {
    let resp = match http.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(%url, error = %e, "cover download");
            return None;
        }
    };
    if !resp.status().is_success() {
        return None;
    }
    {
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .split(';')
            .next()
            .unwrap_or("image/jpeg")
            .trim()
            .to_string();
        if !mime.starts_with("image/") {
            return None;
        }
        let bytes = match resp.bytes().await {
            Ok(b) if !b.is_empty() => b,
            _ => return None,
        };
        if tokio::fs::create_dir_all(art_dir).await.is_err() {
            return None;
        }
        let file = art_dir.join(format!("{prefix}-{album_id}.{}", ext_for(&mime)));
        if tokio::fs::write(&file, &bytes).await.is_err() {
            return None;
        }
        Some(AlbumArt {
            album_id,
            file,
            mime,
            source: source.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::loose;

    #[test]
    fn loose_titles() {
        assert_eq!(loose("Discovery (Remastered 2011)"), loose("discovery"));
        assert_eq!(loose("Random Access Memories [Deluxe]"), "randomaccessmemories");
        assert_eq!(loose("AC/DC"), "acdc");
        assert_ne!(loose("Homework"), loose("Discovery"));
    }
}
