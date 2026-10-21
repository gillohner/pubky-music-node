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
    if out.error.is_none() {
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
    let items: Vec<DiscoverItem> = {
        let lib = s.library.lock().expect("library poisoned");
        page.items
            .iter()
            .filter(|c| c.is_ok())
            .filter(|c| !music_only || is_music(&kind_of_card(c)))
            .take(limit)
            .map(|c| card_item(&acc, &lib, c))
            .collect()
    };
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
