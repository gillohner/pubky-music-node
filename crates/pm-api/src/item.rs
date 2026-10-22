//! One item page per kind (song, album, artist) for every source
//! (docs/ux/08-item-page.md): the library, a streaming service, or a link
//! only Pubky knows. Whatever the source, the answer has the same sections:
//! hero, songs (and albums for artists), versions, On Pubky, About, plus the
//! playback answer and the target that tags, ratings and favourites attach to.

use crate::discover::{load_external, page_id, url_of_page};
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::Json;
use pm_model::{MusicUri, Track, TrackId};
use std::sync::Arc;

/// `GET /v1/item/{kind}/{id}`. A page is one instance (the library file, the TIDAL track…);
/// the same recording elsewhere is `also_on`, other recordings are `versions`
/// (STEWARD-UX-INSTANCES items 1 and 2).
pub(crate) async fn page(
    State(s): State<Arc<NodeState>>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<ItemPage> {
    if !matches!(kind.as_str(), "song" | "album" | "artist") {
        return Err(ApiError::bad_request("kind is song, album or artist"));
    }
    let mut pg = raw_page(&s, &kind, &id).await?;
    // A generic id (`isrc-…`, `mb-…`) is the item's global page: platform-neutral, every source
    // a row (the library's copy too), tags on the item's own id (STEWARD-UX-INSTANCES item 8)
    if id.starts_with("isrc-") || id.starts_with("mb-") {
        let generic = url_of_page(&kind, &id);
        // What the generic page lacks (cover, album, title) comes from the best source
        if pg.image.is_none() || pg.title.is_none() || (kind == "song" && pg.album.is_none()) {
            if let Some((pid, url)) = best_instance(&s, &pg).filter(|(pid, _)| !pid.starts_with("lib-")) {
                let inst = external(&s, &kind, &pid, &url).await;
                pg.image = pg.image.take().or(inst.image);
                pg.title = pg.title.take().or(inst.title);
                pg.artist = pg.artist.take().or(inst.artist);
                if pg.album.is_none() {
                    pg.album = inst.album;
                }
                pg.year = pg.year.or(inst.year);
                if pg.albums.is_empty() {
                    pg.albums = inst.albums;
                }
                pg.versions.extend(inst.versions);
            }
        }
        if !pg.id.starts_with("lib-") {
            pg.target = serde_json::json!({ "uri": generic.clone().unwrap_or_default() });
        }
        pg.id = id.clone();
        pg.source = "All sources".into();
        pg.source_id = "global".into();
        pg.link = None;
        pg.song_uri = pg.song_uri.take().or(generic);
    }
    // A service's own track page: its ISRC ties it to the song, so the library copy and the
    // other services are "also on", not other versions
    if kind == "song" && pg.song_uri.is_none() {
        if let Some((svc, n)) = id
            .split_once('-')
            .filter(|(svc, _)| matches!(*svc, "tidal" | "deezer" | "spotify"))
        {
            let isrc = tokio::time::timeout(std::time::Duration::from_secs(2), service_isrc(&s, svc, n))
                .await
                .ok()
                .flatten();
            if let Some(isrc) = isrc {
                let generic = format!("isrc-{isrc}");
                pg.song_uri = url_of_page("song", &generic);
                if let Ok(g) = raw_page(&s, &kind, &generic).await {
                    if let Some(n) = g.id.strip_prefix("lib-") {
                        pg.versions.push(Version {
                            page: None,
                            source: "library".into(),
                            id: n.into(),
                            label: g
                                .plays_from
                                .as_ref()
                                .map(|p| p.label.clone())
                                .unwrap_or_else(|| "Your library".into()),
                            url: String::new(),
                            uri: Some(format!("track:{n}")),
                        });
                    }
                    pg.versions.extend(g.versions);
                    if pg.on_pubky.is_none() {
                        pg.on_pubky = g.on_pubky;
                    }
                }
            }
        }
    }
    let order = crate::play_from::global_order(&s.library.lock().expect("library poisoned"));
    split_sources(&mut pg, &order);
    if kind == "song" {
        decide_play(&s, &mut pg).await;
    }
    // A song's album when nothing else knew it: from the page's own service, then from the same
    // recording on another service (Deezer answers without a sign-in)
    if kind == "song" && pg.albums.is_empty() {
        let mut tries: Vec<String> = vec![pg.id.clone()];
        tries.extend(pg.also_on.iter().filter_map(|v| v.page.clone()));
        for pid in tries {
            let Some((svc, n)) = pid
                .split_once('-')
                .filter(|(svc, _)| matches!(*svc, "tidal" | "deezer" | "spotify"))
            else {
                continue;
            };
            let fut = async {
                match svc {
                    "deezer" => crate::deezer::track_album(&s, n).await,
                    "spotify" => crate::spotify::track_album(&s, n).await,
                    _ => crate::tidal::track_album(&s, n).await,
                }
            };
            if let Ok(Some((title, url, image, year))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), fut).await
            {
                let aid = page_id(&url);
                if pg.album.is_none() {
                    pg.album = Some(PageLink {
                        name: title.clone(),
                        id: Some(aid.clone()),
                    });
                }
                if pg.year.is_none() {
                    pg.year = year;
                }
                pg.albums.push(ItemAlbum {
                    id: aid,
                    title,
                    year,
                    image,
                    page: url,
                });
                break;
            }
        }
    }
    if kind == "song" {
        pg.versions = other_recordings(&s, &pg);
    }
    Ok(Json(pg))
}

async fn raw_page(s: &Arc<NodeState>, kind: &str, id: &str) -> Result<ItemPage, ApiError> {
    if let Some(n) = id.strip_prefix("lib-") {
        let n: i64 = n
            .parse()
            .map_err(|_| ApiError::bad_request("lib- ids are numbers"))?;
        return match kind {
            "song" => library_song(s, n).await,
            "album" => library_album(s, n).await,
            _ => library_artist(s, n).await,
        };
    }
    let url = url_of_page(kind, id).ok_or_else(|| ApiError::bad_request("unknown page id"))?;
    // A song the library owns (by its canonical identity) is the library's page.
    if kind == "song" {
        if let Ok(u) = MusicUri::parse(&url) {
            let owned = {
                let lib = s.library.lock().expect("library poisoned");
                lib.tracks_by_canonical(&u)
                    .ok()
                    .and_then(|v| v.into_iter().next())
                    .or_else(|| match &u {
                        MusicUri::Isrc { isrc } => {
                            lib.tracks_by_isrc(&isrc.to_string()).ok()?.into_iter().next()
                        }
                        _ => None,
                    })
            };
            if let Some(t) = owned.and_then(|t| t.id) {
                return library_song(s, t.0).await;
            }
        }
    }
    Ok(external(s, kind, id, &url).await)
}

/// The ISRC of a service's track (kept once known: it does not change).
async fn service_isrc(s: &NodeState, svc: &str, id: &str) -> Option<String> {
    let key = format!("isrc.of.{svc}.{id}");
    if let Some(v) = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(&key)
        .ok()
        .flatten()
    {
        return (!v.is_empty()).then_some(v);
    }
    let found = match svc {
        "deezer" => crate::deezer::track_isrc(s, id).await,
        "tidal" => crate::tidal::track_isrc(s, id).await,
        "spotify" => crate::spotify::track_isrc(s, id).await,
        _ => None,
    }
    .and_then(|i| pm_model::Isrc::parse(&i).ok())
    .map(|i| i.to_string());
    if let Some(i) = &found {
        let _ = s.library.lock().expect("library poisoned").set_setting(&key, i);
    }
    found
}

/// "Prefer full tracks from another source": a page whose own source only has a preview plays
/// the full track from the play-from order instead (off by default).
pub const PREFER_FULL_KEY: &str = "play.prefer_full";

/// What Play does on a song page, decided once: the page's own source whenever it can play
/// anything (previews included), else the play-from order with the reason. The page's song row
/// and its eyebrow (`plays_from`, `plays_note`) both come from here, so they can't disagree
/// (STEWARD-UX-INSTANCES item 9).
async fn decide_play(s: &Arc<NodeState>, pg: &mut ItemPage) {
    let Some((svc, n)) = pg.id.split_once('-').map(|(a, b)| (a.to_string(), b.to_string())) else {
        return;
    };
    if !matches!(svc.as_str(), "tidal" | "deezer" | "spotify" | "jamendo") {
        return;
    }
    let acc = crate::discover::service_access(s);
    let a = crate::discover::access(&acc, &svc);
    use crate::discover::Access;
    let own: Result<(String, &'static str), String> = match svc.as_str() {
        "deezer" => match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            crate::deezer::track_playable(s, &n),
        )
        .await
        {
            Ok(Ok(())) => Ok((crate::deezer::track_uri(&n), "preview")),
            Ok(Err(why)) => Err(why),
            Err(_) => Err("Deezer didn't answer in time".into()),
        },
        "tidal" => match a {
            Access::Full => Ok((crate::tidal::track_uri(&n), "full")),
            Access::Preview => Ok((crate::tidal::track_uri(&n), "preview")),
            _ => Err("TIDAL isn't connected on this node".into()),
        },
        "jamendo" => Ok((crate::jamendo::track_uri(&n), "full")),
        _ => Err(match a {
            Access::External => "Spotify plays only on a Spotify Connect device".into(),
            _ => "Spotify sends no audio to other apps".into(),
        }),
    };
    let prefer_full = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(PREFER_FULL_KEY)
        .ok()
        .flatten()
        .is_some_and(|v| v == "1");
    let fallback = pg.plays_from.clone().filter(|p| p.service != svc);
    let fallback_full = fallback.as_ref().is_some_and(|p| p.mode == "full");
    match own {
        Ok((uri, mode)) if !(prefer_full && mode == "preview" && fallback_full) => {
            pg.plays_from = Some(PlaysFrom {
                service: svc.clone(),
                kind: "track".into(),
                id: format!("track:{n}"),
                label: if mode == "preview" {
                    format!("{} · 30 s preview", crate::discover::service_title(&svc))
                } else {
                    crate::discover::service_title(&svc)
                },
                url: pg.link.clone(),
                mode: mode.into(),
            });
            if let Some(first) = pg.songs.first_mut() {
                first.uri = Some(uri);
                first.track_id = None;
            }
        }
        own => {
            pg.plays_note = Some(match own {
                Err(why) => why,
                Ok(_) => "you prefer full tracks from another source".into(),
            });
            // The song row plays what the eyebrow says
            if let (Some(p), Some(first)) = (fallback.as_ref(), pg.songs.first_mut()) {
                let id = p.id.strip_prefix("track:").unwrap_or(&p.id).to_string();
                match p.service.as_str() {
                    "library" => {
                        first.track_id = id.parse().ok();
                        first.uri = None;
                    }
                    "tidal" => first.uri = Some(crate::tidal::track_uri(&id)),
                    "deezer" => first.uri = Some(crate::deezer::track_uri(&id)),
                    "jamendo" => first.uri = Some(crate::jamendo::track_uri(&id)),
                    _ => {}
                }
            }
        }
    }
}

/// A library artist without a picture: when exactly one service artist of that name has your
/// albums (the "likely" match), its picture is used, found in the background at most once a
/// day (STEWARD-FIX-same-artist.md item 4). The page shows it from the next visit on.
fn find_artist_picture(s: &Arc<NodeState>, n: i64, name: String) {
    let tried = format!("artist.picture_tried.{n}");
    {
        let lib = s.library.lock().expect("library poisoned");
        let now = pm_library::db::now_unix();
        let last: i64 = lib
            .get_setting(&tried)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if now - last < 86_400 {
            return;
        }
        let _ = lib.set_setting(&tried, &now.to_string());
    }
    let s = s.clone();
    tokio::spawn(async move {
        let q = CandidateQuery {
            uri: String::new(),
            kind: "artist".into(),
            title: name,
            artist: None,
            item: Some(format!("lib-{n}")),
        };
        let Ok(Json(c)) = candidates(State(s.clone()), axum::extract::Query(q)).await else {
            return;
        };
        if let Some(img) = c
            .iter()
            .find(|x| x.likely)
            .and_then(|x| x.image.clone())
            .filter(|i| i.starts_with("https://"))
        {
            let _ = s
                .library
                .lock()
                .expect("library poisoned")
                .set_setting(&format!("artist.picture.{n}"), &img);
            tracing::info!(artist = n, "artist picture from the likely match");
        }
    });
}

/// A version's own page id on a page of `kind`.
fn page_of(kind: &str, v: &Version) -> Option<String> {
    if v.source == "library" {
        if let Some(n) = v.uri.as_deref().and_then(|u| u.strip_prefix("track:")) {
            return (kind == "song").then(|| format!("lib-{n}"));
        }
        return (kind != "song" && !v.id.is_empty()).then(|| format!("lib-{}", v.id));
    }
    v.url.starts_with("http").then(|| page_id(&v.url))
}

/// MusicBrainz describes a recording; it plays nothing (it shows under About instead).
fn is_metadata(v: &Version) -> bool {
    v.source == "musicbrainz" || v.url.contains("://musicbrainz.org/")
}

/// One entry per source: a service once, the library once per file, other links once per URL.
fn source_key(v: &Version) -> String {
    match v.source.as_str() {
        "library" => format!("library:{}", v.uri.as_deref().unwrap_or(&v.id)),
        "tidal" | "deezer" | "spotify" => v.source.clone(),
        _ => v.url.trim_end_matches('/').to_string(),
    }
}

/// Where a generic song page should open: the first source in the play-from order that has the
/// song and can play it here (the library, or a connected service), else none (stay generic).
fn best_instance(s: &NodeState, pg: &ItemPage) -> Option<(String, String)> {
    let order = crate::play_from::global_order(&s.library.lock().expect("library poisoned"));
    let acc = crate::discover::service_access(s);
    let usable = |svc: &str| {
        svc == "library"
            || matches!(
                crate::discover::access(&acc, svc),
                crate::discover::Access::Full | crate::discover::Access::External
            )
    };
    let mut here: Vec<(usize, String, String)> = pg
        .versions
        .iter()
        .filter(|v| !is_metadata(v) && usable(&v.source))
        .filter_map(|v| {
            let pos = order.iter().position(|o| *o == v.source)?;
            Some((pos, page_of(&pg.kind, v)?, v.url.clone()))
        })
        .collect();
    // The library page itself counts when the page is the library's
    if pg.id.starts_with("lib-") {
        here.push((
            order.iter().position(|o| o == "library").unwrap_or(0),
            pg.id.clone(),
            String::new(),
        ));
    }
    here.sort_by_key(|(p, _, _)| *p);
    here.into_iter().next().map(|(_, id, url)| (id, url))
}

/// Turn the page's list of sources into `also_on`: the same recording elsewhere, one per source,
/// in the play-from order, each with its own page; never this page itself or MusicBrainz.
fn split_sources(pg: &mut ItemPage, order: &[String]) {
    let mut all = std::mem::take(&mut pg.versions);
    for v in &mut all {
        v.page = page_of(&pg.kind, v);
    }
    let mut seen = std::collections::HashSet::new();
    // This page's own source first, so a second entry of it is not listed as elsewhere
    if let Some(me) = all.iter().find(|v| v.page.as_deref() == Some(pg.id.as_str())) {
        seen.insert(source_key(me));
    }
    let mut also: Vec<Version> = all
        .into_iter()
        .filter(|v| !is_metadata(v) && v.page.as_deref() != Some(pg.id.as_str()))
        .filter(|v| seen.insert(source_key(v)))
        .collect();
    also.sort_by_key(|v| order.iter().position(|o| *o == v.source).unwrap_or(usize::MAX));
    pg.open_on = also
        .iter()
        .filter(|v| v.url.starts_with("http"))
        .cloned()
        .collect();
    pg.also_on = also;
}

/// Other recordings of a song in the library: the same title and artist, another recording
/// (a live take, a remaster, another release's edit).
fn other_recordings(s: &NodeState, pg: &ItemPage) -> Vec<Version> {
    let (Some(title), Some(artist)) = (pg.title.as_deref(), pg.artist.as_ref()) else {
        return Vec::new();
    };
    let norm = |x: &str| {
        x.to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    };
    let (t0, a0) = (norm(title), norm(&artist.name));
    if t0.is_empty() {
        return Vec::new();
    }
    let same_here: std::collections::HashSet<String> = pg
        .also_on
        .iter()
        .filter_map(|v| v.uri.clone())
        .chain(pg.id.strip_prefix("lib-").map(|n| format!("track:{n}")))
        .collect();
    let lib = s.library.lock().expect("library poisoned");
    let Ok(hits) = lib.search_tracks(title, 50) else {
        return Vec::new();
    };
    let mine = pg.uri.clone();
    hits.into_iter()
        .filter(|t| norm(&t.title) == t0 || norm(&t.title).starts_with(&t0))
        .filter(|t| {
            t.artists.iter().any(|a| norm(&a.name) == a0)
                || a0.contains(&norm(
                    &t.artists.first().map(|a| a.name.clone()).unwrap_or_default(),
                ))
        })
        .filter(|t| {
            let uri = format!("track:{}", t.id.map(|i| i.0).unwrap_or_default());
            !same_here.contains(&uri)
                && (mine.is_none() || t.canonical.as_ref().map(|(u, _)| u.to_url()) != mine)
        })
        .filter_map(|t| {
            let n = t.id?.0;
            let album = t.album.as_ref().map(|a| a.title.clone()).unwrap_or_default();
            Some(Version {
                page: Some(format!("lib-{n}")),
                source: "library".into(),
                id: n.to_string(),
                label: if album.is_empty() {
                    format!("{} · {}", t.title, t.file.format)
                } else {
                    format!("{} · {album} · {}", t.title, t.file.format)
                },
                url: String::new(),
                uri: Some(format!("track:{n}")),
            })
        })
        .take(12)
        .collect()
}

/// What an item page shows after its main part (related items, an artist's concerts), asked
/// separately so the page never waits on it: `{related, concerts}`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ItemExtras {
    pub related: Vec<DiscoverItem>,
    pub concerts: Vec<DiscoverItem>,
}

/// Each outside call gets this long; what is not ready is left out.
const EXTRAS_BUDGET: std::time::Duration = std::time::Duration::from_millis(1500);

/// `GET /v1/item/{kind}/{id}/extras`.
pub(crate) async fn page_extras(
    State(s): State<Arc<NodeState>>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<ItemExtras> {
    // The facts extras need (the canonical uri, the artist's MusicBrainz id) come from the page,
    // which is cached by now
    let pg = if let Some(n) = id.strip_prefix("lib-") {
        let n: i64 = n
            .parse()
            .map_err(|_| ApiError::bad_request("lib- ids are numbers"))?;
        match kind.as_str() {
            "song" => library_song(&s, n).await,
            "album" => library_album(&s, n).await,
            _ => library_artist(&s, n).await,
        }?
    } else {
        let url = url_of_page(&kind, &id).ok_or_else(|| ApiError::bad_request("unknown page id"))?;
        external(&s, &kind, &id, &url).await
    };
    let mut pg = pg;
    extras(&s, &mut pg).await;
    Ok(Json(ItemExtras {
        related: pg.related,
        concerts: pg.concerts,
    }))
}

/// Sections that appear once Pinakes ships them: related items (announced in
/// its `/v1/info` limits) and an artist's upcoming concerts (events keyed by
/// performer).
async fn extras(s: &Arc<NodeState>, pg: &mut ItemPage) {
    let Some(d) = s.discovery() else { return };
    let budget = EXTRAS_BUDGET;
    let acc = crate::discover::service_access(s);
    let to_items = |cards: Vec<pm_pubky::pinakes::Card>| -> Vec<DiscoverItem> {
        let lib = s.library.lock().expect("library poisoned");
        cards
            .iter()
            .filter(|c| c.is_ok())
            .map(|c| crate::discover::card_item(&acc, &lib, c))
            .collect()
    };
    let related = async {
        let card_id = pg.uri.clone()?;
        if !crate::home::pinakes_has_related(s).await {
            return None;
        }
        let r = d.resource_for(&card_id).await.ok()??;
        let q = pm_pubky::pinakes::SearchQuery {
            related: Some(r.card.id.clone()),
            collapse: Some("group".into()),
            limit: Some(12),
            ..Default::default()
        };
        d.pinakes().search(&q).await.ok()
    };
    let mbid = if pg.kind == "artist" {
        let mbid = pg
            .uri
            .as_deref()
            .and_then(|u| u.strip_prefix("https://musicbrainz.org/artist/"))
            .map(str::to_string)
            .or_else(|| {
                pg.about
                    .iter()
                    .find(|f| {
                        f.href
                            .as_deref()
                            .is_some_and(|h| h.starts_with("https://musicbrainz.org/artist/"))
                    })
                    .map(|f| f.value.clone())
            });
        mbid
    } else {
        None
    };
    let concerts = async {
        let q = pm_pubky::pinakes::SearchQuery {
            key: Some(format!("mbid:{}", mbid?)),
            type_: Some("MusicEvent".into()),
            from: Some("now".into()),
            sort: Some("start".into()),
            limit: Some(6),
            ..Default::default()
        };
        d.pinakes().search(&q).await.ok()
    };
    // Both at once, each within the budget: what is not ready is left out
    let (related, concerts) = tokio::join!(
        tokio::time::timeout(budget, related),
        tokio::time::timeout(budget, concerts)
    );
    if let Ok(Some(p)) = related {
        pg.related = to_items(p.items);
    }
    if let Ok(Some(p)) = concerts {
        pg.concerts = to_items(p.items);
    }
}

/// A library track's page id: its canonical identity when known.
pub(crate) fn track_page_id(t: &Track) -> String {
    match &t.canonical {
        Some((u, _)) => page_id(&u.to_url()),
        None => format!("lib-{}", t.id.map(|i| i.0).unwrap_or_default()),
    }
}

fn library_row(t: &Track) -> ItemTrack {
    ItemTrack {
        no: t.track_no,
        title: t.title.clone(),
        artist: Some(
            t.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
        .filter(|a| !a.is_empty()),
        duration_ms: t.duration_ms,
        uri: None,
        track_id: t.id.map(|i| i.0),
        page: None,
        image: t.album.as_ref().and_then(|a| a.id).map(cover),
        id: Some(track_page_id(t)),
        artist_id: t.artists.first().and_then(|a| a.id).map(|i| format!("lib-{i}")),
        album: t.album.as_ref().map(|a| a.title.clone()),
        album_id: t.album.as_ref().and_then(|a| a.id).map(|i| format!("lib-{i}")),
    }
}

fn cover(album_id: i64) -> String {
    format!("/v1/library/albums/{album_id}/cover")
}

fn mb_fact(label: &str, entity: &str, mbid: &str) -> Fact {
    Fact {
        label: label.into(),
        value: mbid.into(),
        href: Some(format!("https://musicbrainz.org/{entity}/{mbid}")),
    }
}

fn fact(label: &str, value: impl Into<String>) -> Fact {
    Fact {
        label: label.into(),
        value: value.into(),
        href: None,
    }
}

/// Service pages among versions (the "Open on" list).
fn open_on(versions: &[Version]) -> Vec<Version> {
    versions
        .iter()
        .filter(|v| v.url.starts_with("http"))
        .cloned()
        .collect()
}

async fn library_song(s: &Arc<NodeState>, n: i64) -> Result<ItemPage, ApiError> {
    let t = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(n))?
        .ok_or_else(|| ApiError::not_found(format!("no track {n}")))?;
    let row = library_row(&t);
    let mut versions = vec![Version {
        page: None,
        source: "library".into(),
        id: n.to_string(),
        label: format!("Your library · {}", t.file.format),
        url: String::new(),
        uri: Some(format!("track:{n}")),
    }];
    versions.extend(
        crate::track_versions(State(s.clone()), Path(n))
            .await
            .map(|j| j.0)
            .unwrap_or_default(),
    );
    versions.extend(matches_for(s, &format!("lib:song:{n}")));
    if let Some((u, _)) = &t.canonical {
        versions.extend(matches_for(s, &u.to_url()));
    }
    let canonical = t.canonical.as_ref().map(|(u, _)| u.to_url());
    let on_pubky = match &canonical {
        Some(u) => crate::discover::on_pubky_for(s, u).await,
        None => None,
    };
    let mut about = vec![
        fact("Format", t.file.format.to_string()),
        fact("File", t.file.path.display().to_string()),
    ];
    if let Some(g) = t.replaygain_track_gain_db {
        about.push(fact("ReplayGain", format!("{g:+.1} dB")));
    }
    if let Some(i) = &t.isrc {
        about.push(fact("ISRC", i.to_string()));
    }
    match &t.canonical {
        Some((MusicUri::Recording { mbid }, c)) => {
            about.push(mb_fact("MusicBrainz", "recording", &mbid.to_string()));
            about.push(fact("Identified by", c.label().replace('_', " ")));
        }
        Some((u, c)) => {
            about.push(Fact {
                label: "Identity".into(),
                value: u.to_url(),
                href: Some(u.to_url()),
            });
            about.push(fact("Identified by", c.label().replace('_', " ")));
        }
        None => about.push(fact("Identity", "not identified yet")),
    }
    let albums = t
        .album
        .as_ref()
        .and_then(|a| {
            Some(ItemAlbum {
                id: format!("lib-{}", a.id?),
                title: a.title.clone(),
                year: a.year,
                image: a.id.map(cover),
                page: String::new(),
            })
        })
        .into_iter()
        .collect();
    Ok(ItemPage {
        kind: "song".into(),
        id: format!("lib-{n}"),
        song_uri: canonical.clone(),
        uri: canonical,
        target: serde_json::json!({ "track_id": n }),
        title: Some(t.title.clone()),
        artist: t.artists.first().map(|a| PageLink {
            name: row.artist.clone().unwrap_or_else(|| a.name.clone()),
            id: a.id.map(|i| format!("lib-{i}")),
        }),
        album: t.album.as_ref().map(|a| PageLink {
            name: a.title.clone(),
            id: a.id.map(|i| format!("lib-{i}")),
        }),
        year: t.album.as_ref().and_then(|a| a.year),
        image: row.image.clone(),
        source: "Your library".into(),
        source_id: "library".into(),
        plays_from: Some(PlaysFrom {
            service: "library".into(),
            kind: "track".into(),
            id: n.to_string(),
            label: format!("Your library · {}", t.file.format),
            url: None,
            mode: "full".into(),
        }),
        connect: Vec::new(),
        reading: false,
        pending: false,
        total_ms: t.duration_ms.unwrap_or(0),
        songs: vec![row],
        albums,
        open_on: open_on(&versions),
        versions,
        also_on: Vec::new(),
        opened_as: None,
        plays_note: None,
        link: None,
        on_pubky,
        about,
        blurb: None,
        section: "tracks".into(),
        related: Vec::new(),
        concerts: Vec::new(),
    })
}

async fn library_album(s: &Arc<NodeState>, n: i64) -> Result<ItemPage, ApiError> {
    let (a, tracks) = {
        let lib = s.library.lock().expect("library poisoned");
        let a = lib
            .get_album(n)?
            .ok_or_else(|| ApiError::not_found(format!("no album {n}")))?;
        (a, lib.album_tracks(n)?)
    };
    let songs: Vec<ItemTrack> = tracks.iter().map(library_row).collect();
    let mut versions = vec![Version {
        page: None,
        source: "library".into(),
        id: n.to_string(),
        label: format!(
            "Your library{}",
            tracks
                .first()
                .map(|t| format!(" · {}", t.file.format))
                .unwrap_or_default()
        ),
        url: String::new(),
        uri: None,
    }];
    versions.extend(
        crate::album_versions(State(s.clone()), Path(n))
            .await
            .map(|j| j.0)
            .unwrap_or_default(),
    );
    versions.extend(matches_for(s, &format!("lib:album:{n}")));
    let canonical = a
        .release_group_mbid
        .map(|m| format!("https://musicbrainz.org/release-group/{m}"))
        .or_else(|| {
            a.release_mbid
                .map(|m| format!("https://musicbrainz.org/release/{m}"))
        });
    let on_pubky = match &canonical {
        Some(u) => crate::discover::on_pubky_for(s, u).await,
        None => None,
    };
    let mut about = Vec::new();
    if let Some(y) = a.year {
        about.push(fact("Year", y.to_string()));
    }
    about.push(fact("Songs", tracks.len().to_string()));
    if let Some(t) = tracks.first() {
        about.push(fact("Format", t.file.format.to_string()));
    }
    if let Some(m) = a.release_mbid {
        about.push(mb_fact("MusicBrainz release", "release", &m.to_string()));
    }
    if let Some(m) = a.release_group_mbid {
        about.push(mb_fact(
            "MusicBrainz release group",
            "release-group",
            &m.to_string(),
        ));
    }
    if let Some(u) = &a.upc {
        about.push(fact("UPC", u.to_string()));
    }
    let first_artist = tracks.first().and_then(|t| t.artists.first().cloned());
    Ok(ItemPage {
        kind: "album".into(),
        id: format!("lib-{n}"),
        uri: canonical,
        target: serde_json::json!({ "album_id": n }),
        title: Some(a.title.clone()),
        artist: Some(PageLink {
            name: a
                .album_artist
                .clone()
                .or_else(|| first_artist.as_ref().map(|x| x.name.clone()))
                .unwrap_or_default(),
            id: first_artist.and_then(|x| x.id).map(|i| format!("lib-{i}")),
        })
        .filter(|l| !l.name.is_empty()),
        album: None,
        year: a.year,
        image: Some(cover(n)),
        source: "Your library".into(),
        source_id: "library".into(),
        plays_from: Some(PlaysFrom {
            service: "library".into(),
            kind: "album".into(),
            id: n.to_string(),
            label: "Your library".into(),
            url: None,
            mode: "full".into(),
        }),
        connect: Vec::new(),
        reading: false,
        pending: false,
        total_ms: songs.iter().filter_map(|t| t.duration_ms).sum(),
        songs,
        albums: Vec::new(),
        open_on: open_on(&versions),
        versions,
        also_on: Vec::new(),
        opened_as: None,
        plays_note: None,
        song_uri: None,
        link: None,
        on_pubky,
        about,
        blurb: None,
        section: "albums".into(),
        related: Vec::new(),
        concerts: Vec::new(),
    })
}

async fn library_artist(s: &Arc<NodeState>, n: i64) -> Result<ItemPage, ApiError> {
    let (a, albums, tracks) = {
        let lib = s.library.lock().expect("library poisoned");
        let a = lib
            .get_artist(n)?
            .ok_or_else(|| ApiError::not_found(format!("no artist {n}")))?;
        (a, lib.artist_albums(n)?, lib.artist_tracks(n)?)
    };
    let info = crate::artist_info::artist_info(s, n).await.ok();
    let canonical = a.mbid.map(|m| format!("https://musicbrainz.org/artist/{m}"));
    let on_pubky = match &canonical {
        Some(u) => crate::discover::on_pubky_for(s, u).await,
        None => None,
    };
    let mut about = Vec::new();
    if let Some(m) = a.mbid {
        about.push(mb_fact("MusicBrainz", "artist", &m.to_string()));
    }
    about.push(fact(
        "In your library",
        format!(
            "{} · {}",
            crate::plural(a.albums, "album"),
            crate::plural(a.tracks, "song")
        ),
    ));
    if let Some(i) = &info {
        for (label, u) in [
            ("Wikipedia", &i.wikipedia),
            ("Wikidata", &i.wikidata),
            ("Homepage", &i.homepage),
        ] {
            if let Some(u) = u {
                about.push(Fact {
                    label: label.into(),
                    value: u.clone(),
                    href: Some(u.clone()),
                });
            }
        }
    }
    let songs: Vec<ItemTrack> = tracks.iter().take(10).map(library_row).collect();
    Ok(ItemPage {
        kind: "artist".into(),
        id: format!("lib-{n}"),
        uri: canonical,
        target: serde_json::json!({ "artist_id": n }),
        title: Some(a.name.clone()),
        artist: None,
        album: None,
        year: None,
        image: {
            let matched = s
                .library
                .lock()
                .expect("library poisoned")
                .get_setting(&format!("artist.picture.{n}"))
                .ok()
                .flatten()
                .is_some();
            let own = info.as_ref().is_some_and(|i| i.image);
            if !own && !matched {
                find_artist_picture(s, n, a.name.clone());
            }
            (own || matched).then(|| format!("/v1/library/artists/{n}/image"))
        },
        source: "Your library".into(),
        source_id: "library".into(),
        plays_from: Some(PlaysFrom {
            service: "library".into(),
            kind: "artist".into(),
            id: n.to_string(),
            label: "Your library".into(),
            url: None,
            mode: "full".into(),
        }),
        connect: Vec::new(),
        reading: false,
        pending: false,
        total_ms: songs.iter().filter_map(|t| t.duration_ms).sum(),
        songs,
        albums: albums
            .iter()
            .filter_map(|al| {
                Some(ItemAlbum {
                    id: format!("lib-{}", al.id?),
                    title: al.title.clone(),
                    year: al.year,
                    image: al.id.map(cover),
                    page: String::new(),
                })
            })
            .collect(),
        versions: matches_for(s, &format!("lib:artist:{n}")),
        also_on: Vec::new(),
        opened_as: None,
        plays_note: None,
        song_uri: None,
        link: None,
        open_on: Vec::new(),
        on_pubky,
        about,
        blurb: info.and_then(|i| i.bio),
        section: "artists".into(),
        related: Vec::new(),
        concerts: Vec::new(),
    })
}

async fn external(s: &Arc<NodeState>, kind: &str, id: &str, url: &str) -> ItemPage {
    let ext = load_external(s, url).await;
    let it = &ext.it;
    let mut versions = Vec::new();
    if let Some(t) = &it.in_library {
        if let Some(tid) = t.id {
            versions.push(Version {
                page: None,
                source: "library".into(),
                id: tid.0.to_string(),
                label: format!("Your library · {}", t.file.format),
                url: String::new(),
                uri: Some(format!("track:{}", tid.0)),
            });
        }
    }
    if let (Some(a), None) = (ext.album_id, &it.in_library) {
        versions.push(Version {
            page: None,
            source: "library".into(),
            id: a.to_string(),
            label: "Your library".into(),
            url: String::new(),
            uri: None,
        });
    }
    if let Some(p) = &it.page {
        if p.starts_with("http") && !it.also_on.iter().any(|v| v.url == *p) {
            versions.push(Version {
                page: None,
                source: it
                    .site
                    .clone()
                    .unwrap_or_else(|| "link".into())
                    .to_lowercase()
                    .replace(' ', "-"),
                id: String::new(),
                label: it.site.clone().unwrap_or_else(|| "This link".into()),
                url: p.clone(),
                uri: it.playable.clone(),
            });
        }
    }
    versions.extend(it.also_on.iter().cloned());
    for m in matches_for(s, url) {
        if !versions.iter().any(|v| v.url == m.url) {
            versions.push(m);
        }
    }
    let on_pubky = match &ext.card {
        Some(r) => crate::discover::on_pubky_for(s, &r.card.uri).await,
        None => None,
    };
    let mut about = Vec::new();
    // The item itself on every source (what tags "on the song" attach to)
    let song_uri = ext.card.as_ref().and_then(|r| {
        let entity = match kind {
            "artist" => "artist",
            "album" => "release-group",
            _ => "recording",
        };
        r.mbid()
            .filter(|_| kind != "album")
            .map(|m| format!("https://musicbrainz.org/{entity}/{m}"))
            .or_else(|| {
                (kind == "song")
                    .then(|| {
                        r.isrcs()
                            .first()
                            .map(|i| format!("https://musicbrainz.org/isrc/{i}"))
                    })
                    .flatten()
            })
    });
    if let Some(r) = &ext.card {
        let f = r.facts();
        let entity = match kind {
            "artist" => "artist",
            "album" => "release",
            _ => "recording",
        };
        if let Some(m) = r.mbid() {
            about.push(mb_fact("MusicBrainz", entity, &m));
        }
        let isrcs = r.isrcs();
        if !isrcs.is_empty() {
            about.push(fact(
                "ISRC",
                isrcs.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
            ));
        }
        if let Some(u) = r.upc() {
            about.push(fact("UPC", u));
        }
        if let Some(d) = f.duration_secs() {
            about.push(fact("Length", format!("{}:{:02}", d / 60, d % 60)));
        }
        if let Some(a) = &f.album {
            about.push(fact("Album", a.clone()));
        }
        if let Some(t) = &f.artist_type {
            about.push(fact("Type", t.clone()));
        }
        if let Some(g) = &r.card.group {
            about.push(fact("Pinakes", format!("{} links in its group", g.size)));
        }
    }
    if let Some(site) = &it.site {
        about.push(Fact {
            label: "Link".into(),
            value: site.clone(),
            href: it.page.clone(),
        });
    }
    let c = ext.contents;
    let song_self = (kind == "song").then(|| ItemTrack {
        title: it.title.clone().unwrap_or_default(),
        artist: it.artist.clone(),
        page: it.page.clone(),
        uri: it.playable.clone(),
        image: it.image.clone(),
        id: Some(id.to_string()),
        ..Default::default()
    });
    ItemPage {
        kind: kind.into(),
        id: id.into(),
        uri: Some(it.uri.clone()),
        target: serde_json::json!({ "uri": it.uri }),
        title: it.title.clone(),
        artist: it.artist.clone().map(|name| PageLink { name, id: None }),
        album: None,
        year: None,
        image: it.image.clone(),
        source: it.site.clone().unwrap_or_else(|| "Pubky".into()),
        source_id: id
            .split_once('-')
            .map(|(p, _)| p)
            .filter(|p| matches!(*p, "tidal" | "deezer" | "spotify"))
            .map(str::to_string)
            .unwrap_or_else(|| {
                it.site
                    .clone()
                    .unwrap_or_default()
                    .to_lowercase()
                    .replace(' ', "-")
            }),
        plays_from: c.plays_from.clone().or_else(|| it.plays_from.clone()),
        connect: if c.connect.is_empty() {
            it.connect.clone()
        } else {
            c.connect.clone()
        },
        reading: ext.reading,
        pending: ext.pending,
        total_ms: c.total_ms,
        songs: if c.tracks.is_empty() {
            song_self.into_iter().collect()
        } else {
            c.tracks
        },
        albums: c.albums,
        open_on: open_on(&versions),
        versions,
        also_on: Vec::new(),
        opened_as: None,
        plays_note: None,
        song_uri: song_uri.or_else(|| it.uri.contains("://musicbrainz.org/").then(|| it.uri.clone())),
        link: Some(
            it.page
                .clone()
                .filter(|p| p.starts_with("http"))
                .unwrap_or_else(|| url.to_string()),
        )
        .filter(|l| !l.contains("://musicbrainz.org/")),
        on_pubky,
        about,
        blurb: it.description.clone().filter(|d| !d.is_empty() && kind != "song"),
        section: if it.in_library.is_some() { "albums" } else { "home" }.into(),
        related: Vec::new(),
        concerts: Vec::new(),
    }
}

// ---- "Find on other services": the owner's own matches (never written to Pinakes) ----

fn match_key(uri: &str) -> String {
    format!("match.{uri}")
}

/// The links the owner confirmed as the same item (local match store).
pub(crate) fn matches_for(s: &NodeState, uri: &str) -> Vec<Version> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(&match_key(uri))
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CandidateQuery {
    /// The item's canonical URL or service link; a library item without one gives `item`.
    #[serde(default)]
    pub uri: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    /// The page id (`lib-6`), so library facts (albums, ISRC, length) can back a match.
    #[serde(default)]
    pub item: Option<String>,
}

/// What a match is stored under: the item's URL, else its library id (`lib:artist:6`).
pub(crate) fn match_target(kind: &str, uri: Option<&str>, item: Option<&str>) -> Option<String> {
    if let Some(u) = uri.map(str::trim).filter(|u| !u.is_empty()) {
        return Some(u.to_string());
    }
    let n: i64 = item?.strip_prefix("lib-")?.parse().ok()?;
    matches!(kind, "song" | "album" | "artist").then(|| format!("lib:{kind}:{n}"))
}

fn norm(x: &str) -> String {
    x.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// What the library knows about an item, to weigh candidates with.
#[derive(Default)]
struct Known {
    albums: Vec<String>,
    isrc: Option<String>,
    upc: Option<String>,
    year: Option<i32>,
    duration_ms: Option<u64>,
    lib_id: Option<i64>,
}

fn known(s: &NodeState, kind: &str, item: Option<&str>) -> Known {
    let Some(n) = item
        .and_then(|i| i.strip_prefix("lib-"))
        .and_then(|n| n.parse::<i64>().ok())
    else {
        return Known::default();
    };
    let lib = s.library.lock().expect("library poisoned");
    let mut k = Known {
        lib_id: Some(n),
        ..Default::default()
    };
    match kind {
        "artist" => {
            k.albums = lib
                .artist_albums(n)
                .unwrap_or_default()
                .into_iter()
                .map(|a| a.title)
                .collect();
        }
        "album" => {
            if let Ok(Some(a)) = lib.get_album(n) {
                k.upc = a.upc.map(|u| u.to_string());
                k.year = a.year;
            }
        }
        _ => {
            if let Ok(Some(t)) = lib.get_track(TrackId(n)) {
                k.isrc = t.isrc.map(|i| i.to_string());
                k.duration_ms = t.duration_ms;
            }
        }
    }
    k
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `GET /v1/item/candidates?uri&kind&title&artist&item`: the same title (and artist) on the
/// connected services, as "possibly the same", with what tells same-named ones apart and the
/// evidence for each (an artist that has your album, the same ISRC or barcode), best first.
pub(crate) async fn candidates(
    State(s): State<Arc<NodeState>>,
    axum::extract::Query(q): axum::extract::Query<CandidateQuery>,
) -> ApiResult<Vec<MatchCandidate>> {
    let term = if q.kind == "artist" {
        q.title.trim().to_string()
    } else {
        format!("{} {}", q.artist.clone().unwrap_or_default(), q.title)
            .trim()
            .to_string()
    };
    let want = if q.kind == "song" {
        "track"
    } else {
        q.kind.as_str()
    };
    let title = norm(&q.title);
    let key = match_target(&q.kind, Some(&q.uri), q.item.as_deref());
    let have: Vec<String> = key
        .as_deref()
        .map(|k| matches_for(&s, k).into_iter().map(|v| v.url).collect())
        .unwrap_or_default();
    let k = known(&s, &q.kind, q.item.as_deref());
    let mut hits = Vec::new();
    for r in crate::streaming::unified_search(&s, &term, 10).await {
        for h in r.items {
            if h.kind != want || h.url == q.uri || have.contains(&h.url) {
                continue;
            }
            let t = norm(&h.title);
            if t.is_empty() || !(t.starts_with(&title) || title.starts_with(&t)) {
                continue;
            }
            hits.push((r.source.clone(), h));
        }
    }
    // An artist: does it have one of your albums? (each service's artist page, at once)
    let lib_albums: Vec<(String, String)> = k
        .albums
        .iter()
        .map(|a| (norm(a), a.clone()))
        .filter(|(n, _)| !n.is_empty())
        .collect();
    let artist_albums: Vec<Vec<String>> = if q.kind == "artist" && !lib_albums.is_empty() {
        let futs = hits.iter().take(8).map(|(src, h)| {
            let s = s.clone();
            let (src, id) = (src.clone(), h.id.clone());
            async move {
                let Ok(svc) = crate::streaming::find(&src) else {
                    return Vec::new();
                };
                match tokio::time::timeout(std::time::Duration::from_secs(4), svc.artist(&s, &id)).await {
                    Ok(Ok(a)) => a.albums.into_iter().map(|x| x.title).collect(),
                    _ => Vec::new(),
                }
            }
        });
        futures_util::future::join_all(futs).await
    } else {
        Vec::new()
    };
    let mut out: Vec<(usize, u64, MatchCandidate)> = hits
        .into_iter()
        .enumerate()
        .map(|(i, (src, h))| {
            let mut evidence = Vec::new();
            if let Some(albums) = artist_albums.get(i) {
                for (n, name) in &lib_albums {
                    if albums
                        .iter()
                        .any(|a| norm(a) == *n || norm(a).starts_with(n.as_str()))
                        && evidence.len() < 2
                    {
                        evidence.push(format!("has your album {name}"));
                    }
                }
            }
            if k.isrc.is_some() && h.isrc.as_deref().map(str::to_ascii_uppercase) == k.isrc {
                evidence.push("same ISRC".into());
            }
            if k.upc.is_some() && h.upc == k.upc {
                evidence.push("same barcode (UPC)".into());
            }
            if q.kind == "album" && k.year.is_some() && h.year == k.year {
                evidence.push(format!("same year {}", h.year.unwrap_or_default()));
            }
            if let (Some(a), Some(b)) = (k.duration_ms, h.duration_ms) {
                if a.abs_diff(b) <= 2000 {
                    evidence.push(format!("same length {}:{:02}", b / 60_000, b / 1000 % 60));
                }
            }
            if k.lib_id.is_some() && (h.library_album_id == k.lib_id || h.library_track_id == k.lib_id) {
                evidence.push("matches your copy".into());
            }
            // A song or album by someone else (Queen + Adam Lambert for Queen) is not the same
            let other_artist = q.kind != "artist"
                && q.artist.as_deref().is_some_and(|a| {
                    h.creator
                        .as_deref()
                        .is_some_and(|c| !norm(c).is_empty() && norm(c) != norm(a))
                });
            if other_artist {
                evidence.clear();
            }
            let fans = if other_artist { 0 } else { h.downloads.unwrap_or(0) };
            let detail = match q.kind.as_str() {
                "artist" => h.downloads.map(|n| {
                    format!(
                        "{} {}",
                        thousands(n),
                        if src == "deezer" { "fans" } else { "followers" }
                    )
                }),
                _ => {
                    let parts: Vec<String> = [h.year.map(|y| y.to_string()), h.creator.clone()]
                        .into_iter()
                        .flatten()
                        .collect();
                    (!parts.is_empty()).then(|| parts.join(" · "))
                }
            };
            let c = MatchCandidate {
                version: Version {
                    page: None,
                    source: src.clone(),
                    id: h.id.clone(),
                    label: format!(
                        "{} · {}{}",
                        crate::discover::service_title(&src),
                        h.title,
                        h.creator.clone().map(|c| format!(" · {c}")).unwrap_or_default()
                    ),
                    url: h.url.clone(),
                    uri: h.uri.clone(),
                },
                image: h.image.clone(),
                detail,
                evidence,
                likely: false,
            };
            (i, fans, c)
        })
        .collect();
    // Evidence first, then the better known one, else the services' own order
    out.sort_by(|a, b| {
        b.2.evidence
            .len()
            .cmp(&a.2.evidence.len())
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });
    let with_evidence = out.iter().filter(|c| !c.2.evidence.is_empty()).count();
    if with_evidence == 1 {
        out[0].2.likely = true;
    }
    Ok(Json(out.into_iter().map(|c| c.2).collect()))
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConfirmMatch {
    /// The item's URL; a library item without one names itself by `kind` and `id` (`lib-6`).
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    pub version: Version,
    /// The match's picture (a library artist without one shows it).
    #[serde(default)]
    pub image: Option<String>,
}

/// `POST /v1/item/matches {uri | kind+id, version, image?}`: the owner confirms "the same";
/// stored on this node only.
pub(crate) async fn confirm_match(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<ConfirmMatch>,
) -> ApiResult<Vec<Version>> {
    let kind = body.kind.clone().unwrap_or_else(|| "song".into());
    let key = match_target(&kind, body.uri.as_deref(), body.id.as_deref()).ok_or_else(|| {
        ApiError::bad_request("Couldn't tell which item to match: give its uri, or kind and id")
    })?;
    let mut v = matches_for(&s, &key);
    if !v.iter().any(|x| x.url == body.version.url) {
        let mut m = body.version.clone();
        m.label = format!("{} · matched by you", crate::discover::service_title(&m.source));
        v.push(m);
    }
    {
        let lib = s.library.lock().expect("library poisoned");
        lib.set_setting(&match_key(&key), &serde_json::to_string(&v).unwrap_or_default())?;
        // The cached contents may now come from the matched service.
        let _ = lib.delete_setting(&format!("item.contents.{key}"));
        // A library artist's picture from the match, when it has none
        if let (Some(n), Some(img)) = (
            key.strip_prefix("lib:artist:"),
            body.image.as_deref().filter(|i| i.starts_with("https://")),
        ) {
            if lib
                .get_setting(&format!("artist.picture.{n}"))
                .ok()
                .flatten()
                .is_none()
            {
                lib.set_setting(&format!("artist.picture.{n}"), img)?;
            }
        }
    }
    Ok(Json(v))
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct UnlinkMatch {
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    /// The linked source's URL.
    pub url: String,
}

/// `DELETE /v1/item/matches {uri | kind+id, url}`: "Wrong match": unlink a source the owner
/// linked by hand.
pub(crate) async fn unlink_match(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<UnlinkMatch>,
) -> ApiResult<Vec<Version>> {
    let kind = body.kind.clone().unwrap_or_else(|| "song".into());
    let key = match_target(&kind, body.uri.as_deref(), body.id.as_deref()).ok_or_else(|| {
        ApiError::bad_request("Couldn't tell which item to change: give its uri, or kind and id")
    })?;
    let mut v = matches_for(&s, &key);
    let before = v.len();
    v.retain(|x| x.url.trim_end_matches('/') != body.url.trim_end_matches('/'));
    if v.len() == before {
        return Err(ApiError::not_found(
            "That source wasn't linked by hand, so there is nothing to unlink",
        ));
    }
    let lib = s.library.lock().expect("library poisoned");
    if v.is_empty() {
        lib.delete_setting(&match_key(&key))?;
    } else {
        lib.set_setting(&match_key(&key), &serde_json::to_string(&v).unwrap_or_default())?;
    }
    let _ = lib.delete_setting(&format!("item.contents.{key}"));
    Ok(Json(v))
}

#[cfg(test)]
mod instance_tests {
    use super::*;

    fn v(source: &str, label: &str, url: &str, uri: Option<&str>) -> Version {
        Version {
            page: None,
            source: source.into(),
            id: String::new(),
            label: label.into(),
            url: url.into(),
            uri: uri.map(str::to_string),
        }
    }

    /// The owner's "Trip to Serbia" page listed TIDAL twice and MusicBrainz as a version.
    #[test]
    fn one_entry_per_source_and_never_the_page_itself() {
        let mut pg = ItemPage {
            kind: "song".into(),
            id: "lib-7".into(),
            source: "Your library".into(),
            source_id: "library".into(),
            section: "tracks".into(),
            target: serde_json::json!({}),
            versions: vec![
                v("library", "Your library · FLAC", "", Some("track:7")),
                v(
                    "tidal",
                    "TIDAL · FLAC 16/44.1",
                    "https://tidal.com/browse/track/111",
                    Some("tidal:track:111"),
                ),
                v(
                    "tidal",
                    "TIDAL · FLAC 16/44.1",
                    "https://tidal.com/browse/track/222",
                    Some("tidal:track:222"),
                ),
                v(
                    "deezer",
                    "Deezer · Trip to Serbia",
                    "https://www.deezer.com/track/9",
                    Some("deezer:track:9"),
                ),
                v(
                    "musicbrainz",
                    "MusicBrainz · tagged on Pubky",
                    "https://musicbrainz.org/recording/x",
                    None,
                ),
            ],
            ..Default::default()
        };
        split_sources(&mut pg, &["deezer".into(), "library".into(), "tidal".into()]);
        let got: Vec<(&str, Option<&str>)> = pg
            .also_on
            .iter()
            .map(|v| (v.source.as_str(), v.page.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![("deezer", Some("deezer-9")), ("tidal", Some("tidal-111"))]
        );
        assert_eq!(pg.open_on.len(), 2);
    }
}
