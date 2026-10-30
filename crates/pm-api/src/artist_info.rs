//! Artist pages get a portrait and a biography from open sources, keyed by
//! the artist's MusicBrainz id: MusicBrainz (URL relations) → Wikidata
//! (image P18, English Wikipedia sitelink) → Wikipedia REST summary, and when
//! those have no portrait or no text, fanart.tv (with the project's key) and
//! TheAudioDB (compare-music-assistant #16). Fetched
//! lazily on the first request, cached in the library settings for 30 days
//! (misses for 7), the image stored in the art directory. Every origin is a
//! field on `NodeState` so tests point them at a local server.

use crate::state::NodeState;
use crate::types::ArtistInfo;
use pm_model::Mbid;
use std::collections::HashSet;
use std::sync::Mutex;

const HIT_TTL_SECS: i64 = 30 * 24 * 3600;
const MISS_TTL_SECS: i64 = 7 * 24 * 3600;

/// Where the lookups go; tests swap these for a local server.
#[derive(Debug, Clone)]
pub struct InfoBases {
    pub musicbrainz: String,
    pub wikidata: String,
    pub wikipedia: String,
    pub commons: String,
    pub fanart: String,
    pub audiodb: String,
    /// The fanart.tv project key (built in; tests set one).
    pub fanart_key: Option<String>,
}

impl Default for InfoBases {
    fn default() -> Self {
        Self {
            musicbrainz: "https://musicbrainz.org".into(),
            wikidata: "https://www.wikidata.org".into(),
            wikipedia: "https://en.wikipedia.org".into(),
            commons: "https://commons.wikimedia.org".into(),
            fanart: "https://webservice.fanart.tv".into(),
            audiodb: "https://www.theaudiodb.com".into(),
            fanart_key: crate::project_apps::fanart_key().map(str::to_string),
        }
    }
}

fn client(s: &NodeState) -> reqwest::Client {
    s.http_client()
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
    // MusicBrainz: one request at a time with the rest of the node
    let mut turn = match url.contains("/ws/2/") {
        true => Some(pm_library::musicbrainz::take_turn(url, std::time::Duration::from_secs(1)).await),
        false => None,
    };
    let resp = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .ok()?;
    if matches!(resp.status().as_u16(), 503 | 429) {
        if let Some(t) = turn.as_mut() {
            t.back_off(pm_library::musicbrainz::retry_after(
                resp.headers(),
                std::time::Duration::from_secs(2),
            ));
        }
    }
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
                // Anyone can edit MusicBrainz: only a web link is kept
                "official homepage" if info.homepage.is_none() => info.homepage = crate::web::web_link(url),
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
    // 5. fanart.tv: a portrait, with the project's key.
    if !info.image {
        if let Some(key) = bases.fanart_key.as_deref() {
            if let Some(v) = get_json(
                &http,
                &format!(
                    "{}/v3/music/{mbid}?api_key={key}",
                    bases.fanart.trim_end_matches('/')
                ),
            )
            .await
            {
                if let Some(url) = fanart_thumb(&v) {
                    info.image = download(&http, &url, &image_path(s, artist_id)).await;
                    if info.image {
                        info.image_from = Some("fanart.tv".into());
                    }
                }
            }
        }
    }
    // 6. TheAudioDB: a portrait and a biography where the others had none.
    if !info.image || info.bio.is_none() {
        if let Some(v) = get_json(
            &http,
            &format!(
                "{}/api/v1/json/{}/artist-mb.php?i={mbid}",
                bases.audiodb.trim_end_matches('/'),
                crate::project_apps::audiodb_key()
            ),
        )
        .await
        {
            let a = &v["artists"][0];
            if !info.image {
                if let Some(url) = a["strArtistThumb"].as_str().filter(|u| !u.is_empty()) {
                    info.image = download(&http, url, &image_path(s, artist_id)).await;
                    if info.image {
                        info.image_from = Some("TheAudioDB".into());
                    }
                }
            }
            if info.bio.is_none() {
                info.bio = a["strBiographyEN"]
                    .as_str()
                    .or(a["strBiography"].as_str())
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(|b| b.chars().take(1200).collect::<String>());
                if info.bio.is_some() {
                    info.bio_from = Some("TheAudioDB".into());
                }
            }
        }
    }
    if info.bio.is_none() && !info.image {
        info.note = Some(format!(
            "Nothing found for {name} on Wikidata, Wikipedia, fanart.tv or TheAudioDB yet."
        ));
    }
    info
}

/// fanart.tv's best artist portrait: the most liked `artistthumb`, else a background.
pub(crate) fn fanart_thumb(v: &serde_json::Value) -> Option<String> {
    for kind in ["artistthumb", "artistbackground"] {
        let Some(list) = v[kind].as_array() else {
            continue;
        };
        let best = list
            .iter()
            .filter_map(|i| {
                let likes: u64 = i["likes"]
                    .as_str()
                    .and_then(|l| l.parse().ok())
                    .or_else(|| i["likes"].as_u64())?;
                Some((likes, i["url"].as_str()?.to_string()))
            })
            .max_by_key(|(likes, _)| *likes)
            .map(|(_, url)| url);
        if best.is_some() {
            return best;
        }
    }
    None
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
