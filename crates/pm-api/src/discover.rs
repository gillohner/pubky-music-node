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

fn item(acc: &ServiceAccess, lib: &Library, d: &DiscoveredItem) -> DiscoverItem {
    let in_library = lib
        .tracks_by_canonical(&d.uri)
        .ok()
        .and_then(|v| v.into_iter().next());
    let mut it = DiscoverItem {
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
        page: Some(d.card.uri.clone()),
        plays_from: None,
        connect: Vec::new(),
    };
    let r = resolve(acc, &it, None);
    it.plays_from = r.plays_from;
    it.connect = r.connect;
    it
}

// ---- playback resolution (steward UX brief §3) ----

/// A service page the node understands: `tidal.com/track/1`, `tidal.com/album/2`,
/// `deezer.com/album/3`, `open.spotify.com/track/4`, ….
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceLink {
    pub service: &'static str,
    pub kind: &'static str,
    pub id: String,
    pub url: String,
    pub tagged: bool,
}

pub(crate) fn service_link(url: &str, tagged: bool) -> Option<ServiceLink> {
    let u = url::Url::parse(url).ok()?;
    let host = u
        .host_str()?
        .trim_start_matches("www.")
        .trim_start_matches("listen.")
        .trim_start_matches("open.");
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let service = match host {
        "tidal.com" => "tidal",
        "deezer.com" => "deezer",
        "spotify.com" => "spotify",
        _ => return None,
    };
    // Deezer pages carry a language segment (`/en/track/1`); TIDAL a `/browse/`.
    let i = segs.iter().position(|s| *s == "track" || *s == "album")?;
    let kind = if segs[i] == "track" { "track" } else { "album" };
    let id = segs.get(i + 1)?.to_string();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ServiceLink {
        service,
        kind,
        id,
        url: url.to_string(),
        tagged,
    })
}

/// How this node can use a service right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// Streams full tracks into the zone.
    Full,
    /// Plays on the service's own device (Spotify Connect).
    External,
    /// 30 s excerpts only.
    Preview,
    /// Not connected: an account or credentials are needed.
    No,
}

/// What each service can do for this node right now. Computed once, before
/// any library lock: the session loaders take that lock themselves.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ServiceAccess {
    tidal: Access,
    spotify: Access,
}

pub(crate) fn service_access(s: &NodeState) -> ServiceAccess {
    ServiceAccess {
        tidal: if crate::tidal::connected(s) {
            Access::Full
        } else {
            Access::No
        },
        spotify: if crate::spotify::load_user(s).is_some() {
            Access::External
        } else {
            Access::No
        },
    }
}

fn access(acc: &ServiceAccess, service: &str) -> Access {
    match service {
        "tidal" => acc.tidal,
        "deezer" => Access::Preview,
        "spotify" => acc.spotify,
        _ => Access::No,
    }
}

fn service_name(id: &str) -> &'static str {
    match id {
        "tidal" => "TIDAL",
        "deezer" => "Deezer",
        "spotify" => "Spotify",
        _ => "service",
    }
}

fn plays_from_link(l: &ServiceLink, a: Access) -> PlaysFrom {
    let name = service_name(l.service);
    PlaysFrom {
        service: l.service.to_string(),
        kind: l.kind.to_string(),
        id: format!("{}:{}", l.kind, l.id),
        label: match a {
            Access::Preview => format!("{name} · 30 s preview"),
            Access::External => format!("{name} · plays on a Connect device"),
            _ if l.tagged => format!("{name} · tagged on Pubky"),
            _ => name.to_string(),
        },
        url: Some(l.url.clone()),
        mode: match a {
            Access::Preview => "preview",
            Access::External => "connect",
            _ => "full",
        }
        .into(),
    }
}

pub(crate) struct Resolution {
    pub plays_from: Option<PlaysFrom>,
    pub link: Option<ServiceLink>,
    pub connect: Vec<String>,
    pub preview: Option<(PlaysFrom, ServiceLink)>,
}

/// Where an item plays from: the library when the recording (or album) is
/// owned, else the best service link the node can stream (tagged links
/// first, TIDAL before Spotify Connect), else nothing, with the services
/// that have it. Previews are offered separately, never as the answer.
/// No network: the links come with the card.
pub(crate) fn resolve(acc: &ServiceAccess, it: &DiscoverItem, album_id: Option<i64>) -> Resolution {
    if let Some(t) = &it.in_library {
        let fmt = t.file.format.to_string();
        return Resolution {
            plays_from: Some(PlaysFrom {
                service: "library".into(),
                kind: "track".into(),
                id: t.id.map(|i| i.0.to_string()).unwrap_or_default(),
                label: format!("Your library · {fmt}"),
                url: None,
                mode: "full".into(),
            }),
            link: None,
            connect: Vec::new(),
            preview: None,
        };
    }
    if let Some(a) = album_id {
        return Resolution {
            plays_from: Some(PlaysFrom {
                service: "library".into(),
                kind: "album".into(),
                id: a.to_string(),
                label: "Your library".into(),
                url: None,
                mode: "full".into(),
            }),
            link: None,
            connect: Vec::new(),
            preview: None,
        };
    }
    let mut links: Vec<ServiceLink> = Vec::new();
    if let Some(l) = it.page.as_deref().and_then(|p| service_link(p, true)) {
        links.push(l);
    }
    // `also_on` rows are tagged-first already; a row's url is the service page.
    for v in &it.also_on {
        if let Some(l) = service_link(&v.url, v.label.contains("tagged")) {
            if !links
                .iter()
                .any(|x| x.service == l.service && x.kind == l.kind && x.id == l.id)
            {
                links.push(l);
            }
        }
    }
    let rank = |l: &ServiceLink| match (access(acc, l.service), l.service) {
        (Access::Full, _) => 0,
        (Access::External, _) => 2,
        _ => 9,
    } * 2
        + usize::from(!l.tagged);
    let mut sorted = links.clone();
    sorted.sort_by_key(rank);
    let best = sorted
        .iter()
        .find(|l| matches!(access(acc, l.service), Access::Full | Access::External))
        .cloned();
    let preview = links
        .iter()
        .find(|l| access(acc, l.service) == Access::Preview && l.kind == "track")
        .map(|l| (plays_from_link(l, Access::Preview), l.clone()));
    let mut connect: Vec<String> = links
        .iter()
        .filter(|l| access(acc, l.service) == Access::No)
        .map(|l| l.service.to_string())
        .collect();
    connect.dedup();
    Resolution {
        plays_from: best.as_ref().map(|l| plays_from_link(l, access(acc, l.service))),
        link: best,
        connect,
        preview,
    }
}

/// `GET /v1/discover/play?uri=…`: what to queue for an item, resolved through
/// the node's connected services; albums expand to their tracks on the
/// chosen service.
pub(crate) async fn play_plan(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<PlayPlan> {
    let Some(uri) = q.uri.clone().filter(|u| !u.trim().is_empty()) else {
        return Err(ApiError::bad_request("uri is required"));
    };
    let uri = uri.trim().to_string();
    let r = match s.discovery() {
        Some(d) => d.resource_for(&uri).await.unwrap_or_else(|e| {
            tracing::debug!("play: pinakes: {e}");
            None
        }),
        None => None,
    };
    // Pinakes knows it: every link of the group. Otherwise the provider's own page.
    let (it, album_id, album_name, duration) = match &r {
        Some(r) => {
            let (it, a) = detail_item(&s, r);
            let f = r.facts();
            (it, a, f.album.clone(), f.duration_secs())
        }
        None => match describe_from_provider(&s, &uri).await {
            Some(it) => (it, None, None, None),
            None => {
                return Err(ApiError::not_found(
                    "neither Pinakes nor a connected service knows this link",
                ))
            }
        },
    };
    let res = resolve(&service_access(&s), &it, album_id);
    let mut plan = PlayPlan {
        uri: it.uri.clone(),
        plays_from: res.plays_from.clone(),
        track_ids: Vec::new(),
        items: Vec::new(),
        spotify_uris: Vec::new(),
        connect: res.connect.clone(),
        preview: res.preview.as_ref().map(|(p, _)| p.clone()),
    };
    match (&res.plays_from, &res.link) {
        (Some(p), _) if p.service == "library" => {
            if p.kind == "album" {
                let lib = s.library.lock().expect("library poisoned");
                plan.track_ids = lib
                    .album_tracks(p.id.parse().unwrap_or(0))
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|t| t.id.map(|i| i.0))
                    .collect();
            } else if let Some(id) = it.in_library.as_ref().and_then(|t| t.id) {
                plan.track_ids = vec![id.0];
            }
        }
        (Some(p), Some(l)) if p.mode == "connect" => {
            plan.spotify_uris = vec![format!("spotify:{}:{}", l.kind, l.id)];
        }
        (Some(_), Some(l)) => {
            plan.items = expand_link(&s, l, &it, album_name, duration).await?;
        }
        _ => {}
    }
    Ok(Json(plan))
}

/// `POST /v1/discover/describe {uris}`: a proper row for each music URL
/// that is not in the library (favourites, playlist items, history): title,
/// artist, cover and where it plays from. Pinakes answers in one batch; links
/// it has not seen ask the provider's own API for that id (TIDAL, Deezer,
/// Spotify). A URL is never used as a name: unknown titles stay empty.
/// Cached a day per URL.
pub(crate) async fn describe(
    State(s): State<Arc<NodeState>>,
    Json(req): Json<DescribeRequest>,
) -> ApiResult<Vec<DiscoverItem>> {
    let uris: Vec<String> = req
        .uris
        .iter()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .take(500)
        .collect();
    let now = pm_library::db::now_unix();
    let cache_key = |u: &str| format!("describe.{u}");
    let mut found: std::collections::HashMap<String, DiscoverItem> = Default::default();
    {
        let lib = s.library.lock().expect("library poisoned");
        for u in &uris {
            if let Some((at, it)) = lib
                .get_setting(&cache_key(u))
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str::<(i64, DiscoverItem)>(&j).ok())
            {
                if now - at < 86_400 {
                    found.insert(u.clone(), it);
                }
            }
        }
    }
    let missing: Vec<String> = uris.iter().filter(|u| !found.contains_key(*u)).cloned().collect();
    let mut fresh: Vec<(String, DiscoverItem)> = Vec::new();
    if !missing.is_empty() {
        if let Some(d) = s.discovery() {
            match d.pinakes().lookup(&missing).await {
                Ok(cards) => {
                    let acc = service_access(&s);
                    let lib = s.library.lock().expect("library poisoned");
                    let norm = |u: &str| u.trim_end_matches('/').replace("://www.", "://").to_lowercase();
                    for u in &missing {
                        let Some(card) = cards.iter().find(|c| norm(&c.uri) == norm(u)) else {
                            continue;
                        };
                        let Ok(uri) = MusicUri::parse(u).or_else(|_| MusicUri::parse(&card.uri)) else {
                            continue;
                        };
                        let di = DiscoveredItem {
                            uri,
                            card: card.clone(),
                            taggers: None,
                        };
                        fresh.push((u.clone(), item(&acc, &lib, &di)));
                    }
                }
                Err(e) => tracing::debug!("describe: pinakes lookup: {e}"),
            }
        }
    }
    for u in &missing {
        if fresh.iter().any(|(x, _)| x == u) {
            continue;
        }
        if let Some(it) = describe_from_provider(&s, u).await {
            fresh.push((u.clone(), it));
        }
    }
    {
        let lib = s.library.lock().expect("library poisoned");
        for (u, it) in &fresh {
            if let Ok(j) = serde_json::to_string(&(now, it)) {
                let _ = lib.set_setting(&cache_key(u), &j);
            }
        }
    }
    found.extend(fresh);
    Ok(Json(
        uris.iter()
            .map(|u| {
                found.remove(u).unwrap_or_else(|| DiscoverItem {
                    uri: u.clone(),
                    title: None,
                    artist: None,
                    description: None,
                    image: None,
                    taggers: None,
                    in_library: None,
                    kind: "other".into(),
                    site: site_of(u),
                    pinakes_id: None,
                    group_size: None,
                    playable: None,
                    also_on: Vec::new(),
                    page: Some(u.clone()),
                    plays_from: None,
                    connect: Vec::new(),
                })
            })
            .collect(),
    ))
}

/// The service a URL belongs to, as people say it.
fn site_of(u: &str) -> Option<String> {
    let host = url::Url::parse(u)
        .ok()?
        .host_str()?
        .trim_start_matches("www.")
        .to_string();
    Some(
        match host.as_str() {
            "tidal.com" | "listen.tidal.com" => "TIDAL",
            "open.spotify.com" => "Spotify",
            "deezer.com" => "Deezer",
            "music.apple.com" => "Apple Music",
            "musicbrainz.org" => "MusicBrainz",
            "youtube.com" | "music.youtube.com" => "YouTube",
            "bandcamp.com" => "Bandcamp",
            "music.amazon.com" => "Amazon Music",
            h if h.ends_with(".bandcamp.com") => "Bandcamp",
            other => return Some(other.to_string()),
        }
        .to_string(),
    )
}

/// One URL through its own provider's API (no Pinakes card yet).
async fn describe_from_provider(s: &NodeState, u: &str) -> Option<DiscoverItem> {
    use crate::streaming::StreamingService;
    let l = service_link(u, true)?;
    let acc = service_access(s);
    let svc: Box<dyn StreamingService> = match (l.service, access(&acc, l.service)) {
        ("tidal", Access::Full) => Box::new(crate::tidal::Tidal),
        ("deezer", _) => Box::new(crate::deezer::Deezer),
        ("spotify", Access::External) => Box::new(crate::spotify::Spotify),
        _ => return None,
    };
    let src = svc.item(s, &format!("{}:{}", l.kind, l.id)).await.ok()?;
    let mut it = DiscoverItem {
        uri: u.to_string(),
        title: Some(src.title.clone()).filter(|t| !t.trim().is_empty()),
        artist: src.creator.clone(),
        description: None,
        image: src.image.clone(),
        taggers: None,
        in_library: None,
        kind: l.kind.to_string(),
        site: site_of(u),
        pinakes_id: None,
        group_size: None,
        playable: playable_uri(u),
        also_on: Vec::new(),
        page: Some(u.to_string()),
        plays_from: None,
        connect: Vec::new(),
    };
    let r = resolve(&acc, &it, None);
    it.plays_from = r.plays_from;
    it.connect = r.connect;
    Some(it)
}

/// Queue items for a service link: a track straight from the card, an
/// album through the service's item (its tracks in order).
async fn expand_link(
    s: &NodeState,
    l: &ServiceLink,
    it: &DiscoverItem,
    album: Option<String>,
    duration_secs: Option<u64>,
) -> Result<Vec<pm_zones::QueueItem>, ApiError> {
    use crate::streaming::StreamingService;
    let svc: Box<dyn StreamingService> = match l.service {
        "tidal" => Box::new(crate::tidal::Tidal),
        "deezer" => Box::new(crate::deezer::Deezer),
        _ => return Ok(Vec::new()),
    };
    if l.kind == "track" {
        let uri = match l.service {
            "tidal" => crate::tidal::track_uri(&l.id),
            _ => crate::deezer::track_uri(&l.id),
        };
        return Ok(vec![pm_zones::QueueItem {
            id: 0,
            uri,
            title: it.title.clone(),
            artist: it.artist.clone(),
            album,
            duration_ms: duration_secs.map(|d| d * 1000),
            image: it.image.clone(),
        }]);
    }
    let album = svc.item(s, &format!("album:{}", l.id)).await?;
    Ok(album
        .tracks
        .iter()
        .map(|t| pm_zones::QueueItem {
            id: 0,
            uri: t.url.clone(),
            title: Some(t.title.clone()),
            artist: t.artist.clone().or_else(|| album.creator.clone()),
            album: Some(album.title.clone()),
            duration_ms: t.duration_ms,
            image: album.image.clone().or_else(|| it.image.clone()),
        })
        .collect())
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
    let acc = service_access(&s);
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&acc, &lib, Vec::new(), &p.items)))
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
    let acc = service_access(&s);
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&acc, &lib, Vec::new(), &items)))
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
    let (item, in_library_album) = detail_item(&s, &r);
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

fn page(
    acc: &ServiceAccess,
    lib: &Library,
    facets: Vec<pm_pubky::pinakes::TagFacet>,
    items: &[DiscoveredItem],
) -> DiscoverPage {
    DiscoverPage {
        facets: facets.into_iter().map(|f| (f.label, f.count)).collect(),
        items: items.iter().map(|d| item(acc, lib, d)).collect(),
    }
}

/// The item of a full card: library matches by MBID, ISRC and UPC, every
/// link of the group, and where it plays from.
fn detail_item(s: &NodeState, r: &pm_pubky::pinakes::ResourceDetail) -> (DiscoverItem, Option<i64>) {
    let facts = r.facts();
    let acc = service_access(s);
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
    let by_mbid = facts
        .mbid
        .as_deref()
        .and_then(|m| pm_model::Mbid::parse(m).ok())
        .and_then(|mbid| lib.tracks_by_canonical(&MusicUri::Recording { mbid }).ok())
        .and_then(|v| v.into_iter().next());
    let in_library = in_library.or(by_mbid).or(by_isrc);
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
    let kind = kind_of(&r.card);
    let mut item = DiscoverItem {
        uri: music.map(|m| m.to_url()).unwrap_or_else(|| r.card.uri.clone()),
        title: r.card.name.clone(),
        artist: facts.artist.clone().or_else(|| r.card.artist_hint()),
        description: r.card.description.clone(),
        image: r.card.image.clone(),
        taggers: None,
        in_library,
        kind: kind.clone(),
        site: r.card.site.clone(),
        pinakes_id: Some(r.card.id.clone()),
        group_size: r.card.group.as_ref().map(|g| g.size),
        playable: playable_uri(&r.card.uri),
        also_on: also_on_rows(r.also_on()),
        page: Some(r.card.uri.clone()),
        plays_from: None,
        connect: Vec::new(),
    };
    drop(lib);
    let res = resolve(&acc, &item, album.filter(|_| kind == "album"));
    item.plays_from = res.plays_from;
    item.connect = res.connect;
    (item, album)
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
    let acc = service_access(&s);
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&acc, &lib, facets, &items)))
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
    let acc = service_access(&s);
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&acc, &lib, Vec::new(), &p.items)))
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
    let acc = service_access(&s);
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&acc, &lib, Vec::new(), &p.items)))
}
