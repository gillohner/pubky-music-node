//! Discovery (M10 groundwork): Pinakes trends, tags and friends' activity,
//! each item joined to the library by its canonical URL so the controller
//! can play what the user owns and link out for the rest.

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::Library;
use pm_model::MusicUri;
use pm_pubky::{DiscoveredItem, Discovery};
use std::sync::Arc;

fn discovery(s: &NodeState) -> Result<Arc<Discovery>, ApiError> {
    s.discovery().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "discovery is off: set the upstream contact (an email or URL) in Settings or as `contact` in the node config",
        )
    })
}

/// `MusicRecording` → `track`, `MusicAlbum` → `album`, `MusicGroup`/`Person` → `artist`.
fn kind_of(card: &pm_pubky::pinakes::Card) -> String {
    match card.type_.as_deref() {
        Some("MusicRecording" | "MusicComposition") => "track",
        Some("MusicAlbum" | "MusicRelease") => "album",
        Some("MusicGroup" | "Person") => "artist",
        Some("MusicPlaylist") => "playlist",
        _ => "other",
    }
    .into()
}

fn site_slug(site: &str) -> String {
    site.to_lowercase().replace(' ', "-")
}

/// `also_on` entries as Versions rows (tagged first, playable when the node streams the service).
fn also_on_rows(links: &[pm_pubky::pinakes::AlsoOn]) -> Vec<Version> {
    let mut v: Vec<&pm_pubky::pinakes::AlsoOn> = links.iter().collect();
    v.sort_by_key(|a| !a.is_tagged());
    v.into_iter()
        .map(|a| {
            let site = a.site.clone().unwrap_or_else(|| "link".into());
            Version {
                source: site_slug(&site),
                id: a.id.clone().unwrap_or_default(),
                label: if a.is_tagged() {
                    format!("{site} · tagged on Pubky")
                } else {
                    format!(
                        "{site} · via {}",
                        a.via.clone().unwrap_or_else(|| "Pinakes".into())
                    )
                },
                url: a.uri.clone(),
                uri: playable_uri(&a.uri),
            }
        })
        .collect()
}

fn item(lib: &Library, d: &DiscoveredItem) -> DiscoverItem {
    let in_library = lib
        .tracks_by_canonical(&d.uri)
        .ok()
        .and_then(|v| v.into_iter().next());
    DiscoverItem {
        uri: d.uri.to_url(),
        title: d.card.name.clone(),
        artist: d.card.artist_hint(),
        description: d.card.description.clone(),
        image: d.card.image.clone(),
        taggers: d.taggers,
        in_library,
        kind: kind_of(&d.card),
        site: d.card.site.clone(),
        pinakes_id: Some(d.card.id.clone()),
        group_size: d.card.group.as_ref().map(|g| g.size),
        playable: playable_uri(&d.card.uri),
        also_on: also_on_rows(&d.card.also_on),
    }
}

fn chart_type(t: Option<&str>) -> &'static str {
    match t {
        Some("albums") => "MusicAlbum",
        Some("artists") => "MusicGroup",
        _ => "MusicRecording",
    }
}

/// `GET /v1/discover/charts?type=tracks|albums&window=24h|7d|30d`: Pinakes's
/// chart, one song across services one row.
pub(crate) async fn charts(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<DiscoverPage> {
    let d = discovery(&s)?;
    let limit = q.limit.unwrap_or(30).min(200);
    let window = q.window.clone().unwrap_or_else(|| "7d".into());
    let p = d
        .trending_of(chart_type(q.type_.as_deref()), &window, limit)
        .await
        .map_err(upstream)?;
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, Vec::new(), &p.items)))
}

/// `GET /v1/discover/search?q=&type=`: Pinakes's text search over music
/// cards (tracks by default), each joined to the library.
pub(crate) async fn search(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<DiscoverPage> {
    let d = discovery(&s)?;
    let term = q.q.clone().unwrap_or_default();
    if term.trim().is_empty() {
        return Err(ApiError::bad_request("q is required"));
    }
    let limit = q.limit.unwrap_or(20).min(100);
    let type_ = match q.type_.as_deref() {
        Some("albums") => Some("MusicAlbum".to_string()),
        Some("artists") => Some("MusicGroup".to_string()),
        Some("tracks") => Some("MusicRecording".to_string()),
        _ => None,
    };
    let resp = d
        .pinakes()
        .search(&pm_pubky::pinakes::SearchQuery {
            q: Some(term.trim().to_string()),
            type_,
            limit: Some(limit),
            ..Default::default()
        })
        .await
        .map_err(upstream)?;
    let cards: Vec<pm_pubky::pinakes::Card> = resp.items.into_iter().filter(|c| c.is_music()).collect();
    let items = pm_pubky::discovery::items_from_cards(cards);
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, Vec::new(), &items)))
}

/// `GET /v1/discover/item?uri=`: one card with facts, every link, who tagged
/// it on Pubky, and what the library owns of it.
pub(crate) async fn item_detail(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<DiscoverDetail> {
    let d = discovery(&s)?;
    let Some(uri) = q.uri.clone().filter(|u| !u.trim().is_empty()) else {
        return Err(ApiError::bad_request("uri is required"));
    };
    let Some(r) = d.resource_for(uri.trim()).await.map_err(upstream)? else {
        return Err(ApiError::not_found("Pinakes has not seen this link"));
    };
    let facts = r.facts();
    let (item, in_library_album) = {
        let lib = s.library.lock().expect("library poisoned");
        let music = MusicUri::parse(&r.card.uri).ok();
        let in_library = music
            .as_ref()
            .and_then(|m| lib.tracks_by_canonical(m).ok())
            .and_then(|v| v.into_iter().next());
        let by_isrc = facts.isrcs().iter().find_map(|i| {
            let isrc = pm_model::Isrc::parse(i).ok()?;
            lib.tracks_by_canonical(&MusicUri::Isrc { isrc })
                .ok()?
                .into_iter()
                .next()
        });
        let in_library = in_library.or(by_isrc);
        let album = facts
            .upc
            .as_deref()
            .and_then(|u| pm_model::Upc::parse(u).ok())
            .and_then(|u| lib.album_by_upc(&u).ok().flatten())
            .and_then(|a| a.id)
            .or_else(|| {
                in_library
                    .as_ref()
                    .and_then(|t| t.album.as_ref().and_then(|a| a.id))
            });
        let item = DiscoverItem {
            uri: music.map(|m| m.to_url()).unwrap_or_else(|| r.card.uri.clone()),
            title: r.card.name.clone(),
            artist: facts.artist.clone().or_else(|| r.card.artist_hint()),
            description: r.card.description.clone(),
            image: r.card.image.clone(),
            taggers: None,
            in_library,
            kind: kind_of(&r.card),
            site: r.card.site.clone(),
            pinakes_id: Some(r.card.id.clone()),
            group_size: r.card.group.as_ref().map(|g| g.size),
            playable: playable_uri(&r.card.uri),
            also_on: also_on_rows(r.also_on()),
        };
        (item, album)
    };
    let on_pubky = match d.nexus().resource_by_uri(&r.card.uri).await {
        Ok(view) => {
            let mut tags: Vec<(String, u64)> = view
                .tags
                .iter()
                .map(|t| (t.label.clone(), t.taggers_count.max(t.taggers.len() as u64)))
                .collect();
            tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Some(OnPubky {
                uri: r.card.uri.clone(),
                indexed: true,
                taggers: view.distinct_taggers() as u64,
                tags,
            })
        }
        Err(_) => None,
    };
    Ok(Json(DiscoverDetail {
        item,
        mbid: r.mbid(),
        isrcs: r.isrcs(),
        upc: r.upc(),
        duration_secs: facts.duration_secs(),
        album: facts.album.clone(),
        original_album: facts.original_album.clone(),
        artist_type: facts.artist_type.clone(),
        on_pubky,
        in_library_album,
        artist_info: None,
    }))
}

fn page(lib: &Library, facets: Vec<pm_pubky::pinakes::TagFacet>, items: &[DiscoveredItem]) -> DiscoverPage {
    DiscoverPage {
        facets: facets.into_iter().map(|f| (f.label, f.count)).collect(),
        items: items.iter().map(|d| item(lib, d)).collect(),
    }
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, e.to_string())
}

/// A service page as a queue URI the node can play: TIDAL and Deezer tracks.
pub(crate) fn playable_uri(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u
        .host_str()?
        .trim_start_matches("www.")
        .trim_start_matches("listen.");
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let after = |key: &str| {
        segs.iter()
            .position(|s| *s == key)
            .and_then(|i| segs.get(i + 1))
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
            .map(|id| id.to_string())
    };
    match host {
        "tidal.com" => after("track").map(|id| crate::tidal::track_uri(&id)),
        "deezer.com" => after("track").map(|id| crate::deezer::track_uri(&id)),
        _ => None,
    }
}

/// Pinakes's links for an exact key (`mbid:<uuid>`, `isrc:<code>`,
/// `upc:<barcode>`) as "Versions" rows: every tagged page carrying the key
/// on any service, then the `also_on` links of those pages; tagged first,
/// with a queue URI when the node can stream it (TIDAL, Deezer). Pinakes
/// has no card for a MusicBrainz URL itself, so keys are the join. Empty
/// without a contact or when nobody on Pubky tagged the recording. Cached
/// a day.
pub(crate) async fn also_on_versions(s: &NodeState, key: &str, cache_key: &str) -> Vec<Version> {
    let Some(d) = s.discovery() else {
        return Vec::new();
    };
    if let Some(v) = crate::tidal::cache_get_ttl(s, cache_key, 24 * 3600) {
        return v;
    }
    let page = match d
        .pinakes()
        .search(&pm_pubky::pinakes::SearchQuery {
            limit: Some(50),
            ..pm_pubky::pinakes::SearchQuery::key(key)
        })
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(%key, "key search: {e}");
            return Vec::new();
        }
    };
    let mut out: Vec<Version> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let site_slug = |site: &str| site.to_lowercase().replace(' ', "-");
    for card in &page.items {
        if !card.is_ok() || !seen.insert(card.uri.trim_end_matches('/').to_string()) {
            continue;
        }
        let site = card.site.clone().unwrap_or_else(|| "link".into());
        out.push(Version {
            source: site_slug(&site),
            id: card.id.clone(),
            label: format!("{site} · tagged on Pubky"),
            url: card.uri.clone(),
            uri: playable_uri(&card.uri),
        });
    }
    for card in &page.items {
        for a in &card.also_on {
            if !seen.insert(a.uri.trim_end_matches('/').to_string()) {
                continue;
            }
            let site = a.site.clone().unwrap_or_else(|| "link".into());
            out.push(Version {
                source: site_slug(&site),
                id: a.id.clone().unwrap_or_default(),
                label: if a.is_tagged() {
                    format!("{site} · tagged on Pubky")
                } else {
                    format!(
                        "{site} · via {}",
                        a.via.clone().unwrap_or_else(|| "Pinakes".into())
                    )
                },
                url: a.uri.clone(),
                uri: playable_uri(&a.uri),
            });
        }
    }
    crate::tidal::cache_put(s, cache_key, &out);
    out
}

/// `GET /v1/discover/uri?uri=|track_id=|album_id=|artist_id=`: the Pubky
/// view of one object (tags and taggers from Nexus). 412 without a contact.
pub(crate) async fn on_pubky(
    State(s): State<Arc<NodeState>>,
    Query(r): Query<FavouriteRef>,
) -> ApiResult<OnPubky> {
    let d = discovery(&s)?;
    let uri = {
        let lib = s.library.lock().expect("library poisoned");
        crate::favourites::canonical_for(&lib, &r)?
    };
    let url = uri.to_url();
    match d.nexus().resource_by_uri(&url).await {
        Ok(view) => {
            let mut tags: Vec<(String, u64)> = view
                .tags
                .iter()
                .map(|t| (t.label.clone(), t.taggers_count.max(t.taggers.len() as u64)))
                .collect();
            tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(Json(OnPubky {
                uri: url,
                indexed: true,
                taggers: view.distinct_taggers() as u64,
                tags,
            }))
        }
        Err(pm_pubky::nexus::NexusError::NotFound) => Ok(Json(OnPubky {
            uri: url,
            indexed: false,
            tags: Vec::new(),
            taggers: 0,
        })),
        Err(e) => Err(upstream(e)),
    }
}

/// Trending music in a window (`1h`..`90d`); falls back to all-time
/// activity with tag facets when the upstream does not support windows yet.
pub(crate) async fn trends(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<DiscoverPage> {
    let d = discovery(&s)?;
    let limit = q.limit.unwrap_or(30).min(200);
    let window = q.window.clone().unwrap_or_else(|| "7d".into());
    let (facets, items) = match d.trending(&window, limit).await {
        Ok(p) => (Vec::new(), p.items),
        Err(_) => d.trends(limit).await.map_err(upstream)?,
    };
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, facets, &items)))
}

pub(crate) async fn by_tag(
    State(s): State<Arc<NodeState>>,
    Path(label): Path<String>,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<DiscoverPage> {
    let d = discovery(&s)?;
    let p = d
        .by_tag(&label, q.limit.unwrap_or(30).min(200), q.cursor.clone())
        .await
        .map_err(upstream)?;
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, Vec::new(), &p.items)))
}

pub(crate) async fn friends(
    State(s): State<Arc<NodeState>>,
    Path(viewer): Path<String>,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<DiscoverPage> {
    let d = discovery(&s)?;
    let p = d
        .friends_activity(&viewer, q.limit.unwrap_or(30).min(200), q.cursor.clone())
        .await
        .map_err(upstream)?;
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, Vec::new(), &p.items)))
}
