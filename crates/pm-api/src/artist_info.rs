//! Artist pages get a portrait and a biography from open sources, keyed by
//! the artist's MusicBrainz id (from the tags, else `library_match`): MusicBrainz (URL
//! relations: Wikidata and "Also on") → Wikidata (image P18 with its Commons credit, facts, the
//! Wikipedia sitelink in the reader's language, else English) → Wikipedia REST summary (see
//! `about`), and when those have no portrait or no text, fanart.tv (with the project's key) and
//! TheAudioDB (compare-music-assistant #16). Fetched lazily on the first request, cached in the
//! library settings per language for 30 days (misses a day), the image stored in the art
//! directory. Every origin is a field on `NodeState` so tests point them at a local server.

use crate::about::{self, HIT_TTL_SECS, MISS_TTL_SECS};
use crate::state::NodeState;
use crate::types::{AboutText, ArtistInfo};
use pm_model::Mbid;
use std::collections::HashSet;
use std::sync::Mutex;

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
    /// Between two Wikimedia requests (200 a minute); tests set none.
    pub wikimedia_interval: std::time::Duration,
}

impl InfoBases {
    /// The Wikipedia in `lang` (`{lang}` in the base is replaced; a test's base is used as is).
    pub fn wikipedia_for(&self, lang: &str) -> String {
        self.wikipedia.replace("{lang}", lang)
    }

    /// A request to Wikidata, Wikipedia or Commons, paced as Wikimedia asks.
    pub fn is_wikimedia(&self, url: &str) -> bool {
        let wikipedia = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.ends_with(".wikipedia.org")))
            .unwrap_or(false);
        [self.wikidata.as_str(), self.commons.as_str()]
            .iter()
            .any(|b| url.starts_with(b))
            || wikipedia
            || (!self.wikipedia.contains("{lang}") && url.starts_with(&self.wikipedia))
    }
}

impl Default for InfoBases {
    fn default() -> Self {
        Self {
            musicbrainz: "https://musicbrainz.org".into(),
            wikidata: "https://www.wikidata.org".into(),
            wikipedia: "https://{lang}.wikipedia.org".into(),
            commons: "https://commons.wikimedia.org".into(),
            fanart: "https://webservice.fanart.tv".into(),
            audiodb: "https://www.theaudiodb.com".into(),
            fanart_key: crate::project_apps::fanart_key().map(str::to_string),
            wikimedia_interval: std::time::Duration::from_millis(300),
        }
    }
}

fn client(s: &NodeState) -> reqwest::Client {
    s.http_client()
}

fn in_flight() -> &'static Mutex<HashSet<(i64, String)>> {
    static SET: std::sync::OnceLock<Mutex<HashSet<(i64, String)>>> = std::sync::OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Cached {
    at: i64,
    info: ArtistInfo,
}

/// The artist's info in `lang` (a bare language code, see `about::language`), from cache or
/// freshly fetched. Never fails on network trouble: it answers what it has and remembers a
/// miss.
pub(crate) async fn artist_info(
    s: &NodeState,
    artist_id: i64,
    lang: &str,
) -> Result<ArtistInfo, pm_library::LibraryError> {
    let key = format!("artist.info.{artist_id}.{lang}");
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
            let ttl = if c.info.bio.is_some() || c.info.image || !c.info.links.is_empty() {
                HIT_TTL_SECS
            } else {
                MISS_TTL_SECS
            };
            if age < ttl {
                return Ok(c.info);
            }
        }
        // The tags' id, else the node's lookup (`library_match`)
        (lib.artist_match(artist_id)?.map(|m| m.mbid), a.name)
    };
    let Some(mbid) = mbid else {
        return Ok(ArtistInfo {
            note: Some("No MusicBrainz artist id yet.".into()),
            ..ArtistInfo::default()
        });
    };
    let flight = (artist_id, lang.to_string());
    if !in_flight().lock().expect("in-flight lock").insert(flight.clone()) {
        return Ok(ArtistInfo {
            note: Some("Looking this artist up…".into()),
            ..ArtistInfo::default()
        });
    }
    let info = fetch(s, artist_id, mbid, &name, lang).await;
    in_flight().lock().expect("in-flight lock").remove(&flight);
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

/// A Commons file's name from an upload URL (`…/commons/a/ab/Name.jpg`, or a thumbnail's
/// `…/commons/thumb/a/ab/Name.jpg/320px-Name.jpg`).
fn commons_name(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    if u.host_str()? != "upload.wikimedia.org" {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.collect();
    let at = segs.iter().position(|s| *s == "commons")?;
    let rest = &segs[at + 1..];
    let name = if rest.first() == Some(&"thumb") {
        rest.get(3)?
    } else {
        rest.get(2)?
    };
    Some(
        url::form_urlencoded::parse(format!("n={name}").as_bytes())
            .next()?
            .1
            .into_owned(),
    )
}

async fn fetch(s: &NodeState, artist_id: i64, mbid: Mbid, name: &str, lang: &str) -> ArtistInfo {
    let bases = s.info_bases.read().expect("info bases").clone();
    let http = client(s);
    let mut info = ArtistInfo {
        musicbrainz: Some(format!("https://musicbrainz.org/artist/{mbid}")),
        ..ArtistInfo::default()
    };
    // 1. MusicBrainz: URL relations.
    let mb = about::get_json(
        s,
        &format!(
            "{}/ws/2/artist/{mbid}?inc=url-rels&fmt=json",
            bases.musicbrainz.trim_end_matches('/')
        ),
    )
    .await;
    let rels = mb.as_ref().map(about::relations).unwrap_or_default();
    info.homepage = rels.homepage.clone();
    info.links = rels.links.clone();
    // 2. Wikidata: portrait, facts and the article.
    let ent = match &rels.wikidata {
        Some(q) => {
            info.wikidata = Some(format!("https://www.wikidata.org/wiki/{q}"));
            about::entity(s, q).await
        }
        None => None,
    };
    let mut commons_file: Option<String> = ent.as_ref().and_then(|e| {
        e["claims"]["P18"][0]["mainsnak"]["datavalue"]["value"]
            .as_str()
            .map(str::to_string)
    });
    if let Some(e) = &ent {
        info.facts = about::artist_facts(s, e, lang).await;
    }
    // 3. Wikipedia summary.
    if let Some((l, title)) = about::article(lang, &rels, ent.as_ref()) {
        if let Some((text, picture)) = about::summary(s, &l, &title).await {
            info.bio = Some(text.text.clone());
            info.wikipedia = text.url.clone();
            info.text = Some(text);
            // No P18: the article's picture, credited from Commons when it is there
            if commons_file.is_none() {
                commons_file = picture.as_deref().and_then(commons_name);
                if let (None, Some(src)) = (&commons_file, &picture) {
                    info.image = download(&http, src, &image_path(s, artist_id)).await;
                    if info.image {
                        info.image_from = Some("Wikipedia".into());
                    }
                }
            }
        }
    }
    // 4. The portrait from Commons, with its author and licence.
    if let Some(file) = commons_file {
        if let Some((url, credit)) = about::commons_file(s, &file).await {
            info.image = download(&http, &url, &image_path(s, artist_id)).await;
            if info.image {
                info.image_credit = Some(credit);
            }
        }
        if !info.image {
            // The file without its details (Commons' API didn't answer): its page has the credit
            let page = format!(
                "https://commons.wikimedia.org/wiki/File:{}",
                about::urlencode(&file.replace(' ', "_"))
            );
            let url = format!(
                "{}/wiki/Special:FilePath/{}?width=800",
                bases.commons.trim_end_matches('/'),
                about::urlencode(&file.replace(' ', "_"))
            );
            info.image = download(&http, &url, &image_path(s, artist_id)).await;
            if info.image {
                info.image_credit = Some(crate::types::ImageCredit {
                    file_page: Some(page),
                    source: "Wikimedia Commons".into(),
                    ..Default::default()
                });
            }
        }
    }
    // 5. fanart.tv: a portrait, with the project's key.
    if !info.image {
        if let Some(key) = bases.fanart_key.as_deref() {
            if let Some(v) = about::get_json(
                s,
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
        if let Some(v) = about::get_json(
            s,
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
                if let Some(b) = &info.bio {
                    info.bio_from = Some("TheAudioDB".into());
                    info.text = Some(AboutText {
                        text: b.clone(),
                        source: "TheAudioDB".into(),
                        ..AboutText::default()
                    });
                }
            }
        }
    }
    // 7. What Pinakes knows is "also on" (by the artist's MusicBrainz link)
    about::with_pinakes(
        s,
        &format!("https://musicbrainz.org/artist/{mbid}"),
        &mut info.links,
    )
    .await;
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
