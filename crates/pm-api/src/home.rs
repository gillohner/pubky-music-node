//! Home: one page for everything Pinakes knows about music (owner,
//! 2026-10-21; STEWARD-UX-HOME.md). Each section is one Pinakes call, cached
//! sixty seconds, answering items, or a one-line reason it is empty, or its
//! own error; the UI renders each as it arrives.

use crate::discover::{card_item, kind_of_card, service_access};
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::Json;
use pm_pubky::pinakes::SearchQuery;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
const LOCATION_KEY: &str = "home.location";
/// Concerts within this distance of the owner's city.
const CONCERT_RADIUS_M: u32 = 50_000;

/// One Home section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HomeSection {
    pub section: String,
    #[serde(default)]
    pub items: Vec<DiscoverItem>,
    /// Tag labels with counts (the tags section, and co-tags on a tag page).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<(String, i64)>,
    /// Why it is empty, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// What it needs first: `pubky` (a sign-in) or `location` (a city).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HomeQuery {
    /// `24h`, `7d`, `30d` for trending sections.
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Tag page: comma-separated labels, all must match.
    #[serde(default)]
    pub labels: Option<String>,
}

/// The owner's city for concerts.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HomeLocation {
    pub label: String,
    pub lat: f64,
    pub lon: f64,
}

fn cache() -> &'static Mutex<HashMap<String, (Instant, HomeSection)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, HomeSection)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The Home sections, in page order.
pub const SECTIONS: [&str; 9] = [
    "trending-songs",
    "trending-albums",
    "trending-artists",
    "following",
    "tags",
    "concerts",
    "podcasts",
    "yours",
    "new",
];

fn is_music(kind: &str) -> bool {
    matches!(kind, "track" | "album" | "artist" | "playlist")
}

/// The owner's key as Pinakes takes it for `author`/`viewer`: bare z-base-32,
/// without the `pubky` prefix the node's identity carries.
fn owner(s: &NodeState) -> Option<String> {
    s.sync
        .lock()
        .expect("sync lock")
        .as_ref()
        .map(|x| x.z32().trim_start_matches("pubky").to_string())
}

pub(crate) fn location(s: &NodeState) -> Option<HomeLocation> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(LOCATION_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

/// `GET /v1/home/{section}`.
pub(crate) async fn section(
    State(s): State<Arc<NodeState>>,
    Path(name): Path<String>,
    Query(q): Query<HomeQuery>,
) -> ApiResult<HomeSection> {
    if !SECTIONS.contains(&name.as_str()) && name != "tag" {
        return Err(ApiError::not_found(format!("no Home section {name}")));
    }
    let key = format!("{name}|{:?}|{:?}|{:?}", q.window, q.limit, q.labels);
    if let Some((at, c)) = cache().lock().expect("home cache").get(&key) {
        if at.elapsed() < TTL {
            return Ok(Json(c.clone()));
        }
    }
    let mut out = build(&s, &name, &q).await;
    out.section = name;
    // "Needs a contact / sign-in" is not kept: it changes the moment the owner acts
    if out.error.is_none() && out.needs.is_none() {
        cache()
            .lock()
            .expect("home cache")
            .insert(key, (Instant::now(), out.clone()));
    }
    Ok(Json(out))
}

fn note(n: &str) -> HomeSection {
    HomeSection {
        note: Some(n.into()),
        ..Default::default()
    }
}

async fn build(s: &Arc<NodeState>, name: &str, q: &HomeQuery) -> HomeSection {
    let Some(d) = s.discovery() else {
        return HomeSection {
            needs: Some("contact".into()),
            note: Some("Set the upstream contact in Settings to see what Pubky knows.".into()),
            ..Default::default()
        };
    };
    let limit = q.limit.unwrap_or(12).clamp(1, 60);
    let window = q.window.clone().unwrap_or_else(|| "7d".into());
    let trending = |t: &str| SearchQuery {
        limit: Some(limit),
        ..SearchQuery::trending(t, &window)
    };
    let me = owner(s);
    let (query, empty_line, music_only) = match name {
        "trending-songs" => (
            trending("MusicRecording"),
            "No songs were tagged in this window.",
            false,
        ),
        "trending-albums" => (
            trending("MusicAlbum"),
            "No albums were tagged in this window.",
            false,
        ),
        "trending-artists" => (
            trending("MusicGroup"),
            "No artists were tagged in this window.",
            false,
        ),
        "podcasts" => (
            trending("PodcastSeries"),
            "No podcasts were tagged in this window.",
            false,
        ),
        "following" => {
            let Some(me) = me.clone() else {
                return HomeSection {
                    needs: Some("pubky".into()),
                    note: Some("Sign in with Pubky Ring to see what people you follow tag.".into()),
                    ..Default::default()
                };
            };
            (
                SearchQuery {
                    reach: Some("following".into()),
                    viewer: Some(me),
                    sort: Some("recent".into()),
                    limit: Some(limit * 3),
                    ..Default::default()
                },
                "Nobody you follow has tagged music lately.",
                true,
            )
        }
        "yours" => {
            let Some(me) = me.clone() else {
                return HomeSection {
                    needs: Some("pubky".into()),
                    note: Some("Sign in with Pubky Ring to see what you tagged.".into()),
                    ..Default::default()
                };
            };
            (
                SearchQuery {
                    author: Some(me),
                    sort: Some("recent".into()),
                    limit: Some(limit * 3),
                    ..Default::default()
                },
                "You have not tagged music on Pubky yet.",
                true,
            )
        }
        "new" => (
            SearchQuery {
                sort: Some("recent".into()),
                limit: Some(limit * 3),
                ..Default::default()
            },
            "Nothing new on Pubky yet.",
            true,
        ),
        "tags" => (
            SearchQuery {
                type_: Some("MusicRecording".into()),
                facet: Some("tags".into()),
                limit: Some(1),
                ..Default::default()
            },
            "Nobody has tagged music yet.",
            false,
        ),
        "concerts" => {
            let Some(loc) = location(s) else {
                return HomeSection {
                    needs: Some("location".into()),
                    note: Some("Set your city to see concerts near you.".into()),
                    ..Default::default()
                };
            };
            (
                SearchQuery {
                    type_: Some("MusicEvent".into()),
                    near: Some(format!("{:.4},{:.4}", loc.lat, loc.lon)),
                    radius: Some(CONCERT_RADIUS_M.to_string()),
                    from: Some("now".into()),
                    to: Some("now+30d".into()),
                    sort: Some("start".into()),
                    limit: Some(limit),
                    ..Default::default()
                },
                "No concerts near you in the next 30 days.",
                false,
            )
        }
        _ => {
            // A tag page: all labels must match; co-tags from the filtered set.
            let labels = q.labels.clone().unwrap_or_default();
            if labels.trim().is_empty() {
                return note("No tag given.");
            }
            (
                SearchQuery {
                    tags: Some(labels),
                    match_: Some("all".into()),
                    facet: Some("tags".into()),
                    collapse: Some("group".into()),
                    limit: Some(limit),
                    ..Default::default()
                },
                "Nothing carries all of these tags yet.",
                false,
            )
        }
    };
    let page = match d.pinakes().search(&query).await {
        Ok(p) => p,
        Err(e) => {
            return HomeSection {
                error: Some(e.to_string()),
                ..Default::default()
            }
        }
    };
    let tags: Vec<(String, i64)> = page
        .facets
        .as_ref()
        .map(|f| f.tags.iter().map(|t| (t.label.clone(), t.count)).collect())
        .unwrap_or_default();
    if name == "tags" {
        return HomeSection {
            note: tags.is_empty().then(|| empty_line.to_string()),
            tags: tags.into_iter().take(40).collect(),
            ..Default::default()
        };
    }
    let acc = service_access(s);
    let listed: Vec<(Option<String>, DiscoverItem)> = {
        let lib = s.library.lock().expect("library poisoned");
        page.items
            .iter()
            .filter(|c| c.is_ok())
            .filter(|c| !music_only || is_music(&kind_of_card(c)))
            .take(limit)
            .map(|c| {
                let id = c
                    .mbid
                    .clone()
                    .or_else(|| c.isrc.as_ref().map(|i| i.to_ascii_uppercase()));
                (id, card_item(&acc, &lib, c))
            })
            .collect()
    };
    let mut items = merge_same(listed);
    name_placeholders(s, &mut items).await;
    HomeSection {
        note: items.is_empty().then(|| empty_line.to_string()),
        items,
        tags: if name == "tag" { tags } else { Vec::new() },
        ..Default::default()
    }
}

/// `GET /v1/home/location`.
pub(crate) async fn get_location(State(s): State<Arc<NodeState>>) -> ApiResult<Option<HomeLocation>> {
    Ok(Json(location(&s)))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetLocation {
    /// A city, looked up once on OpenStreetMap (Nominatim).
    #[serde(default)]
    pub city: Option<String>,
    /// Or exact coordinates.
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
}

/// `PUT /v1/home/location {city}` (or `{lat, lon}`); `{}` clears it.
pub(crate) async fn set_location(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<SetLocation>,
) -> ApiResult<Option<HomeLocation>> {
    let loc = match (
        body.lat,
        body.lon,
        body.city.as_deref().map(str::trim).filter(|c| !c.is_empty()),
    ) {
        (Some(lat), Some(lon), city) => Some(HomeLocation {
            label: city.unwrap_or("").to_string(),
            lat,
            lon,
        }),
        (_, _, Some(city)) => Some(geocode(&s, city).await?),
        _ => None,
    };
    {
        let lib = s.library.lock().expect("library poisoned");
        match &loc {
            Some(l) => lib.set_setting(LOCATION_KEY, &serde_json::to_string(l).expect("serializes"))?,
            None => {
                lib.delete_setting(LOCATION_KEY)?;
            }
        }
    }
    cache()
        .lock()
        .expect("home cache")
        .retain(|k, _| !k.starts_with("concerts|"));
    Ok(Json(loc))
}

/// A city to coordinates, once, through OpenStreetMap's Nominatim (its usage
/// policy: an identifying User-Agent, one request at a time).
async fn geocode(s: &NodeState, city: &str) -> Result<HomeLocation, ApiError> {
    let base = s.nominatim_base.read().expect("nominatim lock").clone();
    let resp = s
        .http_client()
        .get(format!("{base}/search"))
        .query(&[("q", city), ("format", "jsonv2"), ("limit", "1")])
        .header(reqwest::header::USER_AGENT, s.user_agent())
        .send()
        .await
        .map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, format!("OpenStreetMap: {e}")))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, format!("OpenStreetMap: {e}")))?;
    let first = v
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| ApiError::bad_request(format!("OpenStreetMap does not know “{city}”")))?;
    let num = |k: &str| first[k].as_str().and_then(|x| x.parse::<f64>().ok());
    Ok(HomeLocation {
        label: first["name"].as_str().unwrap_or(city).to_string(),
        lat: num("lat").ok_or_else(|| ApiError::bad_request("no coordinates"))?,
        lon: num("lon").ok_or_else(|| ApiError::bad_request("no coordinates"))?,
    })
}

/// What Pinakes ships beyond search (related items), checked from its
/// `/v1/info` limits and cached ten minutes.
pub(crate) async fn pinakes_has_related(s: &NodeState) -> bool {
    static C: OnceLock<Mutex<Option<(Instant, bool)>>> = OnceLock::new();
    let c = C.get_or_init(Default::default);
    if let Some((at, v)) = *c.lock().expect("caps lock") {
        if at.elapsed() < Duration::from_secs(600) {
            return v;
        }
    }
    let Some(d) = s.discovery() else { return false };
    let v = match d.pinakes().info_raw().await {
        Ok(info) => info
            .get("limits")
            .and_then(|l| l.as_object().map(|o| o.keys().any(|k| k.starts_with("related"))))
            .unwrap_or(false),
        Err(_) => false,
    };
    *c.lock().expect("caps lock") = Some((Instant::now(), v));
    v
}

/// A title as people compare songs: no "(Remastered 2011)", "- Live", "| Music Video".
pub(crate) fn clean_title(t: &str) -> String {
    let mut t = t.to_lowercase();
    if let Some((a, _)) = t.split_once(" | ") {
        t = a.to_string();
    }
    // Bracketed asides
    let mut out = String::new();
    let mut depth = 0;
    for ch in t.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    let words = [
        "remaster", "live", "version", "edit", "mix", "mono", "stereo", "video", "official", "single",
        "radio", "demo",
    ];
    if let Some((a, b)) = out.split_once(" - ") {
        if words.iter().any(|w| b.contains(w)) {
            out = a.to_string();
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The same song listed more than once (in two Pinakes groups, as a remaster, with and without
/// its artist) shows once, with the others counted in `versions` (owner's Discover finding).
/// Same MusicBrainz id or ISRC, else the same cleaned title and artist; a listing without an
/// artist joins the one with the same title that has one.
pub(crate) fn merge_same(listed: Vec<(Option<String>, DiscoverItem)>) -> Vec<DiscoverItem> {
    let mut out: Vec<(Option<String>, String, Option<String>, DiscoverItem)> = Vec::new();
    for (id, it) in listed {
        let title = it.title.as_deref().map(clean_title).unwrap_or_default();
        let artist = it
            .artist
            .as_deref()
            .map(|a| a.to_lowercase().trim().to_string())
            .filter(|a| !a.is_empty());
        let same = out.iter().position(|(oid, ot, oa, _)| {
            (id.is_some() && *oid == id)
                || (!title.is_empty()
                    && *ot == title
                    && match (&artist, oa) {
                        (Some(a), Some(b)) => a == b,
                        _ => true,
                    })
        });
        match same {
            Some(k) => {
                let kept = &mut out[k];
                kept.3.versions += 1 + it.versions;
                // Show the plain title ("Bohemian Rhapsody", not "… - Remastered 2011")
                let plain = |t: &Option<String>| {
                    t.as_deref().is_some_and(|t| {
                        t.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ") == clean_title(t)
                    })
                };
                if !plain(&kept.3.title) && plain(&it.title) {
                    kept.3.title = it.title.clone();
                }
                if kept.2.is_none() && artist.is_some() {
                    kept.2 = artist;
                    kept.3.artist = it.artist.clone();
                }
                if kept.3.image.is_none() {
                    kept.3.image = it.image.clone();
                }
                if kept.0.is_none() {
                    kept.0 = id;
                }
            }
            None => out.push((id, title, artist, it)),
        }
    }
    out.into_iter().map(|(_, _, _, it)| it).collect()
}

/// Listings Pinakes names only by a placeholder ("ISRC NLS161600475", or nothing): MusicBrainz
/// names them (a few per section; the answers are cached).
async fn name_placeholders(s: &Arc<NodeState>, items: &mut [DiscoverItem]) {
    let mut asked = 0;
    for it in items.iter_mut() {
        let placeholder = it
            .title
            .as_deref()
            .is_none_or(|t| t.starts_with("ISRC ") && t.len() == 17);
        if !placeholder || asked >= 5 {
            continue;
        }
        let page = it.page.clone().unwrap_or_else(|| it.uri.clone());
        let recording = match page.strip_prefix("https://musicbrainz.org/isrc/") {
            Some(isrc) => {
                asked += 1;
                crate::discover::mb_recording_for_isrc(s, isrc.trim_end_matches('/')).await
            }
            None if page.starts_with("https://musicbrainz.org/recording/") => {
                asked += 1;
                crate::discover::mb_recording(s, &page).await
            }
            // Another link of the same song that plays here can name it (a TIDAL track)
            None => {
                if let Some(url) = it.plays_from.as_ref().and_then(|p| p.url.clone()) {
                    asked += 1;
                    if let Some(named) = crate::discover::describe_from_provider(s, &url).await {
                        it.title = named.title;
                        it.artist = it.artist.take().or(named.artist);
                        it.image = it.image.take().or(named.image);
                    }
                }
                continue;
            }
        };
        if let Some(m) = recording {
            it.title = Some(m.title);
            if it.artist.is_none() {
                it.artist = Some(m.artist).filter(|a| !a.is_empty());
            }
            continue;
        }
        // MusicBrainz does not know the ISRC: Deezer's public catalogue may, and its track names itself
        if let Some(isrc) = page.strip_prefix("https://musicbrainz.org/isrc/") {
            let link = crate::deezer::isrc_versions(s, isrc.trim_end_matches('/'))
                .await
                .ok()
                .and_then(|v| v.into_iter().next());
            if let Some(named) = match link {
                Some(v) => crate::discover::describe_from_provider(s, &v.url).await,
                None => None,
            } {
                it.title = named.title;
                it.artist = it.artist.take().or(named.artist);
                it.image = it.image.take().or(named.image);
                continue;
            }
        }
        if it.title.as_deref().is_some_and(|t| t.starts_with("ISRC ")) {
            it.title = None;
        }
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn item(title: Option<&str>, artist: Option<&str>) -> DiscoverItem {
        DiscoverItem {
            uri: format!("https://x/{}", title.unwrap_or("")),
            title: title.map(Into::into),
            artist: artist.map(Into::into),
            description: None,
            image: None,
            taggers: None,
            in_library: None,
            kind: "track".into(),
            site: None,
            pinakes_id: None,
            group_size: None,
            playable: None,
            also_on: Vec::new(),
            page: None,
            plays_from: None,
            connect: Vec::new(),
            versions: 0,
        }
    }

    #[test]
    fn the_same_song_shows_once() {
        assert_eq!(
            clean_title("Bohemian Rhapsody - Remastered 2011"),
            "bohemian rhapsody"
        );
        assert_eq!(
            clean_title("Cut To The Feeling - Carly Rae Jepsen | Music Video"),
            "cut to the feeling - carly rae jepsen"
        );
        assert_eq!(clean_title("Get Lucky (feat. Pharrell Williams)"), "get lucky");
        assert_eq!(
            clean_title("Love - Hate"),
            "love - hate",
            "a dash that is part of the title stays"
        );
        let merged = merge_same(vec![
            (None, item(Some("Bohemian Rhapsody - Remastered 2011"), None)),
            (
                Some("b1a9".into()),
                item(Some("Bohemian Rhapsody"), Some("Queen")),
            ),
            (None, item(Some("Bohemian Rhapsody"), Some("Queen"))),
            (
                Some("b1a9".into()),
                item(Some("Bohemian Rhapsody (Live Aid)"), Some("Queen")),
            ),
            (None, item(Some("Bohemian Rhapsody"), Some("Panic! At The Disco"))),
            (None, item(Some("Das Boot"), Some("Sefa"))),
            (None, item(Some("Das Boot"), Some("Sefa"))),
        ]);
        let shown: Vec<(Option<&str>, Option<&str>, u32)> = merged
            .iter()
            .map(|i| (i.title.as_deref(), i.artist.as_deref(), i.versions))
            .collect();
        assert_eq!(
            shown,
            vec![
                (Some("Bohemian Rhapsody"), Some("Queen"), 3),
                (Some("Bohemian Rhapsody"), Some("Panic! At The Disco"), 0),
                (Some("Das Boot"), Some("Sefa"), 1),
            ]
        );
    }
}
