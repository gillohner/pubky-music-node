//! Podcasts (`podcasts`, owner-approved list §6.2): subscribe by RSS URL or from search,
//! episodes on any zone, resume where you left off, the newest unplayed episodes for Home.
//!
//! - **Search** goes through Pinakes (`type=PodcastSeries`): it groups a show's pages (Apple
//!   Podcasts, Spotify, Podcast Index, the RSS feed) and holds the Podcast Index key on its side,
//!   so the node never has it. When Pinakes has nothing, Apple's iTunes Search API (no key).
//! - **Feeds** are plain RSS 2.0 with the iTunes tags most podcasts use. They live in the
//!   node's settings and refresh every few hours.
//! - **Resume**: while a zone plays an episode, its position is recorded every few seconds;
//!   playing it again starts there. An episode counts as played past 95 %.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use pm_zones::{Insert, QueueItem, ZoneId};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const SOURCE_ID: &str = "podcasts";
pub(crate) const FEEDS: &str = "podcasts.feeds";
pub(crate) const PROGRESS: &str = "podcasts.progress";
/// Episodes kept per feed (newest first).
const MAX_EPISODES: usize = 300;
const REFRESH_EVERY: Duration = Duration::from_secs(6 * 3600);
const PROGRESS_EVERY: Duration = Duration::from_secs(5);
const ITUNES: &str = "https://itunes.apple.com/search";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Episode {
    /// Stable within the node: a hash of the feed and the episode's guid.
    pub id: String,
    pub title: String,
    /// The audio file.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Feed {
    pub id: String,
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Unix seconds of the last successful refresh.
    #[serde(default)]
    pub refreshed: i64,
    #[serde(default)]
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct Progress {
    pub position_ms: u64,
    #[serde(default)]
    pub played: bool,
    /// Unix seconds.
    #[serde(default)]
    pub updated: i64,
}

pub fn feed_id(url: &str) -> String {
    blake3::hash(url.trim().as_bytes()).to_hex()[..16].to_string()
}

fn episode_id(feed: &str, guid: &str) -> String {
    blake3::hash(format!("{feed}\n{guid}").as_bytes()).to_hex()[..16].to_string()
}

/// `HH:MM:SS`, `MM:SS` or seconds.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total: f64 = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some((total * 1000.0) as u64)
}

fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc2822)
        .or_else(|_| time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339))
        .ok()
        .map(|d| d.unix_timestamp())
}

/// Text without markup, collapsed, at most `max` characters.
fn plain(html: &str, max: usize) -> Option<String> {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(if collapsed.chars().count() > max {
        format!("{}…", collapsed.chars().take(max).collect::<String>())
    } else {
        collapsed
    })
}

fn unref(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        n if n.starts_with("#x") => u32::from_str_radix(&n[2..], 16)
            .ok()
            .and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_default(),
        n if n.starts_with('#') => n[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Parse an RSS 2.0 podcast feed (iTunes tags understood).
pub fn parse_feed(url: &str, xml: &str) -> Result<Feed, String> {
    let fid = feed_id(url);
    let mut r = Reader::from_str(xml);
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut feed = Feed {
        id: fid.clone(),
        url: url.trim().to_string(),
        ..Default::default()
    };
    let mut ep: Option<(Episode, String)> = None; // episode, guid
    let mut saw_channel = false;
    let attr = |e: &quick_xml::events::BytesStart, name: &str| -> Option<String> {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    loop {
        let ev = r.read_event().map_err(|e| format!("not a feed: {e}"))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = e.name().as_ref().to_string();
                let empty = matches!(ev, Event::Empty(_));
                let in_item = ep.is_some();
                match name.as_str() {
                    "channel" => saw_channel = true,
                    "item" => ep = Some((Episode::default(), String::new())),
                    "enclosure" if in_item => {
                        if let Some((e2, _)) = ep.as_mut() {
                            if let Some(u) = attr(e, "url") {
                                e2.url = u;
                            }
                            e2.mime = attr(e, "type");
                        }
                    }
                    "itunes:image" => {
                        let href = attr(e, "href");
                        match ep.as_mut() {
                            Some((e2, _)) => e2.image = e2.image.take().or(href),
                            None => feed.image = feed.image.take().or(href),
                        }
                    }
                    _ => {}
                }
                if !empty {
                    path.push(name);
                    text.clear();
                }
            }
            Event::Text(t) => text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0)),
            Event::GeneralRef(g) => text.push_str(&unref(g.as_ref())),
            Event::CData(c) => text.push_str(c.as_ref()),
            Event::End(_) => {
                let name = path.pop().unwrap_or_default();
                let parent = path.last().map(String::as_str).unwrap_or("");
                let v = std::mem::take(&mut text).trim().to_string();
                if name == "item" {
                    if let Some((mut e2, guid)) = ep.take() {
                        if e2.url.starts_with("http") {
                            let key = if guid.is_empty() { e2.url.clone() } else { guid };
                            e2.id = episode_id(&fid, &key);
                            feed.episodes.push(e2);
                        }
                    }
                    continue;
                }
                if let Some((e2, guid)) = ep.as_mut() {
                    match name.as_str() {
                        "title" if e2.title.is_empty() => e2.title = v,
                        "guid" => *guid = v,
                        "pubDate" => e2.published = parse_date(&v),
                        "itunes:duration" => e2.duration_ms = parse_duration(&v),
                        "description" | "itunes:summary" | "content:encoded" if e2.description.is_none() => {
                            e2.description = plain(&v, 600)
                        }
                        _ => {}
                    }
                } else if parent == "channel" {
                    match name.as_str() {
                        "title" if feed.title.is_empty() => feed.title = v,
                        "link" if feed.link.is_none() && v.starts_with("http") => feed.link = Some(v),
                        "itunes:author" if feed.author.is_none() && !v.is_empty() => feed.author = Some(v),
                        "description" | "itunes:summary" if feed.description.is_none() => {
                            feed.description = plain(&v, 1000)
                        }
                        _ => {}
                    }
                } else if parent == "image" && name == "url" && feed.image.is_none() && v.starts_with("http")
                {
                    feed.image = Some(v);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_channel || feed.title.is_empty() {
        return Err("not an RSS podcast feed".into());
    }
    feed.episodes
        .sort_by_key(|e| std::cmp::Reverse(e.published.unwrap_or(0)));
    feed.episodes.truncate(MAX_EPISODES);
    Ok(feed)
}

// ---- storage ----

pub(crate) fn load<T: for<'de> Deserialize<'de> + Default>(s: &NodeState, key: &str) -> T {
    let lib = s.library.lock().expect("library poisoned");
    lib.get_setting(key)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

pub(crate) fn store<T: Serialize>(s: &NodeState, key: &str, v: &T) -> Result<(), ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    lib.set_setting(key, &serde_json::to_string(v).expect("serialize"))?;
    Ok(())
}

pub(crate) fn feeds(s: &NodeState) -> Vec<Feed> {
    load(s, FEEDS)
}

pub(crate) fn progress(s: &NodeState) -> HashMap<String, Progress> {
    load(s, PROGRESS)
}

pub(crate) fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn bad_gateway(m: String) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, m)
}

pub(crate) async fn fetch_feed(s: &NodeState, url: &str) -> Result<Feed, ApiError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::bad_request(
            "a feed URL starts with http:// or https://",
        ));
    }
    let resp = s
        .http_client()
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| bad_gateway(format!("feed: {e}")))?;
    if !resp.status().is_success() {
        return Err(bad_gateway(format!("feed answered {}", resp.status())));
    }
    let text = resp.text().await.map_err(|e| bad_gateway(format!("feed: {e}")))?;
    let mut f = parse_feed(url, &text).map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    f.refreshed = now();
    Ok(f)
}

// ---- search ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Show {
    /// `feed:<base64url of the feed URL>`: play or subscribe straight from it.
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub feed: String,
    /// The same show elsewhere (Apple Podcasts, Spotify, Podcast Index…).
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub subscribed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub site: String,
    pub uri: String,
}

pub fn show_id(feed: &str) -> String {
    format!(
        "feed:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(feed.as_bytes())
    )
}

fn feed_from_show_id(id: &str) -> Option<String> {
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(id.strip_prefix("feed:")?)
        .ok()?;
    String::from_utf8(b).ok()
}

/// Sites whose links are a show's page, not its feed.
const DIRECTORY_SITES: &[&str] = &[
    "podcasts.apple.com",
    "open.spotify.com",
    "podcastindex.org",
    "castro.fm",
    "pca.st",
    "overcast.fm",
    "music.amazon",
    "youtube.com",
    "pocketcasts.com",
];

fn is_directory(uri: &str) -> bool {
    DIRECTORY_SITES.iter().any(|d| uri.contains(d))
}

/// Pinakes search results (PodcastSeries) grouped into shows that have a feed.
pub fn shows_from_pinakes(json: &serde_json::Value) -> Vec<Show> {
    let mut out: Vec<Show> = Vec::new();
    let mut seen_groups: Vec<String> = Vec::new();
    for it in json["items"].as_array().into_iter().flatten() {
        if let Some(g) = it["group"]["id"].as_str() {
            if seen_groups.iter().any(|x| x == g) {
                continue;
            }
            seen_groups.push(g.to_string());
        }
        let own = it["uri"].as_str().unwrap_or_default();
        let mut links: Vec<Link> = Vec::new();
        let mut feed: Option<String> =
            (!is_directory(own) && own.starts_with("http")).then(|| own.to_string());
        if is_directory(own) {
            links.push(Link {
                site: it["site"].as_str().unwrap_or_default().to_string(),
                uri: own.to_string(),
            });
        }
        for a in it["also_on"].as_array().into_iter().flatten() {
            let uri = a["uri"].as_str().unwrap_or_default();
            if uri.is_empty() {
                continue;
            }
            if is_directory(uri) {
                links.push(Link {
                    site: a["site"].as_str().unwrap_or_default().to_string(),
                    uri: uri.to_string(),
                });
            } else if feed.is_none() && a["derived"].as_bool() != Some(true) && uri.starts_with("http") {
                feed = Some(uri.to_string());
            }
        }
        let Some(feed) = feed else { continue };
        // One link per site (Pinakes also lists derived forms of the same page).
        let mut sites: Vec<String> = Vec::new();
        links.retain(|l| {
            let new = !sites.contains(&l.site);
            sites.push(l.site.clone());
            new
        });
        out.push(Show {
            id: show_id(&feed),
            title: it["name"].as_str().unwrap_or_default().trim().to_string(),
            author: None,
            description: it["description"].as_str().map(str::to_string),
            image: it["image"].as_str().map(str::to_string),
            feed,
            links,
            subscribed: false,
        });
    }
    out
}

pub fn shows_from_itunes(json: &serde_json::Value) -> Vec<Show> {
    json["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let feed = r["feedUrl"].as_str()?.to_string();
            Some(Show {
                id: show_id(&feed),
                title: r["collectionName"].as_str()?.to_string(),
                author: r["artistName"].as_str().map(str::to_string),
                description: None,
                image: r["artworkUrl600"]
                    .as_str()
                    .or(r["artworkUrl100"].as_str())
                    .map(str::to_string),
                links: r["collectionViewUrl"]
                    .as_str()
                    .map(|u| {
                        vec![Link {
                            site: "Apple Podcasts".into(),
                            uri: u.to_string(),
                        }]
                    })
                    .unwrap_or_default(),
                feed,
                subscribed: false,
            })
        })
        .collect()
}

async fn get_json(s: &NodeState, url: &str, q: &[(&str, String)]) -> Result<serde_json::Value, ApiError> {
    let resp = s
        .http_client()
        .get(url)
        .query(q)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| bad_gateway(format!("{url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(bad_gateway(format!("{url} answered {}", resp.status())));
    }
    resp.json().await.map_err(|e| bad_gateway(format!("{url}: {e}")))
}

/// Shows matching `term`: Pinakes first (it keeps the Podcast Index key), then iTunes Search.
pub async fn search_shows(s: &NodeState, term: &str, limit: u32) -> Result<Vec<Show>, ApiError> {
    let pinakes = format!("{}/v1/search", s.pubky.pinakes_base.trim_end_matches('/'));
    let mut shows = match get_json(
        s,
        &pinakes,
        &[
            ("q", term.to_string()),
            ("type", "PodcastSeries".into()),
            ("limit", (limit * 3).to_string()),
        ],
    )
    .await
    {
        Ok(v) => shows_from_pinakes(&v),
        Err(e) => {
            tracing::info!("podcast search on Pinakes failed: {}", e.message);
            Vec::new()
        }
    };
    if shows.len() < limit as usize {
        match get_json(
            s,
            ITUNES,
            &[
                ("media", "podcast".into()),
                ("entity", "podcast".into()),
                ("term", term.to_string()),
                ("limit", limit.to_string()),
            ],
        )
        .await
        {
            Ok(v) => {
                for sh in shows_from_itunes(&v) {
                    if !shows.iter().any(|x| x.feed == sh.feed) {
                        shows.push(sh);
                    }
                }
            }
            Err(e) if !shows.is_empty() => tracing::info!("iTunes search: {}", e.message),
            Err(e) => return Err(e),
        }
    }
    let subscribed: Vec<String> = feeds(s).into_iter().map(|f| f.url).collect();
    for sh in shows.iter_mut() {
        sh.subscribed = subscribed.contains(&sh.feed);
    }
    shows.truncate(limit as usize);
    Ok(shows)
}

// ---- routes ----

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shows {
    pub shows: Vec<Show>,
}

/// `GET /v1/podcasts/search?q=&limit=`.
pub(crate) async fn search(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Shows> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let term = q.q.trim();
    if term.is_empty() {
        return Err(ApiError::bad_request("q is required"));
    }
    Ok(Json(Shows {
        shows: search_shows(&s, term, q.limit.unwrap_or(20).clamp(1, 50)).await?,
    }))
}

/// A subscription without its episodes, with counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub episodes: usize,
    pub unplayed: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<i64>,
    pub refreshed: i64,
}

fn summary(f: &Feed, prog: &HashMap<String, Progress>) -> Subscription {
    Subscription {
        id: f.id.clone(),
        url: f.url.clone(),
        title: f.title.clone(),
        author: f.author.clone(),
        image: f.image.clone(),
        episodes: f.episodes.len(),
        unplayed: f
            .episodes
            .iter()
            .filter(|e| !prog.get(&e.id).is_some_and(|p| p.played))
            .count(),
        latest: f.episodes.iter().filter_map(|e| e.published).max(),
        refreshed: f.refreshed,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscriptions {
    pub subscriptions: Vec<Subscription>,
}

/// `GET /v1/podcasts/subscriptions`.
pub(crate) async fn subscriptions(State(s): State<Arc<NodeState>>) -> ApiResult<Subscriptions> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let prog = progress(&s);
    Ok(Json(Subscriptions {
        subscriptions: feeds(&s).iter().map(|f| summary(f, &prog)).collect(),
    }))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Subscribe {
    /// The feed URL, or a show id from search (`feed:…`).
    pub feed: String,
}

/// `POST /v1/podcasts/subscriptions {feed}`: fetch the feed and keep it.
pub(crate) async fn subscribe(
    State(s): State<Arc<NodeState>>,
    Json(b): Json<Subscribe>,
) -> ApiResult<Subscription> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let url = feed_from_show_id(&b.feed).unwrap_or_else(|| b.feed.trim().to_string());
    let f = fetch_feed(&s, &url).await?;
    let mut all = feeds(&s);
    all.retain(|x| x.id != f.id);
    all.insert(0, f.clone());
    store(&s, FEEDS, &all)?;
    crate::gpodder::note_subscribed(&s, &f.url);
    tracing::info!(feed = %f.url, episodes = f.episodes.len(), "podcast subscribed");
    Ok(Json(summary(&f, &progress(&s))))
}

/// `DELETE /v1/podcasts/subscriptions/{id}`.
pub(crate) async fn unsubscribe(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<Subscriptions> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let mut all = feeds(&s);
    let Some(gone) = all.iter().position(|f| f.id == id).map(|i| all.remove(i)) else {
        return Err(ApiError::not_found(format!("no subscription {id}")));
    };
    store(&s, FEEDS, &all)?;
    crate::gpodder::note_unsubscribed(&s, &gone.url);
    let prog = progress(&s);
    Ok(Json(Subscriptions {
        subscriptions: all.iter().map(|f| summary(f, &prog)).collect(),
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeView {
    #[serde(flatten)]
    pub episode: Episode,
    pub feed_id: String,
    pub feed_title: String,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub played: bool,
}

fn view(f: &Feed, e: &Episode, prog: &HashMap<String, Progress>) -> EpisodeView {
    let p = prog.get(&e.id).copied().unwrap_or_default();
    EpisodeView {
        episode: Episode {
            image: e.image.clone().or_else(|| f.image.clone()),
            ..e.clone()
        },
        feed_id: f.id.clone(),
        feed_title: f.title.clone(),
        position_ms: p.position_ms,
        played: p.played,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedView {
    #[serde(flatten)]
    pub subscription: Subscription,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub items: Vec<EpisodeView>,
}

/// `GET /v1/podcasts/subscriptions/{id}`: the show with its episodes, newest first.
pub(crate) async fn subscription(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<FeedView> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let f = feeds(&s)
        .into_iter()
        .find(|f| f.id == id)
        .ok_or_else(|| ApiError::not_found(format!("no subscription {id}")))?;
    let prog = progress(&s);
    Ok(Json(FeedView {
        subscription: summary(&f, &prog),
        description: f.description.clone(),
        link: f.link.clone(),
        items: f.episodes.iter().map(|e| view(&f, e, &prog)).collect(),
    }))
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NewQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episodes {
    pub episodes: Vec<EpisodeView>,
}

/// `GET /v1/podcasts/new?limit=`: the newest unplayed episodes across subscriptions (a Home row).
pub(crate) async fn newest(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<NewQuery>,
) -> ApiResult<Episodes> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let prog = progress(&s);
    let mut all: Vec<EpisodeView> = feeds(&s)
        .iter()
        .flat_map(|f| f.episodes.iter().map(|e| view(f, e, &prog)).collect::<Vec<_>>())
        .filter(|v| !v.played)
        .collect();
    all.sort_by_key(|v| std::cmp::Reverse(v.episode.published.unwrap_or(0)));
    all.truncate(q.limit.unwrap_or(20).clamp(1, 100) as usize);
    Ok(Json(Episodes { episodes: all }))
}

/// `POST /v1/podcasts/refresh`: fetch every feed again now.
pub(crate) async fn refresh_route(State(s): State<Arc<NodeState>>) -> ApiResult<Subscriptions> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    refresh_all(&s).await;
    let prog = progress(&s);
    Ok(Json(Subscriptions {
        subscriptions: feeds(&s).iter().map(|f| summary(f, &prog)).collect(),
    }))
}

pub async fn refresh_all(s: &NodeState) {
    let current = feeds(s);
    let mut fresh: Vec<Feed> = Vec::with_capacity(current.len());
    for f in &current {
        match fetch_feed(s, &f.url).await {
            Ok(n) => fresh.push(n),
            Err(e) => {
                tracing::info!(feed = %f.url, "podcast refresh failed: {}", e.message);
                fresh.push(f.clone());
            }
        }
    }
    // Keep subscriptions made or dropped meanwhile.
    let now_ids: Vec<String> = feeds(s).into_iter().map(|f| f.id).collect();
    fresh.retain(|f| now_ids.contains(&f.id));
    for f in feeds(s) {
        if !fresh.iter().any(|x| x.id == f.id) {
            fresh.push(f);
        }
    }
    if let Err(e) = store(s, FEEDS, &fresh) {
        tracing::warn!("podcast refresh not saved: {}", e.message);
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlayEpisode {
    pub zone: String,
    pub episode: String,
    /// Start from the beginning even when there is a saved position.
    #[serde(default)]
    pub restart: bool,
}

fn find_episode(s: &NodeState, id: &str) -> Option<(Feed, Episode)> {
    feeds(s).into_iter().find_map(|f| {
        f.episodes
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .map(|e| (f.clone(), e))
    })
}

fn queue_item(f: &Feed, e: &Episode) -> QueueItem {
    let mut item = QueueItem::new(e.url.clone());
    item.title = Some(e.title.clone());
    item.artist = Some(f.author.clone().unwrap_or_else(|| f.title.clone()));
    item.album = Some(f.title.clone());
    item.duration_ms = e.duration_ms;
    item.image = e.image.clone().or_else(|| f.image.clone());
    item
}

/// `POST /v1/podcasts/play {zone, episode, restart?}`: play an episode, from where it was left.
pub(crate) async fn play(State(s): State<Arc<NodeState>>, Json(b): Json<PlayEpisode>) -> ApiResult<Enqueued> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let zone = s
        .zones
        .get(&ZoneId::new(&b.zone))
        .ok_or_else(|| ApiError::not_found(format!("no zone {}", b.zone)))?;
    let (f, e) = find_episode(&s, &b.episode)
        .ok_or_else(|| ApiError::not_found(format!("no episode {}", b.episode)))?;
    let resume = if b.restart {
        0
    } else {
        progress(&s)
            .get(&e.id)
            .filter(|p| !p.played)
            .map(|p| p.position_ms)
            .unwrap_or(0)
    };
    zone.stop();
    zone.clear();
    let ids = zone.enqueue(vec![queue_item(&f, &e)], Insert::End);
    zone.play_index(0);
    if resume > 5_000 {
        zone.seek_ms(resume);
    }
    Ok(Json(Enqueued { ids }))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetProgress {
    #[serde(default)]
    pub position_ms: Option<u64>,
    #[serde(default)]
    pub played: Option<bool>,
}

/// `PUT /v1/podcasts/progress/{episode} {position_ms?, played?}`: mark played or set the position.
pub(crate) async fn set_progress(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(b): Json<SetProgress>,
) -> ApiResult<Progress> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    find_episode(&s, &id).ok_or_else(|| ApiError::not_found(format!("no episode {id}")))?;
    let mut prog = progress(&s);
    let p = prog.entry(id).or_default();
    if let Some(ms) = b.position_ms {
        p.position_ms = ms;
    }
    if let Some(played) = b.played {
        p.played = played;
        if !played && b.position_ms.is_none() {
            p.position_ms = 0;
        }
    }
    p.updated = now();
    let out = *p;
    store(&s, PROGRESS, &prog)?;
    Ok(Json(out))
}

/// Record positions of episodes playing on any zone, and refresh feeds now and then.
pub fn spawn_background(s: &Arc<NodeState>) {
    let weak = Arc::downgrade(s);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(PROGRESS_EVERY);
        let mut last_refresh = std::time::Instant::now();
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { return };
            if !s.source_enabled(SOURCE_ID) {
                continue;
            }
            record_positions(&s);
            if last_refresh.elapsed() >= REFRESH_EVERY && !feeds(&s).is_empty() {
                last_refresh = std::time::Instant::now();
                refresh_all(&s).await;
            }
        }
    });
}

fn record_positions(s: &NodeState) {
    let playing: Vec<(String, u64, Option<u64>)> = s
        .zones
        .list()
        .into_iter()
        .filter_map(|z| {
            let np = z.now_playing?;
            Some((
                np.item.uri,
                np.position_ms,
                np.duration_ms.or(np.item.duration_ms),
            ))
        })
        .collect();
    if playing.is_empty() {
        return;
    }
    let all = feeds(s);
    let by_url: HashMap<&str, &Episode> = all
        .iter()
        .flat_map(|f| f.episodes.iter().map(|e| (e.url.as_str(), e)))
        .collect();
    let mut prog = progress(s);
    let mut changed = false;
    for (uri, pos, dur) in playing {
        let Some(e) = by_url.get(uri.as_str()) else {
            continue;
        };
        let p = prog.entry(e.id.clone()).or_default();
        let played = dur
            .or(e.duration_ms)
            .is_some_and(|d| d > 0 && pos * 100 >= d * 95);
        if p.position_ms != pos || (played && !p.played) {
            p.position_ms = pos;
            p.played |= played;
            p.updated = now();
            changed = true;
        }
    }
    if changed {
        if let Err(e) = store(s, PROGRESS, &prog) {
            tracing::debug!("podcast progress not saved: {}", e.message);
        }
    }
}

// ---- as a source ----

pub struct Podcasts;

#[async_trait::async_trait]
impl StreamingService for Podcasts {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let n = feeds(s).len();
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Podcasts".into(),
            description: if n == 0 {
                "Any podcast with an RSS feed: search, subscribe, resume where you left off; no account"
                    .into()
            } else {
                format!(
                    "{n} subscription{}; any podcast with an RSS feed, no account",
                    if n == 1 { "" } else { "s" }
                )
            },
            status: "ready".into(),
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        Ok(search_shows(s, term, limit)
            .await?
            .into_iter()
            .map(|sh| SourceHit {
                id: sh.id.clone(),
                title: sh.title,
                creator: sh.author,
                year: None,
                downloads: None,
                image: sh.image,
                kind: "item".into(),
                duration_ms: None,
                library_artist_id: None,
                isrc: None,
                upc: None,
                library_track_id: None,
                library_album_id: None,
                uri: None,
                url: sh.links.first().map(|l| l.uri.clone()).unwrap_or(sh.feed),
            })
            .collect())
    }

    /// A show (a subscription id, or `feed:…` from search) with its episodes as tracks.
    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let f = match feeds(s).into_iter().find(|f| f.id == id) {
            Some(f) => f,
            None => {
                let url =
                    feed_from_show_id(id).ok_or_else(|| ApiError::not_found(format!("no podcast {id}")))?;
                match feeds(s).into_iter().find(|f| f.url == url) {
                    Some(f) => f,
                    None => fetch_feed(s, &url).await?,
                }
            }
        };
        Ok(SourceItem {
            source: SOURCE_ID.into(),
            id: f.id.clone(),
            title: f.title.clone(),
            creator: f.author.clone(),
            year: None,
            description: f.description.clone(),
            licence: None,
            image: f.image.clone(),
            url: f.link.clone().unwrap_or_else(|| f.url.clone()),
            tracks: f
                .episodes
                .iter()
                .take(100)
                .map(|e| SourceTrack {
                    title: e.title.clone(),
                    track_no: None,
                    duration_ms: e.duration_ms,
                    format: e.mime.clone().unwrap_or_else(|| "audio".into()),
                    url: e.url.clone(),
                    artist: f.author.clone(),
                })
                .collect(),
            library_album_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" xmlns:content="http://purl.org/rss/1.0/modules/content/">
<channel>
  <title>Test &amp; Show</title>
  <link>https://show.example/</link>
  <description><![CDATA[<p>A <b>show</b> about things.</p>]]></description>
  <itunes:author>Jane Host</itunes:author>
  <itunes:image href="https://show.example/cover.jpg"/>
  <item>
    <title>Episode 1</title>
    <guid isPermaLink="false">ep-1</guid>
    <pubDate>Mon, 05 Oct 2026 08:00:00 +0000</pubDate>
    <enclosure url="https://cdn.example/ep1.mp3" length="123" type="audio/mpeg"/>
    <itunes:duration>1:02:03</itunes:duration>
  </item>
  <item>
    <title>Episode 2</title>
    <guid>ep-2</guid>
    <pubDate>Mon, 12 Oct 2026 08:00:00 +0000</pubDate>
    <enclosure url="https://cdn.example/ep2.m4a" type="audio/mp4"/>
    <itunes:duration>2730</itunes:duration>
    <itunes:image href="https://show.example/ep2.jpg"/>
    <description>Second &lt;i&gt;one&lt;/i&gt;</description>
  </item>
  <item><title>No audio</title><guid>x</guid></item>
</channel></rss>"#;

    #[test]
    fn rss_feeds_parse() {
        let f = parse_feed("https://show.example/feed.xml", FEED).unwrap();
        assert_eq!(f.title, "Test & Show");
        assert_eq!(f.author.as_deref(), Some("Jane Host"));
        assert_eq!(f.image.as_deref(), Some("https://show.example/cover.jpg"));
        assert_eq!(f.description.as_deref(), Some("A show about things."));
        assert_eq!(f.episodes.len(), 2, "the item without audio is left out");
        let (e2, e1) = (&f.episodes[0], &f.episodes[1]);
        assert_eq!(e2.title, "Episode 2", "newest first");
        assert_eq!(e2.duration_ms, Some(2_730_000));
        assert_eq!(e2.image.as_deref(), Some("https://show.example/ep2.jpg"));
        assert_eq!(e2.description.as_deref(), Some("Second one"));
        assert_eq!(e1.duration_ms, Some(3_723_000));
        assert_eq!(e1.mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(e1.published, Some(1_791_187_200));
        assert_eq!(
            e1.id,
            episode_id(&feed_id("https://show.example/feed.xml"), "ep-1")
        );
        assert!(parse_feed("https://x", "<html><body>no</body></html>").is_err());
    }

    #[test]
    fn search_results_become_shows_with_feeds() {
        let pinakes = serde_json::json!({"items": [
            {"uri": "https://podcasts.apple.com/us/podcast/x/id1", "type": "PodcastSeries", "name": "Show X", "site": "Apple Podcasts",
             "image": "https://img/x.jpg", "group": {"id": "g1"},
             "also_on": [{"uri": "https://open.spotify.com/show/1", "site": "Spotify"},
                         {"uri": "https://feeds.example/x.xml", "site": "feeds.example"},
                         {"uri": "https://castro.fm/itunes/1", "site": "Castro", "derived": true}]},
            {"uri": "https://open.spotify.com/show/1", "type": "PodcastSeries", "name": "Show X", "group": {"id": "g1"}, "also_on": []},
            {"uri": "https://podcasts.apple.com/us/podcast/y/id2", "type": "PodcastSeries", "name": "No feed", "group": {"id": "g2"}, "also_on": []}
        ]});
        let shows = shows_from_pinakes(&pinakes);
        assert_eq!(shows.len(), 1, "one per group, and only shows with a feed");
        assert_eq!(shows[0].feed, "https://feeds.example/x.xml");
        assert_eq!(shows[0].links.len(), 3, "Apple Podcasts, Spotify, Castro");
        assert_eq!(
            feed_from_show_id(&shows[0].id).as_deref(),
            Some("https://feeds.example/x.xml")
        );
        let itunes = serde_json::json!({"results": [{"collectionName": "Show Y", "artistName": "Bob", "feedUrl": "https://y/rss", "artworkUrl600": "https://y.jpg", "collectionViewUrl": "https://podcasts.apple.com/y"}, {"collectionName": "No feed"}]});
        let s2 = shows_from_itunes(&itunes);
        assert_eq!(s2.len(), 1);
        assert_eq!(
            (s2[0].author.as_deref(), s2[0].feed.as_str()),
            (Some("Bob"), "https://y/rss")
        );
        assert_eq!(parse_duration("45:30"), Some(2_730_000));
        assert_eq!(parse_duration(""), None);
    }
}
