//! "About" for library artists, albums and songs from open sources, keyed by their MusicBrainz
//! ids (from the tags or `library_match`): MusicBrainz URL relations give the "Also on" links
//! (TIDAL, Qobuz, Spotify, Apple Music, Deezer, Bandcamp, YouTube, SoundCloud, Discogs, the
//! official site) and the Wikidata item; Wikidata gives the Wikipedia article in the reader's
//! language (English when there is none), a few facts with labels in that language, and the
//! artist's picture on Wikimedia Commons with its author and licence; Wikipedia's REST summary
//! gives the text, shown with its CC BY-SA credit.
//!
//! Every Wikimedia request carries the node's User-Agent (with its contact) and waits its turn
//! at the node's Wikimedia pace (200 a minute at most); MusicBrainz goes through the shared gate.
//! Answers are cached in the library settings per item and language: 30 days when something was
//! found, a day when not. Albums and songs are fetched on the page's first view, which waits a
//! few seconds for them and then shows the page without; the fetch goes on and is cached.

use crate::state::NodeState;
use crate::types::{AboutText, Fact, ImageCredit, Version};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(crate) const HIT_TTL_SECS: i64 = 30 * 24 * 3600;
pub(crate) const MISS_TTL_SECS: i64 = 24 * 3600;
/// Wikipedia's text licence.
const WIKIPEDIA_LICENCE: &str = "CC BY-SA 4.0";
const WIKIPEDIA_LICENCE_URL: &str = "https://creativecommons.org/licenses/by-sa/4.0/";
/// How long an album or song page waits for its first fetch.
const PAGE_WAIT: Duration = Duration::from_secs(6);

/// The reader's language for Wikipedia: `?lang=` (the browser's), else the first of
/// Accept-Language; a bare code (`de`, `pt`, `zh`), English otherwise.
pub(crate) fn language(query: Option<&str>, headers: &axum::http::HeaderMap) -> String {
    let header = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next());
    query
        .into_iter()
        .chain(header)
        .filter_map(|l| {
            let code = l.trim().split(['-', '_', ';']).next()?.to_ascii_lowercase();
            ((2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase())).then_some(code)
        })
        .next()
        .unwrap_or_else(|| "en".into())
}

/// One request at a time to Wikimedia, `interval` apart (200 a minute by default).
async fn wikimedia_turn(interval: Duration) {
    static NEXT: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);
    let mut next = NEXT.lock().await;
    if let Some(t) = *next {
        let now = Instant::now();
        if t > now {
            tokio::time::sleep(t - now).await;
        }
    }
    *next = Some(Instant::now() + interval);
}

/// A GET of JSON: MusicBrainz (`/ws/2/`) through the node's gate, Wikimedia at its pace.
pub(crate) async fn get_json(s: &NodeState, url: &str) -> Option<Value> {
    let http = s.http_client();
    let bases = s.info_bases.read().expect("info bases").clone();
    let mut turn = None;
    if url.contains("/ws/2/") {
        turn = Some(pm_library::musicbrainz::take_turn(url, Duration::from_secs(1)).await);
    } else if bases.is_wikimedia(url) {
        wikimedia_turn(bases.wikimedia_interval).await;
    }
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
                Duration::from_secs(2),
            ));
        }
    }
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

pub(crate) fn urlencode(s: &str) -> String {
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

/// What an entity's MusicBrainz URL relations say.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Relations {
    pub wikidata: Option<String>,
    /// A Wikipedia article linked directly: (language, title).
    pub wikipedia: Vec<(String, String)>,
    pub homepage: Option<String>,
    /// "Also on": one link per service.
    pub links: Vec<Version>,
}

/// The services "Also on" shows, by host: (slug, name).
fn service_of(url: &str) -> Option<(&'static str, &'static str)> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.trim_start_matches("www.");
    Some(match host {
        "tidal.com" | "listen.tidal.com" => ("tidal", "TIDAL"),
        "qobuz.com" | "open.qobuz.com" | "play.qobuz.com" => ("qobuz", "Qobuz"),
        "open.spotify.com" => ("spotify", "Spotify"),
        "music.apple.com" | "itunes.apple.com" => ("apple-music", "Apple Music"),
        "deezer.com" => ("deezer", "Deezer"),
        "bandcamp.com" => ("bandcamp", "Bandcamp"),
        h if h.ends_with(".bandcamp.com") => ("bandcamp", "Bandcamp"),
        "youtube.com" | "music.youtube.com" | "youtu.be" => ("youtube", "YouTube"),
        "soundcloud.com" => ("soundcloud", "SoundCloud"),
        "discogs.com" => ("discogs", "Discogs"),
        _ => return None,
    })
}

/// Sort an entity's `relations` (from `inc=url-rels`) into Wikidata, Wikipedia, the official
/// site and one "Also on" link per service. Anyone can edit MusicBrainz: only web links count.
pub(crate) fn relations(v: &Value) -> Relations {
    let mut r = Relations::default();
    for rel in v["relations"].as_array().into_iter().flatten() {
        let kind = rel["type"].as_str().unwrap_or("");
        let Some(url) = rel["url"]["resource"].as_str().and_then(crate::web::web_link) else {
            continue;
        };
        if kind == "wikidata" {
            let q = url.rsplit('/').next().unwrap_or("");
            if r.wikidata.is_none()
                && q.len() > 1
                && q.starts_with('Q')
                && q[1..].bytes().all(|b| b.is_ascii_digit())
            {
                r.wikidata = Some(q.to_string());
            }
            continue;
        }
        if kind == "wikipedia" {
            if let Ok(u) = url::Url::parse(&url) {
                let lang = u
                    .host_str()
                    .unwrap_or("")
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_string();
                if let Some(title) = u.path().strip_prefix("/wiki/") {
                    r.wikipedia.push((lang, title.to_string()));
                }
            }
            continue;
        }
        let (slug, name) = match service_of(&url) {
            Some(s) => s,
            None if kind == "official homepage" => ("homepage", "Official site"),
            None => continue,
        };
        if slug == "homepage" && r.homepage.is_none() {
            r.homepage = Some(url.clone());
        }
        if r.links.iter().any(|l| l.source == slug) {
            continue;
        }
        let id = crate::discover::service_link(&url, true)
            .map(|l| format!("{}:{}", l.kind, l.id))
            .unwrap_or_default();
        r.links.push(Version {
            source: slug.into(),
            id,
            label: name.into(),
            uri: crate::discover::playable_uri(&url),
            url,
            page: None,
        });
    }
    r
}

/// Add Pinakes' "also on" for the item's MusicBrainz URI to `links`, a service once; never
/// waits long for it.
pub(crate) async fn with_pinakes(s: &NodeState, uri: &str, links: &mut Vec<Version>) {
    let Some(d) = s.discovery() else {
        return;
    };
    let card = tokio::time::timeout(Duration::from_millis(1500), d.pinakes().card_for_uri(uri)).await;
    let Ok(Ok(Some(card))) = card else {
        return;
    };
    for v in crate::discover::also_on_rows(&card.also_on) {
        let slug = service_of(&v.url).map(|(slug, _)| slug.to_string());
        let source = slug.clone().unwrap_or_else(|| v.source.clone());
        if links.iter().any(|l| l.source == source || l.url == v.url) {
            continue;
        }
        links.push(Version { source, ..v });
    }
}

/// A Wikidata item.
pub(crate) async fn entity(s: &NodeState, q: &str) -> Option<Value> {
    let base = s.info_bases.read().expect("info bases").wikidata.clone();
    let v = get_json(
        s,
        &format!("{}/wiki/Special:EntityData/{q}.json", base.trim_end_matches('/')),
    )
    .await?;
    Some(v["entities"][q].clone())
}

/// The article to show: the reader's language, else English; from MusicBrainz's direct links
/// first, then the item's sitelinks.
pub(crate) fn article(lang: &str, rels: &Relations, ent: Option<&Value>) -> Option<(String, String)> {
    for want in [lang, "en"] {
        if let Some((l, t)) = rels.wikipedia.iter().find(|(l, _)| l == want) {
            return Some((l.clone(), t.clone()));
        }
        if let Some(t) = ent.and_then(|e| e["sitelinks"][format!("{want}wiki")]["title"].as_str()) {
            return Some((want.to_string(), t.replace(' ', "_")));
        }
    }
    None
}

/// Wikipedia's summary of an article: its first paragraph, with the credit it needs, and the
/// article's picture.
pub(crate) async fn summary(s: &NodeState, lang: &str, title: &str) -> Option<(AboutText, Option<String>)> {
    let base = s.info_bases.read().expect("info bases").wikipedia_for(lang);
    let v = get_json(
        s,
        &format!(
            "{}/api/rest_v1/page/summary/{}",
            base.trim_end_matches('/'),
            urlencode(title)
        ),
    )
    .await?;
    let text = v["extract"].as_str().map(str::trim).filter(|e| !e.is_empty())?;
    let picture = v["originalimage"]["source"]
        .as_str()
        .or(v["thumbnail"]["source"].as_str())
        .map(str::to_string);
    let text = AboutText {
        text: text.to_string(),
        source: "Wikipedia".into(),
        title: v["title"].as_str().map(str::to_string),
        url: v["content_urls"]["desktop"]["page"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(format!("https://{lang}.wikipedia.org/wiki/{title}"))),
        lang: Some(lang.to_string()),
        licence: Some(WIKIPEDIA_LICENCE.into()),
        licence_url: Some(WIKIPEDIA_LICENCE_URL.into()),
    };
    Some((text, picture))
}

/// The ids of a property's values (`Q…`), in order.
fn items(ent: &Value, prop: &str) -> Vec<String> {
    ent["claims"][prop]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["rank"].as_str() != Some("deprecated"))
        .filter_map(|c| {
            c["mainsnak"]["datavalue"]["value"]["id"]
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// The year of a property's first time value.
fn year(ent: &Value, prop: &str) -> Option<i32> {
    let t = ent["claims"][prop][0]["mainsnak"]["datavalue"]["value"]["time"].as_str()?;
    let (sign, rest) = t.split_at(1);
    let y: i32 = rest.split('-').next()?.parse().ok()?;
    Some(if sign == "-" { -y } else { y })
}

/// Labels for items in `lang` (English where it has none), one request for up to 50.
async fn labels(s: &NodeState, ids: &[String], lang: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let base = s.info_bases.read().expect("info bases").wikidata.clone();
    let ids: Vec<&str> = ids.iter().take(50).map(String::as_str).collect();
    let url = format!(
        "{}/w/api.php?action=wbgetentities&ids={}&props=labels&languages={lang}%7Cen&format=json",
        base.trim_end_matches('/'),
        ids.join("%7C")
    );
    let Some(v) = get_json(s, &url).await else {
        return out;
    };
    for id in ids {
        let l = &v["entities"][id]["labels"];
        if let Some(name) = l[lang]["value"].as_str().or(l["en"]["value"].as_str()) {
            out.insert(id.to_string(), name.to_string());
        }
    }
    out
}

/// An artist's facts from its Wikidata item: origin (where a group formed, where a person was
/// born, else the country), active years, up to three genres, up to eight members.
pub(crate) async fn artist_facts(s: &NodeState, ent: &Value, lang: &str) -> Vec<Fact> {
    let person = items(ent, "P31").iter().any(|q| q == "Q5");
    let origin: Vec<String> = ["P740", "P19", "P495"]
        .iter()
        .map(|p| items(ent, p))
        .find(|v| !v.is_empty())
        .map(|v| v.into_iter().take(1).collect())
        .unwrap_or_default();
    let genres: Vec<String> = items(ent, "P136").into_iter().take(3).collect();
    let members: Vec<String> = if person {
        Vec::new()
    } else {
        items(ent, "P527").into_iter().take(8).collect()
    };
    let all: Vec<String> = origin.iter().chain(&genres).chain(&members).cloned().collect();
    let names = labels(s, &all, lang).await;
    let named = |ids: &[String]| -> Option<String> {
        let v: Vec<&str> = ids
            .iter()
            .filter_map(|q| names.get(q).map(String::as_str))
            .collect();
        (!v.is_empty()).then(|| v.join(", "))
    };
    let mut facts = Vec::new();
    if let Some(o) = named(&origin) {
        facts.push(fact("Origin", o));
    }
    let start = year(ent, "P2031").or_else(|| (!person).then(|| year(ent, "P571")).flatten());
    let end = year(ent, "P2032").or_else(|| (!person).then(|| year(ent, "P576")).flatten());
    match (start, end) {
        (Some(a), Some(b)) => facts.push(fact("Active", format!("{a}–{b}"))),
        (Some(a), None) => facts.push(fact("Active", format!("since {a}"))),
        _ => {}
    }
    if let Some(g) = named(&genres) {
        facts.push(fact("Genres", g));
    }
    if let Some(m) = named(&members) {
        facts.push(fact("Members", m));
    }
    facts
}

fn fact(label: &str, value: String) -> Fact {
    Fact {
        label: label.into(),
        value,
        href: None,
    }
}

/// A Commons file's picture (a thumbnail URL) and its credit: author, licence, file page.
pub(crate) async fn commons_file(s: &NodeState, file: &str) -> Option<(String, ImageCredit)> {
    let base = s.info_bases.read().expect("info bases").commons.clone();
    let title = format!("File:{}", file.replace(' ', "_"));
    let v = get_json(
        s,
        &format!(
            "{}/w/api.php?action=query&titles={}&prop=imageinfo&iiprop=url%7Cextmetadata&iiurlwidth=800&format=json",
            base.trim_end_matches('/'),
            urlencode(&title)
        ),
    )
    .await?;
    let page = v["query"]["pages"].as_object()?.values().next()?.clone();
    let info = &page["imageinfo"][0];
    let url = info["thumburl"].as_str().or(info["url"].as_str())?.to_string();
    let meta = &info["extmetadata"];
    let text = |k: &str| {
        meta[k]["value"]
            .as_str()
            .map(strip_html)
            .filter(|t| !t.is_empty())
    };
    Some((
        url,
        ImageCredit {
            author: text("Artist").or_else(|| text("Credit")),
            licence: text("LicenseShortName"),
            licence_url: meta["LicenseUrl"]["value"]
                .as_str()
                .and_then(crate::web::web_link),
            file_page: info["descriptionurl"].as_str().and_then(crate::web::web_link),
            source: "Wikimedia Commons".into(),
        },
    ))
}

/// Text of an HTML snippet (Commons' author field is HTML).
fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut tag = false;
    for c in s.chars() {
        match c {
            '<' => tag = true,
            '>' => tag = false,
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// What open sources say about an album or a song.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct About {
    #[serde(default)]
    pub text: Option<AboutText>,
    #[serde(default)]
    pub facts: Vec<Fact>,
    #[serde(default)]
    pub links: Vec<Version>,
    #[serde(default)]
    pub wikidata: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Cached {
    at: i64,
    about: About,
}

/// An item's About from cache, else fetched (`fetch`) and cached; the page waits
/// [`PAGE_WAIT`] at most and the fetch goes on after that.
async fn cached<F>(s: &std::sync::Arc<NodeState>, key: String, fetch: F) -> Option<About>
where
    F: std::future::Future<Output = About> + Send + 'static,
{
    {
        let lib = s.library.lock().expect("library poisoned");
        if let Some(c) = lib
            .get_setting(&key)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str::<Cached>(&j).ok())
        {
            let found = c.about.text.is_some() || !c.about.facts.is_empty() || !c.about.links.is_empty();
            let ttl = if found { HIT_TTL_SECS } else { MISS_TTL_SECS };
            if pm_library::db::now_unix() - c.at < ttl {
                return Some(c.about);
            }
        }
    }
    if !in_flight().lock().expect("in-flight").insert(key.clone()) {
        return None;
    }
    let st = s.clone();
    let task = tokio::spawn(async move {
        let about = fetch.await;
        let c = Cached {
            at: pm_library::db::now_unix(),
            about: about.clone(),
        };
        let _ = st
            .library
            .lock()
            .expect("library poisoned")
            .set_setting(&key, &serde_json::to_string(&c).expect("about serializes"));
        in_flight().lock().expect("in-flight").remove(&key);
        about
    });
    tokio::time::timeout(PAGE_WAIT, task).await.ok()?.ok()
}

fn in_flight() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(Default::default)
}

/// The text from an entity's Wikidata item and MusicBrainz links.
async fn text_for(s: &NodeState, rels: &Relations, lang: &str) -> (Option<AboutText>, Option<Value>) {
    let ent = match &rels.wikidata {
        Some(q) => entity(s, q).await,
        None => None,
    };
    let text = match article(lang, rels, ent.as_ref()) {
        Some((l, t)) => summary(s, &l, &t).await.map(|(text, _)| text),
        None => None,
    };
    (text, ent)
}

/// An album's About by its release group (and a tagged release, which carries the label and
/// most streaming links): the Wikipedia article if the group has one, label and release date.
pub(crate) async fn album(
    s: &std::sync::Arc<NodeState>,
    album_id: i64,
    group: pm_model::Mbid,
    release: Option<pm_model::Mbid>,
    lang: &str,
) -> Option<About> {
    let st = s.clone();
    let lang_owned = lang.to_string();
    cached(s, format!("album.about.{album_id}.{lang}"), async move {
        let s = &st;
        let lang = lang_owned.as_str();
        let mb = s.info_bases.read().expect("info bases").musicbrainz.clone();
        let mb = mb.trim_end_matches('/').to_string();
        let mut about = About::default();
        let Some(g) = get_json(
            s,
            &format!("{mb}/ws/2/release-group/{group}?inc=url-rels+releases&fmt=json"),
        )
        .await
        else {
            return about;
        };
        let mut rels = relations(&g);
        // The tagged release, else the group's first official one
        let release = release.map(|r| r.to_string()).or_else(|| {
            g["releases"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["status"].as_str().is_none_or(|st| st == "Official"))
                .filter_map(|r| {
                    Some((
                        r["date"].as_str().unwrap_or("9999").to_string(),
                        r["id"].as_str()?.to_string(),
                    ))
                })
                .min()
                .map(|(_, id)| id)
        });
        let mut labels: Vec<String> = Vec::new();
        if let Some(r) = release {
            if let Some(rv) =
                get_json(s, &format!("{mb}/ws/2/release/{r}?inc=labels+url-rels&fmt=json")).await
            {
                for l in rv["label-info"].as_array().into_iter().flatten() {
                    if let Some(n) = l["label"]["name"].as_str() {
                        if !labels.iter().any(|x| x == n) && labels.len() < 2 {
                            labels.push(n.to_string());
                        }
                    }
                }
                let more = relations(&rv);
                for l in more.links {
                    if !rels.links.iter().any(|x| x.source == l.source) {
                        rels.links.push(l);
                    }
                }
            }
        }
        if let Some(d) = g["first-release-date"].as_str().filter(|d| !d.is_empty()) {
            about.facts.push(fact("Released", d.to_string()));
        }
        if !labels.is_empty() {
            about.facts.push(fact("Label", labels.join(", ")));
        }
        let (text, _) = text_for(s, &rels, lang).await;
        about.text = text;
        about.wikidata = rels
            .wikidata
            .clone()
            .map(|q| format!("https://www.wikidata.org/wiki/{q}"));
        about.links = rels.links;
        with_pinakes(
            s,
            &format!("https://musicbrainz.org/release-group/{group}"),
            &mut about.links,
        )
        .await;
        about
    })
    .await
}

/// A song's About by its recording: the work it records, and that work's Wikipedia article.
pub(crate) async fn song(
    s: &std::sync::Arc<NodeState>,
    track_id: i64,
    recording: pm_model::Mbid,
    lang: &str,
) -> Option<About> {
    let st = s.clone();
    let lang_owned = lang.to_string();
    cached(s, format!("song.about.{track_id}.{lang}"), async move {
        let s = &st;
        let lang = lang_owned.as_str();
        let mb = s.info_bases.read().expect("info bases").musicbrainz.clone();
        let mb = mb.trim_end_matches('/').to_string();
        let mut about = About::default();
        let Some(r) = get_json(
            s,
            &format!("{mb}/ws/2/recording/{recording}?inc=work-rels+url-rels&fmt=json"),
        )
        .await
        else {
            return about;
        };
        let mut rels = relations(&r);
        let work = r["relations"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["target-type"].as_str() == Some("work"))
            .map(|x| x["work"].clone());
        if let Some(w) = work.as_ref() {
            if let (Some(id), Some(title)) = (w["id"].as_str(), w["title"].as_str()) {
                about.facts.push(Fact {
                    label: "Work".into(),
                    value: title.to_string(),
                    href: Some(format!("https://musicbrainz.org/work/{id}")),
                });
                if let Some(wv) = get_json(s, &format!("{mb}/ws/2/work/{id}?inc=url-rels&fmt=json")).await {
                    let wr = relations(&wv);
                    rels.wikidata = rels.wikidata.or(wr.wikidata);
                    rels.wikipedia.extend(wr.wikipedia);
                }
            }
        }
        let (text, _) = text_for(s, &rels, lang).await;
        about.text = text;
        about.wikidata = rels
            .wikidata
            .clone()
            .map(|q| format!("https://www.wikidata.org/wiki/{q}"));
        about.links = rels.links;
        about
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_language_comes_from_the_query_then_the_header() {
        let mut h = axum::http::HeaderMap::new();
        assert_eq!(language(None, &h), "en");
        h.insert(
            axum::http::header::ACCEPT_LANGUAGE,
            "de-CH,de;q=0.9,en;q=0.8".parse().unwrap(),
        );
        assert_eq!(language(None, &h), "de");
        assert_eq!(language(Some("fr-FR"), &h), "fr");
        // Never a host name or anything that isn't a language code
        assert_eq!(language(Some("evil.example/"), &h), "de");
        assert_eq!(language(Some("x"), &axum::http::HeaderMap::new()), "en");
    }

    #[test]
    fn relations_give_one_link_per_service_and_the_wiki_ids() {
        let rel = |kind: &str, url: &str| json!({"type": kind, "url": {"resource": url}});
        let v = json!({"relations": [
            rel("wikidata", "https://www.wikidata.org/wiki/Q123"),
            rel("wikipedia", "https://de.wikipedia.org/wiki/7_Dollar_Taxi"),
            rel("official homepage", "https://7dollartaxi.ch/"),
            rel("free streaming", "https://open.spotify.com/artist/abc123"),
            rel("free streaming", "https://open.spotify.com/artist/second"),
            rel("streaming", "https://tidal.com/browse/artist/42"),
            rel("purchase for download", "https://7dollartaxi.bandcamp.com/"),
            rel("discogs", "https://www.discogs.com/artist/1-Taxi"),
            rel("youtube", "https://www.youtube.com/channel/x"),
            rel("social network", "https://twitter.com/x"),
            rel("official homepage", "javascript:alert(1)"),
            rel("wikidata", "https://www.wikidata.org/wiki/not-an-item"),
        ]});
        let r = relations(&v);
        assert_eq!(
            r.wikidata.as_deref(),
            Some("Q123"),
            "not the link that isn't an item"
        );
        assert_eq!(r.wikipedia, vec![("de".to_string(), "7_Dollar_Taxi".to_string())]);
        assert_eq!(r.homepage.as_deref(), Some("https://7dollartaxi.ch/"));
        let sources: Vec<&str> = r.links.iter().map(|l| l.source.as_str()).collect();
        assert_eq!(
            sources,
            ["homepage", "spotify", "tidal", "bandcamp", "discogs", "youtube"]
        );
        let tidal = r.links.iter().find(|l| l.source == "tidal").unwrap();
        assert_eq!((tidal.label.as_str(), tidal.id.as_str()), ("TIDAL", "artist:42"));
        assert_eq!(r.links[1].url, "https://open.spotify.com/artist/abc123");
    }

    #[test]
    fn the_article_is_in_the_readers_language_else_english() {
        let ent = json!({"sitelinks": {"enwiki": {"title": "Seven Dollar Taxi"}, "frwiki": {"title": "Taxi à sept"}}});
        let none = Relations::default();
        assert_eq!(
            article("fr", &none, Some(&ent)),
            Some(("fr".into(), "Taxi_à_sept".into()))
        );
        assert_eq!(
            article("de", &none, Some(&ent)),
            Some(("en".into(), "Seven_Dollar_Taxi".into()))
        );
        let direct = Relations {
            wikipedia: vec![("de".into(), "Sieben".into())],
            ..Relations::default()
        };
        assert_eq!(
            article("de", &direct, Some(&ent)),
            Some(("de".into(), "Sieben".into()))
        );
        assert_eq!(article("de", &none, None), None);
    }

    #[test]
    fn commons_authors_are_plain_text() {
        assert_eq!(
            strip_html("<a href=\"//commons.wikimedia.org/wiki/User:X\">Jane &amp; John</a>\n Doe"),
            "Jane & John Doe"
        );
    }
}
