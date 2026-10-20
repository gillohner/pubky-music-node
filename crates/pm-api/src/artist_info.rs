//! Artist pages get a portrait and a biography from open sources, keyed by
//! the artist's MusicBrainz id: MusicBrainz (URL relations) → Wikidata
//! (image P18, English Wikipedia sitelink) → Wikipedia REST summary. Fetched
//! lazily on the first request, cached in the library settings for 30 days
//! (misses for 7), the image stored in the art directory. Every origin is a
//! field on `NodeState` so tests point them at a local server.

use crate::state::NodeState;
use crate::types::ArtistInfo;
use pm_model::Mbid;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

const HIT_TTL_SECS: i64 = 30 * 24 * 3600;
const MISS_TTL_SECS: i64 = 7 * 24 * 3600;

/// Where the lookups go; tests swap these for a local server.
#[derive(Debug, Clone)]
pub struct InfoBases {
    pub musicbrainz: String,
    pub wikidata: String,
    pub wikipedia: String,
    pub commons: String,
}

impl Default for InfoBases {
    fn default() -> Self {
        Self {
            musicbrainz: "https://musicbrainz.org".into(),
            wikidata: "https://www.wikidata.org".into(),
            wikipedia: "https://en.wikipedia.org".into(),
            commons: "https://commons.wikimedia.org".into(),
        }
    }
}

fn client(s: &NodeState) -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let contact = s.pubky.user_agent_contact.clone();
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

#[derive(serde::Serialize, serde::Deserialize)]
struct Cached {
    at: i64,
    info: ArtistInfo,
}

/// The artist's info, from cache or freshly fetched. Never fails on network
/// trouble: it answers what it has and remembers a miss.
pub(crate) async fn artist_info(
    s: &NodeState,
    artist_id: i64,
) -> Result<ArtistInfo, pm_library::LibraryError> {
    let key = format!("artist.info.{artist_id}");
    let (mbid, name) = {
        let lib = s.library.lock().expect("library poisoned");
        let Some(a) = lib.get_artist(artist_id)? else {
            return Ok(ArtistInfo::default());
        };
        if let Some(c) = lib
            .get_setting(&key)?
            .and_then(|j| serde_json::from_str::<Cached>(&j).ok())
        {
            let age = pm_library::db::now_unix() - c.at;
            let ttl = if c.info.bio.is_some() || c.info.image {
                HIT_TTL_SECS
            } else {
                MISS_TTL_SECS
            };
            if age < ttl {
                return Ok(c.info);
            }
        }
        (a.mbid, a.name)
    };
    let Some(mbid) = mbid else {
        return Ok(ArtistInfo {
            note: Some("No MusicBrainz artist id in the tags yet.".into()),
            ..ArtistInfo::default()
        });
    };
    if !in_flight().lock().expect("in-flight lock").insert(artist_id) {
        return Ok(ArtistInfo {
            note: Some("Looking this artist up…".into()),
            ..ArtistInfo::default()
        });
    }
    let info = fetch(s, artist_id, mbid, &name).await;
    in_flight().lock().expect("in-flight lock").remove(&artist_id);
    let lib = s.library.lock().expect("library poisoned");
    let cached = Cached {
        at: pm_library::db::now_unix(),
        info: info.clone(),
    };
    lib.set_setting(&key, &serde_json::to_string(&cached).expect("info serializes"))?;
    Ok(info)
}

pub(crate) fn image_path(s: &NodeState, artist_id: i64) -> std::path::PathBuf {
    s.art_dir.join(format!("artist-{artist_id}.jpg"))
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<serde_json::Value> {
    let resp = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

async fn fetch(s: &NodeState, artist_id: i64, mbid: Mbid, name: &str) -> ArtistInfo {
    let bases = s.info_bases.read().expect("info bases").clone();
    let http = client(s);
    let mut info = ArtistInfo {
        musicbrainz: Some(format!("https://musicbrainz.org/artist/{mbid}")),
        ..ArtistInfo::default()
    };
    // 1. MusicBrainz: URL relations.
    let mb = get_json(
        &http,
        &format!(
            "{}/ws/2/artist/{mbid}?inc=url-rels&fmt=json",
            bases.musicbrainz.trim_end_matches('/')
        ),
    )
    .await;
    let mut wikidata_id: Option<String> = None;
    let mut wikipedia_title: Option<String> = None;
    if let Some(rels) = mb.as_ref().and_then(|v| v["relations"].as_array()) {
        for r in rels {
            let kind = r["type"].as_str().unwrap_or("");
            let url = r["url"]["resource"].as_str().unwrap_or("");
            match kind {
                "wikidata" => wikidata_id = url.rsplit('/').next().map(str::to_string),
                "official homepage" if info.homepage.is_none() => info.homepage = Some(url.to_string()),
                "wikipedia" if url.contains("en.wikipedia.org/wiki/") => {
                    wikipedia_title = url.rsplit('/').next().map(str::to_string)
                }
                _ => {}
            }
        }
    }
    // 2. Wikidata: portrait and the English article.
    let mut commons_file: Option<String> = None;
    if let Some(q) = &wikidata_id {
        info.wikidata = Some(format!("https://www.wikidata.org/wiki/{q}"));
        if let Some(v) = get_json(
            &http,
            &format!(
                "{}/wiki/Special:EntityData/{q}.json",
                bases.wikidata.trim_end_matches('/')
            ),
        )
        .await
        {
            let ent = &v["entities"][q.as_str()];
            commons_file = ent["claims"]["P18"][0]["mainsnak"]["datavalue"]["value"]
                .as_str()
                .map(str::to_string);
            if wikipedia_title.is_none() {
                wikipedia_title = ent["sitelinks"]["enwiki"]["title"].as_str().map(str::to_string);
            }
        }
    }
    // 3. Wikipedia summary.
    if let Some(title) = &wikipedia_title {
        let t = title.replace(' ', "_");
        if let Some(v) = get_json(
            &http,
            &format!(
                "{}/api/rest_v1/page/summary/{}",
                bases.wikipedia.trim_end_matches('/'),
                urlencode(&t)
            ),
        )
        .await
        {
            info.bio = v["extract"]
                .as_str()
                .filter(|e| !e.trim().is_empty())
                .map(str::to_string);
            info.wikipedia = v["content_urls"]["desktop"]["page"]
                .as_str()
                .map(str::to_string)
                .or_else(|| Some(format!("https://en.wikipedia.org/wiki/{t}")));
            if commons_file.is_none() {
                if let Some(src) = v["thumbnail"]["source"].as_str() {
                    info.image = download(&http, src, &image_path(s, artist_id)).await;
                }
            }
        }
    }
    // 4. The portrait from Commons.
    if let Some(file) = commons_file {
        let url = format!(
            "{}/wiki/Special:FilePath/{}?width=800",
            bases.commons.trim_end_matches('/'),
            urlencode(&file.replace(' ', "_"))
        );
        info.image = download(&http, &url, &image_path(s, artist_id)).await;
    }
    if info.bio.is_none() && !info.image {
        info.note = Some(format!("Nothing found for {name} on Wikidata or Wikipedia yet."));
    }
    info
}

async fn download(http: &reqwest::Client, url: &str, to: &std::path::Path) -> bool {
    let Ok(resp) = http.get(url).send().await else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    let is_image = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|m| m.starts_with("image/"));
    if !is_image {
        return false;
    }
    let Ok(bytes) = resp.bytes().await else {
        return false;
    };
    if bytes.is_empty()
        || tokio::fs::create_dir_all(to.parent().unwrap_or(to))
            .await
            .is_err()
    {
        return false;
    }
    tokio::fs::write(to, &bytes).await.is_ok()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'(' | b')' | b',' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
