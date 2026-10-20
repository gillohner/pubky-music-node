//! Cover art for albums whose files carry none: the Cover Art Archive
//! (coverartarchive.org) by release MBID, then release-group MBID. Fetched
//! lazily on the first cover request, stored in the art directory like
//! embedded art, and remembered as a miss for a week so a library of
//! untagged albums does not hammer the archive.

use crate::state::NodeState;
use pm_library::AlbumArt;
use pm_model::Mbid;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

const MISS_TTL_SECS: i64 = 7 * 24 * 3600;

fn client(s: &NodeState) -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let contact = s.contact();
    CLIENT
        .get_or_init(move || {
            let ua = if contact.is_empty() || contact == "unknown" {
                format!("pubky-music-node/{}", env!("CARGO_PKG_VERSION"))
            } else {
                format!("pubky-music-node/{} ({contact})", env!("CARGO_PKG_VERSION"))
            };
            reqwest::Client::builder()
                .user_agent(ua)
                .timeout(Duration::from_secs(20))
                .build()
                .expect("http client")
        })
        .clone()
}

fn in_flight() -> &'static Mutex<HashSet<i64>> {
    static SET: std::sync::OnceLock<Mutex<HashSet<i64>>> = std::sync::OnceLock::new();
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
    let (release, group, art_dir) = {
        let lib = s.library.lock().expect("library poisoned");
        let Some(album) = lib.get_album(album_id)? else {
            return Ok(None);
        };
        if album.release_mbid.is_none() && album.release_group_mbid.is_none() {
            return Ok(None);
        }
        let key = format!("art.caa.miss.{album_id}");
        if let Some(at) = lib.get_setting(&key)?.and_then(|v| v.parse::<i64>().ok()) {
            if pm_library::db::now_unix() - at < MISS_TTL_SECS {
                return Ok(None);
            }
        }
        (album.release_mbid, album.release_group_mbid, s.art_dir.clone())
    };
    if !in_flight().lock().expect("in-flight lock").insert(album_id) {
        return Ok(None);
    }
    let result = fetch(s, album_id, release, group, &art_dir).await;
    in_flight().lock().expect("in-flight lock").remove(&album_id);
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
        let resp = match http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(%url, error = %e, "cover art archive");
                continue;
            }
        };
        if !resp.status().is_success() {
            continue;
        }
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
            continue;
        }
        let bytes = match resp.bytes().await {
            Ok(b) if !b.is_empty() => b,
            _ => continue,
        };
        if tokio::fs::create_dir_all(art_dir).await.is_err() {
            return None;
        }
        let file = art_dir.join(format!("caa-{album_id}.{}", ext_for(&mime)));
        if tokio::fs::write(&file, &bytes).await.is_err() {
            return None;
        }
        return Some(AlbumArt {
            album_id,
            file,
            mime,
            source: "coverartarchive".into(),
        });
    }
    None
}
