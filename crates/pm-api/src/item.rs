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
    // A generic id (`isrc-…`, `mb-…`) opens the best instance by the play-from order
    if kind == "song" && (id.starts_with("isrc-") || id.starts_with("mb-")) {
        if let Some((pid, url)) = best_instance(&s, &pg) {
            let mut inst = if let Some(n) = pid.strip_prefix("lib-").and_then(|n| n.parse().ok()) {
                library_song(&s, n).await?
            } else {
                external(&s, &kind, &pid, &url).await
            };
            if inst.id != pg.id {
                // Keep what the generic page knew (its sources and its About facts)
                let mut known = std::mem::take(&mut pg.versions);
                known.append(&mut inst.versions);
                inst.versions = known;
                for f in pg.about.drain(..) {
                    if !inst.about.iter().any(|x| x.label == f.label) {
                        inst.about.push(f);
                    }
                }
                if inst.on_pubky.is_none() {
                    inst.on_pubky = pg.on_pubky.take();
                }
                pg = inst;
            }
        }
        if pg.song_uri.is_none() {
            pg.song_uri = url_of_page(&kind, &id);
        }
        pg.opened_as = Some(id.clone());
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
        image: info
            .as_ref()
            .filter(|i| i.image)
            .map(|_| format!("/v1/library/artists/{n}/image")),
        source: "Your library".into(),
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
        versions: Vec::new(),
        also_on: Vec::new(),
        opened_as: None,
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
    pub uri: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
}

/// `GET /v1/item/candidates?uri&kind&title&artist`: the same title (and
/// artist) on the connected services, shown as "possibly the same".
pub(crate) async fn candidates(
    State(s): State<Arc<NodeState>>,
    axum::extract::Query(q): axum::extract::Query<CandidateQuery>,
) -> ApiResult<Vec<Version>> {
    let term = format!("{} {}", q.artist.clone().unwrap_or_default(), q.title)
        .trim()
        .to_string();
    let want = if q.kind == "song" {
        "track"
    } else {
        q.kind.as_str()
    };
    let norm = |x: &str| {
        x.to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    };
    let title = norm(&q.title);
    let have: Vec<String> = matches_for(&s, &q.uri).into_iter().map(|v| v.url).collect();
    let mut out = Vec::new();
    for r in crate::streaming::unified_search(&s, &term, 10).await {
        for h in r.items {
            if h.kind != want || h.url == q.uri || have.contains(&h.url) {
                continue;
            }
            let t = norm(&h.title);
            if !(t.starts_with(&title) || title.starts_with(&t)) {
                continue;
            }
            out.push(Version {
                page: None,
                source: r.source.clone(),
                id: h.id.clone(),
                label: format!(
                    "{} · {}{}",
                    crate::discover::service_title(&r.source),
                    h.title,
                    h.creator.map(|c| format!(" · {c}")).unwrap_or_default()
                ),
                url: h.url.clone(),
                uri: h.uri.clone(),
            });
        }
    }
    Ok(Json(out))
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConfirmMatch {
    pub uri: String,
    pub version: Version,
}

/// `POST /v1/item/matches {uri, version}`: the owner confirms "the same";
/// stored on this node only.
pub(crate) async fn confirm_match(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<ConfirmMatch>,
) -> ApiResult<Vec<Version>> {
    let mut v = matches_for(&s, &body.uri);
    if !v.iter().any(|x| x.url == body.version.url) {
        let mut m = body.version.clone();
        m.label = format!("{} · matched by you", crate::discover::service_title(&m.source));
        v.push(m);
    }
    {
        let lib = s.library.lock().expect("library poisoned");
        lib.set_setting(
            &match_key(&body.uri),
            &serde_json::to_string(&v).unwrap_or_default(),
        )?;
        // The cached contents may now come from the matched service.
        let _ = lib.delete_setting(&format!("item.contents.{}", body.uri));
    }
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
