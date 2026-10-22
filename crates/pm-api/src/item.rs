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

/// `GET /v1/item/{kind}/{id}`.
pub(crate) async fn page(
    State(s): State<Arc<NodeState>>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<ItemPage> {
    if !matches!(kind.as_str(), "song" | "album" | "artist") {
        return Err(ApiError::bad_request("kind is song, album or artist"));
    }
    if let Some(n) = id.strip_prefix("lib-") {
        let n: i64 = n
            .parse()
            .map_err(|_| ApiError::bad_request("lib- ids are numbers"))?;
        let pg = match kind.as_str() {
            "song" => library_song(&s, n, None).await,
            "album" => library_album(&s, n).await,
            _ => library_artist(&s, n).await,
        }?;
        return Ok(Json(pg));
    }
    let url = url_of_page(&kind, &id).ok_or_else(|| ApiError::bad_request("unknown page id"))?;
    // A song the library owns (by its canonical identity) is the library's page.
    if kind == "song" {
        if let Ok(u) = MusicUri::parse(&url) {
            let owned = s
                .library
                .lock()
                .expect("library poisoned")
                .tracks_by_canonical(&u)
                .ok()
                .and_then(|v| v.into_iter().next());
            if let Some(t) = owned.and_then(|t| t.id) {
                return library_song(&s, t.0, Some(id)).await.map(Json);
            }
        }
    }
    Ok(Json(external(&s, &kind, &id, &url).await))
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
            "song" => library_song(&s, n, None).await,
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

async fn library_song(s: &Arc<NodeState>, n: i64, id: Option<String>) -> Result<ItemPage, ApiError> {
    let t = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(n))?
        .ok_or_else(|| ApiError::not_found(format!("no track {n}")))?;
    let row = library_row(&t);
    let mut versions = vec![Version {
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
        id: id.unwrap_or_else(|| row.id.clone().unwrap_or_default()),
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
        total_ms: t.duration_ms.unwrap_or(0),
        songs: vec![row],
        albums,
        open_on: open_on(&versions),
        versions,
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
        total_ms: songs.iter().filter_map(|t| t.duration_ms).sum(),
        songs,
        albums: Vec::new(),
        open_on: open_on(&versions),
        versions,
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
        total_ms: c.total_ms,
        songs: if c.tracks.is_empty() {
            song_self.into_iter().collect()
        } else {
            c.tracks
        },
        albums: c.albums,
        open_on: open_on(&versions),
        versions,
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
