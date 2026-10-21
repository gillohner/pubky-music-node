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

/// `MusicRecording` → `track`, `MusicAlbum` → `album`, `MusicGroup`/`Person`
/// → `artist`, podcasts → `podcast`, `MusicEvent` → `concert`.
fn kind_of(card: &pm_pubky::pinakes::Card) -> String {
    match card.type_.as_deref() {
        Some("MusicRecording" | "MusicComposition") => "track",
        Some("MusicAlbum" | "MusicRelease") => "album",
        Some("MusicGroup" | "Person") => "artist",
        Some("MusicPlaylist") => "playlist",
        Some("PodcastSeries" | "PodcastEpisode") => "podcast",
        Some("MusicEvent") => "concert",
        _ => "other",
    }
    .into()
}

pub(crate) fn kind_of_card(card: &pm_pubky::pinakes::Card) -> String {
    kind_of(card)
}

/// A row for any card: music links through the library and the resolver,
/// podcasts and concerts as they are (they open their own page).
pub(crate) fn card_item(acc: &ServiceAccess, lib: &Library, c: &pm_pubky::pinakes::Card) -> DiscoverItem {
    if let Ok(uri) = MusicUri::parse(&c.uri) {
        let d = DiscoveredItem {
            uri,
            card: c.clone(),
            taggers: None,
        };
        return item(acc, lib, &d);
    }
    DiscoverItem {
        uri: c.uri.clone(),
        title: c.name.clone(),
        artist: c.artist_hint(),
        description: c.description.clone().or_else(|| c.start.clone()),
        image: c.image.clone(),
        taggers: None,
        in_library: None,
        kind: kind_of(c),
        site: c.site.clone(),
        pinakes_id: Some(c.id.clone()),
        group_size: c.group.as_ref().map(|g| g.size),
        playable: None,
        also_on: also_on_rows(&c.also_on),
        page: Some(c.uri.clone()),
        plays_from: None,
        connect: Vec::new(),
    }
}

fn site_slug(site: &str) -> String {
    site.to_lowercase().replace(' ', "-")
}

/// `also_on` entries as Versions rows (tagged first, playable when the node streams the service).
pub(crate) fn also_on_rows(links: &[pm_pubky::pinakes::AlsoOn]) -> Vec<Version> {
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
    /// The page the item itself is (played first among equals).
    pub own: bool,
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
    let i = segs
        .iter()
        .position(|s| matches!(*s, "track" | "album" | "artist"))?;
    let kind = match segs[i] {
        "track" => "track",
        "album" => "album",
        _ => "artist",
    };
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
        own: false,
    })
}

/// How this node can use a service right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
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

pub(crate) fn access(acc: &ServiceAccess, service: &str) -> Access {
    match service {
        "tidal" => acc.tidal,
        "deezer" => Access::Preview,
        "spotify" => acc.spotify,
        _ => Access::No,
    }
}

/// A service's name as people say it ("TIDAL", "Deezer", …).
pub(crate) fn service_title(id: &str) -> String {
    match id {
        "tidal" => "TIDAL".into(),
        "deezer" => "Deezer".into(),
        "spotify" => "Spotify".into(),
        "archive" => "Internet Archive".into(),
        "audius" => "Audius".into(),
        other => other.to_string(),
    }
}

pub(crate) fn service_name(id: &str) -> &'static str {
    match id {
        "tidal" => "TIDAL",
        "deezer" => "Deezer",
        "spotify" => "Spotify",
        _ => "service",
    }
}

pub(crate) fn plays_from_link(l: &ServiceLink, a: Access) -> PlaysFrom {
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
    // The card's own page comes first; it counts as tagged only when its row says so
    // (Pinakes now reads links nobody tagged).
    if let Some(mut l) = it
        .page
        .as_deref()
        .and_then(|p| service_link(p, it.taggers.unwrap_or(0) > 0))
    {
        l.own = true;
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
    } * 4
        + if l.own { 0 } else { 1 + usize::from(!l.tagged) };
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

/// A MusicBrainz card without links (Pinakes is making MusicBrainz links
/// their own hubs; until their cards carry `also_on`): ask by `key=mbid:` and
/// resolve again.
pub(crate) async fn fill_from_key(s: &NodeState, it: &mut DiscoverItem, album_id: Option<i64>) {
    if !it.also_on.is_empty() || it.in_library.is_some() {
        return;
    }
    // A MusicBrainz recording asks by `mbid:`, an ISRC link by `isrc:`
    let page = it.page.clone().unwrap_or_else(|| it.uri.clone());
    let key = if let Some(m) = page.strip_prefix("https://musicbrainz.org/recording/") {
        format!("mbid:{}", m.trim_end_matches('/'))
    } else if let Some(i) = page.strip_prefix("https://musicbrainz.org/isrc/") {
        format!("isrc:{}", i.trim_end_matches('/').to_ascii_uppercase())
    } else {
        return;
    };
    it.also_on = also_on_versions(s, &key, &format!("pinakes.version.{key}")).await;
    let r = resolve(&service_access(s), it, album_id);
    it.plays_from = r.plays_from;
    it.connect = r.connect;
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
    let r = match crate::pinakes_reader::card(&s, &uri).await {
        crate::pinakes_reader::Card::Found(r) => Some(*r),
        _ => None,
    };
    // Pinakes knows it: every link of the group. Otherwise the provider's own page.
    let (it, album_id, album_name, duration) = match &r {
        Some(r) => {
            let (mut it, a) = detail_item(&s, r);
            fill_from_key(&s, &mut it, a).await;
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
    Ok(Json(describe_uris(&s, &req.uris).await))
}

/// The rows behind [`describe`], one per URL in order (at most 500).
pub(crate) async fn describe_uris(s: &Arc<NodeState>, uris: &[String]) -> Vec<DiscoverItem> {
    let uris: Vec<String> = uris
        .iter()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .take(500)
        .collect();
    let now = pm_library::db::now_unix();
    // v2: track links are named by their song, not their album
    let cache_key = |u: &str| format!("describe2.{u}");
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
                // Rows Pinakes did not know yet are looked at again after ten minutes.
                let ttl = if it.pinakes_id.is_some() { 86_400 } else { 600 };
                if now - at < ttl {
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
                    let acc = service_access(s);
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
    let mut unknown_to_pinakes = Vec::new();
    for u in &missing {
        if fresh.iter().any(|(x, _)| x == u) {
            continue;
        }
        unknown_to_pinakes.push(u.clone());
        if let Some(it) = describe_from_provider(s, u).await {
            fresh.push((u.clone(), it));
        }
    }
    // Ask Pinakes to read them (paced), so their groups and tags come next time.
    crate::pinakes_reader::resolve_later(s, unknown_to_pinakes);
    {
        let lib = s.library.lock().expect("library poisoned");
        for (u, it) in &fresh {
            if let Ok(j) = serde_json::to_string(&(now, it)) {
                let _ = lib.set_setting(&cache_key(u), &j);
            }
        }
    }
    found.extend(fresh);
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
        .collect()
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
pub(crate) async fn describe_from_provider(s: &NodeState, u: &str) -> Option<DiscoverItem> {
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
    // A track link's item is its album; the song is its one track
    let song = (l.kind == "track" && src.tracks.len() == 1).then(|| &src.tracks[0]);
    let mut it = DiscoverItem {
        uri: u.to_string(),
        title: song
            .map(|t| t.title.clone())
            .or_else(|| Some(src.title.clone()))
            .filter(|t| !t.trim().is_empty()),
        artist: song
            .and_then(|t| t.artist.clone())
            .or_else(|| src.creator.clone()),
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

/// An album's tracklist (a release, or the first release of a release
/// group) or an artist's albums, from MusicBrainz.
async fn musicbrainz_contents(s: &NodeState, kind: &str, mbid: &str) -> (Vec<ItemTrack>, Vec<ItemAlbum>) {
    let contact = s.contact();
    if contact.is_empty() || pm_model::Mbid::parse(mbid).is_err() {
        return (Vec::new(), Vec::new());
    }
    let mut cfg = pm_library::musicbrainz::MusicBrainzConfig::new(contact);
    cfg.base_url = s.mb_base.read().expect("mb lock").clone();
    let Ok(mb) = pm_library::musicbrainz::MusicBrainzClient::new(cfg) else {
        return (Vec::new(), Vec::new());
    };
    let ms = |v: &serde_json::Value| v.as_u64();
    match kind {
        "album" => {
            // The MBID may name a release or a release group.
            let mut rel = mb
                .get_json(
                    &format!("release/{mbid}"),
                    &[("inc", "recordings artist-credits")],
                )
                .await
                .ok()
                .flatten();
            if rel.is_none() {
                if let Some(g) = mb
                    .get_json(&format!("release-group/{mbid}"), &[("inc", "releases")])
                    .await
                    .ok()
                    .flatten()
                {
                    if let Some(first) = g["releases"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|r| r["id"].as_str())
                    {
                        rel = mb
                            .get_json(
                                &format!("release/{first}"),
                                &[("inc", "recordings artist-credits")],
                            )
                            .await
                            .ok()
                            .flatten();
                    }
                }
            }
            let Some(rel) = rel else {
                return (Vec::new(), Vec::new());
            };
            let credit = |v: &serde_json::Value| {
                v["artist-credit"].as_array().map(|a| {
                    a.iter()
                        .map(|c| {
                            format!(
                                "{}{}",
                                c["name"].as_str().unwrap_or(""),
                                c["joinphrase"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<String>()
                })
            };
            let album_title = rel["title"].as_str().map(str::to_string);
            let tracks = rel["media"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|m| m["tracks"].as_array().cloned().unwrap_or_default())
                .map(|t| {
                    let rec = t["recording"]["id"]
                        .as_str()
                        .map(|r| format!("https://musicbrainz.org/recording/{r}"));
                    ItemTrack {
                        no: t["position"].as_u64().map(|n| n as u32),
                        title: t["title"].as_str().unwrap_or("").to_string(),
                        artist: credit(&t["recording"])
                            .or_else(|| credit(&rel))
                            .filter(|a| !a.is_empty()),
                        duration_ms: ms(&t["length"]),
                        id: rec.as_deref().map(page_id),
                        page: rec,
                        album: album_title.clone(),
                        ..Default::default()
                    }
                })
                .collect();
            (tracks, Vec::new())
        }
        "artist" => {
            let Some(g) = mb
                .get_json(
                    "release-group",
                    &[("artist", mbid), ("type", "album"), ("limit", "100")],
                )
                .await
                .ok()
                .flatten()
            else {
                return (Vec::new(), Vec::new());
            };
            let mut albums: Vec<ItemAlbum> = g["release-groups"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|x| x["secondary-types"].as_array().is_none_or(|a| a.is_empty()))
                .filter_map(|x| {
                    let id = x["id"].as_str()?;
                    let page = format!("https://musicbrainz.org/release-group/{id}");
                    Some(ItemAlbum {
                        id: page_id(&page),
                        title: x["title"].as_str()?.to_string(),
                        year: x["first-release-date"]
                            .as_str()
                            .and_then(|d| d.get(..4))
                            .and_then(|y| y.parse().ok()),
                        image: Some(format!(
                            "https://coverartarchive.org/release-group/{id}/front-250"
                        )),
                        page,
                    })
                })
                .collect();
            albums.sort_by_key(|a| std::cmp::Reverse(a.year));
            (Vec::new(), albums)
        }
        _ => (Vec::new(), Vec::new()),
    }
}

/// A page id for a link (mirrors the web UI): `mb-<mbid>` for MusicBrainz
/// recordings, artists and release groups, `isrc-<ISRC>`, else
/// `u-<base64url of the link>`.
pub(crate) fn page_id(url: &str) -> String {
    use base64::Engine;
    for prefix in [
        "https://musicbrainz.org/recording/",
        "https://musicbrainz.org/artist/",
        "https://musicbrainz.org/release-group/",
    ] {
        if let Some(m) = url.strip_prefix(prefix) {
            if pm_model::Mbid::parse(m.trim_end_matches('/')).is_ok() {
                return format!("mb-{}", m.trim_end_matches('/'));
            }
        }
    }
    if let Some(i) = url.strip_prefix("https://musicbrainz.org/isrc/") {
        if pm_model::Isrc::parse(i).is_ok() {
            return format!("isrc-{}", i.to_ascii_uppercase());
        }
    }
    format!(
        "u-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url)
    )
}

/// The link a page id names (`mb-`, `isrc-`, `u-`); `kind` picks the
/// MusicBrainz entity for `mb-`.
pub(crate) fn url_of_page(kind: &str, id: &str) -> Option<String> {
    use base64::Engine;
    if let Some(m) = id.strip_prefix("mb-") {
        pm_model::Mbid::parse(m).ok()?;
        let entity = match kind {
            "artist" => "artist",
            "album" => "release-group",
            _ => "recording",
        };
        return Some(format!("https://musicbrainz.org/{entity}/{m}"));
    }
    if let Some(i) = id.strip_prefix("isrc-") {
        pm_model::Isrc::parse(i).ok()?;
        return Some(format!("https://musicbrainz.org/isrc/{i}"));
    }
    let b = id.strip_prefix("u-")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b).ok()?;
    let u = String::from_utf8(bytes).ok()?;
    url::Url::parse(&u).ok().map(|_| u)
}

/// The service page of a queue URI (`tidal:track:1` → `https://tidal.com/track/1`).
pub(crate) fn page_of_queue_uri(uri: &str) -> Option<String> {
    let (svc, rest) = uri.split_once(':')?;
    let (kind, id) = rest.split_once(':')?;
    Some(match svc {
        "tidal" => format!("https://tidal.com/{kind}/{id}"),
        "deezer" => format!("https://www.deezer.com/{kind}/{id}"),
        "spotify" => format!("https://open.spotify.com/{kind}/{id}"),
        _ => return None,
    })
}

/// `GET /v1/discover/contents?uri=…`: an item's songs (albums) or albums and
/// top songs (artists), from the service it plays from (Deezer previews when
/// nothing full is connected). Cached six hours per link.
pub(crate) async fn contents(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<ItemContents> {
    let Some(uri) = q.uri.clone().filter(|u| !u.trim().is_empty()) else {
        return Err(ApiError::bad_request("uri is required"));
    };
    Ok(Json(load_external(&s, uri.trim()).await.contents))
}

/// Everything known about a music link that is not a library page: the
/// item, its Pinakes card (or the provider's data while Pinakes reads it),
/// and its contents.
pub(crate) struct External {
    pub it: DiscoverItem,
    pub album_id: Option<i64>,
    pub card: Option<pm_pubky::pinakes::ResourceDetail>,
    pub reading: bool,
    pub contents: ItemContents,
}

pub(crate) async fn load_external(s: &Arc<NodeState>, uri: &str) -> External {
    use crate::streaming::StreamingService;
    let uri = uri.trim().to_string();
    let cache_key = format!("item.contents.{uri}");
    let cached: Option<ItemContents> = {
        let lib = s.library.lock().expect("library poisoned");
        lib.get_setting(&cache_key)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str::<(i64, ItemContents)>(&j).ok())
            .filter(|(at, c)| pm_library::db::now_unix() - at < 6 * 3600 && !c.tracks.is_empty())
            .map(|(_, c)| c)
    };
    let card = crate::pinakes_reader::card(s, &uri).await;
    let reading = matches!(card, crate::pinakes_reader::Card::Reading(_));
    let r = match card {
        crate::pinakes_reader::Card::Found(r) => Some(*r),
        _ => None,
    };
    let (mut it, album_id) = match &r {
        Some(r) => detail_item(s, r),
        None => match describe_from_provider(s, &uri).await {
            Some(it) => (it, None),
            None => (bare_item(&uri), None),
        },
    };
    if r.is_some() {
        fill_from_key(s, &mut it, album_id).await;
    }
    // The owner's own matches ("Find on other services") count like Pinakes's links.
    for m in crate::item::matches_for(s, &uri) {
        if !it.also_on.iter().any(|v| v.url == m.url) {
            it.also_on.push(m);
        }
    }
    let acc = service_access(s);
    let res = resolve(&acc, &it, album_id);
    let mut out = ItemContents {
        kind: it.kind.clone(),
        plays_from: res.plays_from.clone(),
        connect: res.connect.clone(),
        ..Default::default()
    };
    if let Some(c) = cached {
        return External {
            it,
            album_id,
            card: r,
            reading,
            contents: c,
        };
    }
    // The library album: its tracks, in order.
    if let (Some(p), Some(a)) = (&res.plays_from, album_id) {
        if p.service == "library" && p.kind == "album" {
            let lib = s.library.lock().expect("library poisoned");
            out.tracks = lib
                .album_tracks(a)
                .unwrap_or_default()
                .into_iter()
                .map(|t| ItemTrack {
                    no: t.track_no,
                    artist: Some(
                        t.artists
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    title: t.title,
                    duration_ms: t.duration_ms,
                    uri: None,
                    track_id: t.id.map(|i| i.0),
                    page: None,
                    image: None,
                    ..Default::default()
                })
                .collect();
        }
    }
    // Otherwise the service it plays from; when that one lists nothing (Spotify
    // refusing the app, a region gap), the page's own link, then Deezer's previews.
    let mut candidates: Vec<ServiceLink> = res.link.clone().into_iter().collect();
    if let Some(own) = it.page.as_deref().and_then(|p| service_link(p, false)) {
        candidates.push(own);
    }
    candidates.extend(
        it.also_on
            .iter()
            .filter_map(|v| service_link(&v.url, false))
            .filter(|l| l.service == "deezer" && l.kind == it.kind),
    );
    let mut tried = std::collections::HashSet::new();
    for cand in &candidates {
        if !out.tracks.is_empty() || !out.albums.is_empty() {
            break;
        }
        if !tried.insert((cand.service, cand.id.clone())) {
            continue;
        }
        let l = cand;
        {
            let svc: Option<Box<dyn StreamingService>> = match l.service {
                "tidal" => Some(Box::new(crate::tidal::Tidal)),
                "deezer" => Some(Box::new(crate::deezer::Deezer)),
                "spotify" => Some(Box::new(crate::spotify::Spotify)),
                _ => None,
            };
            if let Some(svc) = svc {
                match l.kind {
                    "album" => {
                        if let Ok(item) = svc.item(s, &format!("album:{}", l.id)).await {
                            out.tracks = item
                                .tracks
                                .iter()
                                .map(|t| ItemTrack {
                                    no: t.track_no,
                                    title: t.title.clone(),
                                    artist: t.artist.clone().or_else(|| item.creator.clone()),
                                    duration_ms: t.duration_ms,
                                    uri: Some(t.url.clone()),
                                    track_id: None,
                                    page: page_of_queue_uri(&t.url),
                                    image: item.image.clone(),
                                    id: page_of_queue_uri(&t.url).map(|p| page_id(&p)),
                                    album: Some(item.title.clone()),
                                    ..Default::default()
                                })
                                .collect();
                        }
                    }
                    "artist" => {
                        if let Ok(a) = svc.artist(s, &format!("artist:{}", l.id)).await {
                            out.albums = a
                                .albums
                                .iter()
                                .map(|h| ItemAlbum {
                                    id: page_id(&h.url),
                                    title: h.title.clone(),
                                    year: h.year,
                                    image: h.image.clone(),
                                    page: h.url.clone(),
                                })
                                .collect();
                        }
                        let top = match l.service {
                            "tidal" => crate::tidal::artist_top_tracks(s, &l.id, 10).await.ok(),
                            "deezer" => crate::deezer::artist_top_tracks(s, &l.id, 10).await.ok(),
                            _ => None,
                        };
                        out.tracks = top
                            .unwrap_or_default()
                            .into_iter()
                            .map(|h| ItemTrack {
                                no: None,
                                title: h.title,
                                artist: h.creator,
                                duration_ms: h.duration_ms,
                                page: h.uri.as_deref().and_then(page_of_queue_uri).or(Some(h.url)),
                                uri: h.uri,
                                track_id: None,
                                image: h.image,
                                ..Default::default()
                            })
                            .collect();
                    }
                    _ => {}
                }
            }
        }
        if (!out.tracks.is_empty() || !out.albums.is_empty()) && res.link.as_ref() != Some(l) {
            // Say where the list (and Play) actually comes from.
            out.plays_from = Some(plays_from_link(l, access(&acc, l.service)));
        }
    }
    // No service lists it: MusicBrainz by the card's MBID (owner's scope decision:
    // Pinakes carries no tracklists or discographies). Rows open their pages;
    // they play once a connected service has them.
    if out.tracks.is_empty() && out.albums.is_empty() {
        if let Some(mbid) = r.as_ref().and_then(|r| r.mbid()) {
            let (t, a) = musicbrainz_contents(s, &it.kind, &mbid).await;
            out.tracks = t;
            out.albums = a;
        }
    }
    for t in &mut out.tracks {
        if t.id.is_none() {
            t.id = t.page.as_deref().map(page_id);
        }
    }
    out.total_ms = out.tracks.iter().filter_map(|t| t.duration_ms).sum();
    if !out.tracks.is_empty() || !out.albums.is_empty() {
        let lib = s.library.lock().expect("library poisoned");
        let _ = lib.set_setting(
            &cache_key,
            &serde_json::to_string(&(pm_library::db::now_unix(), &out)).unwrap_or_default(),
        );
    }
    External {
        it,
        album_id,
        card: r,
        reading,
        contents: out,
    }
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
    if l.kind == "artist" {
        // An artist plays its top songs.
        let top = match l.service {
            "tidal" => crate::tidal::artist_top_tracks(s, &l.id, 20).await?,
            _ => crate::deezer::artist_top_tracks(s, &l.id, 20).await?,
        };
        return Ok(top
            .into_iter()
            .filter_map(|h| {
                Some(pm_zones::QueueItem {
                    id: 0,
                    uri: h.uri?,
                    title: Some(h.title),
                    artist: h.creator,
                    album: None,
                    duration_ms: h.duration_ms,
                    image: h.image,
                })
            })
            .collect());
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
    let Some(uri) = q.uri.clone().filter(|u| !u.trim().is_empty()) else {
        return Err(ApiError::bad_request("uri is required"));
    };
    let uri = uri.trim().to_string();
    let r = match crate::pinakes_reader::card(&s, &uri).await {
        crate::pinakes_reader::Card::Found(r) => *r,
        // Not in Pinakes yet: the provider's own data for that id, never an
        // error page; the client asks again while `reading` is set.
        other => {
            let reading = matches!(other, crate::pinakes_reader::Card::Reading(_));
            let pending = match other {
                crate::pinakes_reader::Card::Reading(Some(p)) => Some(p),
                _ => None,
            };
            let mut item = match describe_from_provider(&s, &uri).await {
                Some(it) => it,
                None => bare_item(&uri),
            };
            if let Some(p) = &pending {
                item.title = item.title.or_else(|| p.card.name.clone());
                item.image = item.image.or_else(|| p.card.image.clone());
                item.description = item.description.or_else(|| p.card.description.clone());
            }
            return Ok(Json(DiscoverDetail {
                item,
                mbid: None,
                isrcs: Vec::new(),
                upc: None,
                duration_secs: None,
                album: None,
                original_album: None,
                artist_type: None,
                on_pubky: None,
                in_library_album: None,
                artist_info: None,
                reading,
            }));
        }
    };
    let facts = r.facts();
    let (mut item, in_library_album) = detail_item(&s, &r);
    fill_from_key(&s, &mut item, in_library_album).await;
    let on_pubky = on_pubky_for(&s, &r.card.uri).await;
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
        reading: false,
    }))
}

/// Who tagged a URL on Pubky (Nexus): labels with counts, distinct taggers.
pub(crate) async fn on_pubky_for(s: &NodeState, uri: &str) -> Option<OnPubky> {
    let d = s.discovery()?;
    let view = d.nexus().resource_by_uri(uri).await.ok()?;
    let mut tags: Vec<(String, u64)> = view
        .tags
        .iter()
        .map(|t| (t.label.clone(), t.taggers_count.max(t.taggers.len() as u64)))
        .collect();
    tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Some(OnPubky {
        uri: uri.to_string(),
        indexed: true,
        taggers: view.distinct_taggers() as u64,
        tags,
    })
}

/// A row for a link nobody describes yet: its service, never the URL as a name.
fn bare_item(u: &str) -> DiscoverItem {
    DiscoverItem {
        uri: u.to_string(),
        title: None,
        artist: None,
        description: None,
        image: None,
        taggers: None,
        in_library: None,
        kind: service_link(u, false)
            .map(|l| l.kind.to_string())
            .unwrap_or_else(|| "other".into()),
        site: site_of(u),
        pinakes_id: None,
        group_size: None,
        playable: playable_uri(u),
        also_on: Vec::new(),
        page: Some(u.to_string()),
        plays_from: None,
        connect: Vec::new(),
    }
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
/// The service links of a MusicBrainz URL's own card (Pinakes treats
/// recording, release, artist and ISRC URLs as hubs since pubky/pinakes#24):
/// an identified file's canonical URL gets its versions directly. Unknown
/// hubs are read on demand (paced); empty while Pinakes reads or when the
/// card has no links yet.
pub(crate) async fn hub_versions(s: &Arc<NodeState>, hub: &str) -> Vec<Version> {
    match crate::pinakes_reader::card(s, hub).await {
        crate::pinakes_reader::Card::Found(r) => also_on_rows(r.also_on()),
        _ => Vec::new(),
    }
}

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
