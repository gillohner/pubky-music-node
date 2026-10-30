//! One search, scoped, that never feels stuck (owner, 2026-10-21;
//! STEWARD-UX-HOME.md "Search").
//!
//! `GET /v1/search?q=&in=` asks every source of the scope in parallel and
//! streams one NDJSON line per source as soon as it answers: the library
//! first (local, instant), then Pubky (Pinakes) and each connected service.
//! A source that fails or passes its time limit sends a line saying so; the
//! others are not held back. Every hit has the same shape, so the UI renders
//! them with one row and one tile, and the same song from several sources is
//! one row (Pinakes group, ISRC or UPC). Answers are cached five minutes per
//! query and source.

use crate::state::NodeState;
use crate::streaming::{services, StreamingService};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long one source may take before it is reported as not answering.
pub const SOURCE_TIMEOUT: Duration = Duration::from_secs(8);
const CACHE_TTL: Duration = Duration::from_secs(300);

/// One result, whatever the source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SearchHit {
    /// `song`, `album`, `artist`, `playlist`, `podcast`, `concert`, `genre`.
    pub kind: String,
    pub title: String,
    /// Artist (songs, albums), date and place (concerts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The item page id (`lib-<n>`, `mb-…`, `isrc-…`, `u-…`) or the view arg.
    pub id: String,
    /// The service page or canonical URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// Queue URI when it streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    /// Identity for merging rows across sources: `g:<pinakes group>`,
    /// `isrc:<ISRC>`, `upc:<UPC>`, `mb:<mbid>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Found through a Pubky tag: the label (the row says "tagged #chill").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagged: Option<String>,
    /// A library file's format in words: `FLAC 16/44.1`, `MP3 320 kbps`, `DSD64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// `hi_res`, `lossless` or `lossy` (library files); a facet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// The site a Pubky card is from (`tidal.com`, `open.spotify.com`); a facet and a badge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

/// One source's answer (one NDJSON line).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SourceAnswer {
    /// `library`, `pubky`, `tidal`, `deezer`, `spotify`, …
    pub source: String,
    /// `ok`, `error` or `timeout`.
    pub status: String,
    pub ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub hits: Vec<SearchHit>,
    /// Counts by kind over the whole match (Pinakes's `facet=type`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counts: Vec<(String, i64)>,
    /// Served from the five-minute cache.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: String,
    /// `everywhere` (default), `library`, `pubky`, or a service id.
    #[serde(default, rename = "in")]
    pub scope: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Tag labels, comma-separated: the same as `#label` words in `q` (several: all of them).
    #[serde(default)]
    pub tags: Option<String>,
}

/// `q` with the `tags` parameter folded in as `#label` words, so one grammar serves both.
pub(crate) fn query_with_tags(p: &SearchParams) -> String {
    let mut q = p.q.trim().to_string();
    for label in p.tags.iter().flat_map(|t| t.split(',')).flat_map(tag_labels) {
        q.push_str(" #");
        q.push_str(&label);
    }
    q.trim().to_string()
}

/// The sources a scope asks.
pub fn sources_for(s: &NodeState, scope: &str) -> Vec<String> {
    let svcs: Vec<String> = services()
        .iter()
        .filter(|x| s.source_enabled(x.id()) && x.unified(s))
        .map(|x| x.id().to_string())
        .collect();
    // People and posts come from Nexus, which the node reaches through its discovery setup
    let social: Vec<String> = if s.discovery().is_some() {
        vec!["people".to_string(), "posts".to_string()]
    } else {
        Vec::new()
    };
    match scope {
        "" | "everywhere" => {
            let mut v = vec!["library".to_string(), "pubky".to_string()];
            v.extend(social);
            v.extend(svcs);
            v
        }
        "pubky" => {
            let mut v = vec!["pubky".to_string()];
            v.extend(social);
            v
        }
        "library" => vec![scope.to_string()],
        other if services().iter().any(|x| x.id() == other) && s.source_enabled(other) => {
            vec![other.to_string()]
        }
        _ => vec!["library".to_string()],
    }
}

/// Answers by (query, source), with when they were fetched.
type AnswerCache = Mutex<HashMap<(String, String), (Instant, SourceAnswer)>>;

/// Drop cached answers (a source was switched on or off).
pub(crate) fn forget_answers() {
    cache().lock().expect("search cache").clear();
}

fn cache() -> &'static AnswerCache {
    static C: OnceLock<AnswerCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// `GET /v1/search?q=&in=`: NDJSON, one line per source as it answers.
pub(crate) async fn search(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<crate::accounts::Caller>,
    Query(p): Query<SearchParams>,
) -> Response {
    let owner = c.role == crate::accounts::Role::Owner;
    let q = query_with_tags(&p);
    let scope = p.scope.clone().unwrap_or_default();
    let limit = p.limit.unwrap_or(20).clamp(1, 50);
    let sources = if q.is_empty() {
        Vec::new()
    } else if !hash_tags(&q).is_empty() {
        // `#tag`: tags only, yours and everyone's (posts carrying the tag too, unless the scope is
        // the library)
        let mut v = vec!["library".to_string(), "pubky".to_string()];
        if s.discovery().is_some() && matches!(scope.as_str(), "" | "everywhere" | "pubky") {
            v.push("posts".to_string());
        }
        v
    } else {
        sources_for(&s, &scope)
    };
    let announced = sources.join(",");
    let (tx, rx) = tokio::sync::mpsc::channel::<SourceAnswer>(sources.len().max(1));
    for src in sources {
        let (s, q, tx) = (s.clone(), q.clone(), tx.clone());
        tokio::spawn(async move {
            // The library's answer differs for a listener (no owner's playlists): cached apart
            let key = (
                q.to_lowercase(),
                if owner {
                    src.clone()
                } else {
                    format!("{src}|listener")
                },
            );
            let hit = cache()
                .lock()
                .expect("search cache")
                .get(&key)
                .filter(|(at, _)| at.elapsed() < CACHE_TTL)
                .map(|(_, a)| a.clone());
            let answer = match hit {
                Some(mut a) => {
                    a.cached = true;
                    a
                }
                None => {
                    let started = Instant::now();
                    let res =
                        tokio::time::timeout(SOURCE_TIMEOUT, one_source(&s, &src, &q, limit, owner)).await;
                    let ms = started.elapsed().as_millis() as u64;
                    let a = match res {
                        Ok(Ok((hits, counts))) => SourceAnswer {
                            source: src.clone(),
                            status: "ok".into(),
                            ms,
                            hits,
                            counts,
                            ..Default::default()
                        },
                        Ok(Err(e)) => SourceAnswer {
                            source: src.clone(),
                            status: "error".into(),
                            ms,
                            error: Some(e),
                            ..Default::default()
                        },
                        Err(_) => SourceAnswer {
                            source: src.clone(),
                            status: "timeout".into(),
                            ms,
                            error: Some(format!("no answer within {} s", SOURCE_TIMEOUT.as_secs())),
                            ..Default::default()
                        },
                    };
                    if a.status == "ok" {
                        cache()
                            .lock()
                            .expect("search cache")
                            .insert(key, (Instant::now(), a.clone()));
                    }
                    a
                }
            };
            let _ = tx.send(answer).await;
        });
    }
    drop(tx);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let a = rx.recv().await?;
        let mut line = serde_json::to_vec(&a).unwrap_or_default();
        line.push(b'\n');
        Some((Ok::<_, std::io::Error>(bytes::Bytes::from(line)), rx))
    });
    (
        [
            (header::CONTENT_TYPE, "application/x-ndjson".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
            // Which sources will answer, so the UI can show them pending at once.
            (header::HeaderName::from_static("x-pm-sources"), announced),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

type Found = (Vec<SearchHit>, Vec<(String, i64)>);

async fn one_source(
    s: &Arc<NodeState>,
    src: &str,
    q: &str,
    limit: u32,
    owner: bool,
) -> Result<Found, String> {
    match src {
        "library" => Ok((
            library(s, q, limit, owner).map_err(|e| e.to_string())?,
            Vec::new(),
        )),
        "pubky" => pubky(s, q, limit).await,
        "people" => people(s, q, limit).await,
        "posts" => posts(s, q, limit).await,
        other => {
            let svc = services()
                .into_iter()
                .find(|x| x.id() == other)
                .ok_or_else(|| format!("no source {other}"))?;
            service(s, svc.as_ref(), q, limit).await
        }
    }
}

/// A library song as a search row.
pub(crate) fn song_hit(t: &pm_model::Track) -> Option<SearchHit> {
    let tid = t.id?;
    let mut keys = Vec::new();
    if let Some(i) = &t.isrc {
        keys.push(format!("isrc:{i}"));
    }
    if let Some(m) = t.recording_mbid {
        keys.push(format!("mb:{m}"));
    }
    Some(SearchHit {
        kind: "song".into(),
        title: t.title.clone(),
        subtitle: Some(
            t.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
        .filter(|x| !x.is_empty()),
        image: t
            .album
            .as_ref()
            .and_then(|a| a.id)
            .map(|i| format!("/v1/library/albums/{i}/cover")),
        id: crate::item::track_page_id(t),
        track_id: Some(tid.0),
        artist_id: t.artists.first().and_then(|a| a.id).map(|i| format!("lib-{i}")),
        album: t.album.as_ref().map(|a| a.title.clone()),
        album_id: t.album.as_ref().and_then(|a| a.id).map(|i| format!("lib-{i}")),
        duration_ms: t.duration_ms,
        keys,
        format: Some(file_format(&t.file)),
        quality: Some(file_quality(&t.file).into()),
        ..Default::default()
    })
}

/// A library artist as a result.
pub(crate) fn artist_hit(a: &pm_library::ArtistSummary) -> SearchHit {
    SearchHit {
        kind: "artist".into(),
        title: a.name.clone(),
        subtitle: Some(format!("{} · your library", crate::plural(a.albums, "album"))),
        id: format!("lib-{}", a.id),
        keys: a.mbid.map(|m| vec![format!("mb:{m}")]).unwrap_or_default(),
        ..Default::default()
    }
}

/// A library album as a result.
pub(crate) fn album_hit(a: &pm_model::Album) -> Option<SearchHit> {
    let id = a.id?;
    Some(SearchHit {
        kind: "album".into(),
        title: a.title.clone(),
        subtitle: a.album_artist.clone(),
        image: Some(format!("/v1/library/albums/{id}/cover")),
        id: format!("lib-{id}"),
        year: a.year,
        keys: a
            .upc
            .as_ref()
            .map(|u| vec![format!("upc:{u}")])
            .unwrap_or_default(),
        ..Default::default()
    })
}

/// A file's format in words: `FLAC 16/44.1`, `MP3 320 kbps` (no bitrate known: `MP3`), `DSD64`.
pub(crate) fn file_format(f: &pm_model::AudioFile) -> String {
    let fm = &f.format;
    if matches!(fm.kind, pm_model::SampleKind::Dsd) {
        return format!("DSD{}", (fm.rate_hz / 44_100).max(1));
    }
    let codec = f.container.to_uppercase();
    if file_quality(f) == "lossy" {
        return codec;
    }
    let k = fm.rate_hz as f64 / 1000.0;
    let rate = format!("{:.1}", k).trim_end_matches(".0").to_string();
    format!("{codec} {}/{rate}", fm.bits)
}

/// `hi_res`, `lossless` or `lossy`, from the container and the sample format.
pub(crate) fn file_quality(f: &pm_model::AudioFile) -> &'static str {
    let fm = &f.format;
    let lossy = pm_library::tags::is_lossy_container(&f.container.to_lowercase());
    if lossy {
        "lossy"
    } else if matches!(fm.kind, pm_model::SampleKind::Dsd) || fm.bits > 16 || fm.rate_hz > 48_000 {
        "hi_res"
    } else {
        "lossless"
    }
}

/// `#chill #90s` in the library: what you tagged with all of them (your tags table, offline).
fn library_tagged(
    lib: &pm_library::Library,
    labels: &[String],
    limit: u32,
) -> pm_library::Result<Vec<SearchHit>> {
    let mut by_uri: HashMap<String, Vec<String>> = HashMap::new();
    for t in lib
        .list_tags(None)?
        .into_iter()
        .filter(|t| labels.contains(&t.label))
    {
        by_uri.entry(t.uri).or_default().push(t.label);
    }
    let shown = labels
        .iter()
        .map(|l| format!("#{l}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = Vec::new();
    for (uri, got) in by_uri {
        if !labels.iter().all(|l| got.contains(l)) {
            continue;
        }
        let Ok(m) = pm_model::MusicUri::parse(&uri) else {
            continue;
        };
        if let pm_model::MusicUri::Release { mbid } = &m {
            if let Some(a) = lib.album_by_release_mbid(*mbid)? {
                if let Some(id) = a.id {
                    out.push(SearchHit {
                        kind: "album".into(),
                        title: a.title.clone(),
                        subtitle: a.album_artist.clone(),
                        image: Some(format!("/v1/library/albums/{id}/cover")),
                        id: format!("lib-{id}"),
                        tagged: Some(shown.clone()),
                        ..Default::default()
                    });
                }
            }
            continue;
        }
        for t in lib.tracks_by_canonical(&m)?.into_iter().take(1) {
            if let Some(mut h) = song_hit(&t) {
                h.tagged = Some(shown.clone());
                out.push(h);
            }
        }
        if out.len() >= limit as usize {
            break;
        }
    }
    Ok(out)
}

/// `owner`: the node's own playlists are the owner's; a listener's search leaves them out.
fn library(s: &NodeState, q: &str, limit: u32, owner: bool) -> pm_library::Result<Vec<SearchHit>> {
    let lib = s.library.lock().expect("library poisoned");
    let hashed = hash_tags(q);
    if !hashed.is_empty() {
        return library_tagged(&lib, &hashed, limit);
    }
    let needle = q.to_lowercase();
    let mut out = Vec::new();
    for a in lib
        .list_artists()?
        .into_iter()
        .filter(|a| a.name.to_lowercase().contains(&needle))
        .take(8)
    {
        out.push(artist_hit(&a));
    }
    let own_playlists = if owner {
        crate::playlists::summaries_named(&lib, &needle, 6).unwrap_or_default()
    } else {
        Vec::new()
    };
    for pl in own_playlists {
        out.push(SearchHit {
            kind: "playlist".into(),
            title: pl.name.clone(),
            subtitle: Some(format!(
                "{} · your playlist",
                crate::plural(pl.items as u64, "song")
            )),
            image: pl.image.clone(),
            id: pl.id.clone(),
            ..Default::default()
        });
    }
    for a in lib.search_albums(q, 12)? {
        out.extend(album_hit(&a));
    }
    for t in lib.search_tracks(q, limit)? {
        out.extend(song_hit(&t));
    }
    Ok(out)
}

fn kind_of_type(t: Option<&str>) -> Option<&'static str> {
    Some(match t? {
        "MusicRecording" | "MusicComposition" => "song",
        "MusicAlbum" | "MusicRelease" => "album",
        "MusicGroup" | "Person" => "artist",
        "MusicPlaylist" => "playlist",
        "PodcastSeries" | "PodcastEpisode" => "podcast",
        "MusicEvent" => "concert",
        _ => return None,
    })
}

/// A search term as pubky.app tag labels: lowercase, no spaces or commas, 1–20 characters; a
/// multi-word term also hyphenated ("bohemian rhapsody" → `bohemian-rhapsody`).
pub(crate) fn tag_labels(term: &str) -> Vec<String> {
    let clean = |x: &str| -> String {
        x.trim()
            .trim_start_matches('#')
            .to_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace() && *c != ',')
            .collect()
    };
    let words: Vec<&str> = term.split_whitespace().collect();
    let mut out = Vec::new();
    for l in [clean(term), clean(&words.join("-"))] {
        if !l.is_empty() && l.chars().count() <= 20 && !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// `#chill #90s` (or `tag:chill tag:90s`) → `["chill", "90s"]`; none when the term has no tag word.
pub(crate) fn hash_tags(q: &str) -> Vec<String> {
    q.split_whitespace()
        .filter_map(|w| {
            if w.starts_with('#') && w.len() > 1 {
                Some(w)
            } else {
                w.strip_prefix("tag:").filter(|r| !r.is_empty())
            }
        })
        .flat_map(tag_labels)
        .collect()
}

/// The words of `q` that are not tags (`#x` or `tag:x`): what text search gets.
pub(crate) fn text_words(q: &str) -> String {
    q.split_whitespace()
        .filter(|w| !(w.starts_with('#') && w.len() > 1) && !w.starts_with("tag:"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn card_hit(c: &pm_pubky::pinakes::Card) -> Option<SearchHit> {
    if !c.is_ok() {
        return None;
    }
    let kind = kind_of_type(c.type_.as_deref())?;
    let mut keys = Vec::new();
    if let Some(g) = &c.group {
        keys.push(format!("g:{}", g.id));
    }
    let subtitle = if kind == "concert" {
        c.start.clone().map(|st| st.get(..10).unwrap_or(&st).to_string())
    } else {
        c.artist_hint().or_else(|| c.site.clone())
    };
    Some(SearchHit {
        kind: kind.into(),
        title: c
            .name
            .clone()
            .unwrap_or_else(|| format!("Untitled on {}", c.site.clone().unwrap_or_default())),
        subtitle,
        image: c.image.clone(),
        id: crate::discover::page_id(&c.uri),
        uri: crate::discover::playable_uri(&c.uri),
        page: Some(c.uri.clone()),
        keys,
        site: c.site.clone(),
        ..Default::default()
    })
}

/// People on Pubky whose name starts with the term (Nexus).
/// Until when Nexus asked the node to wait (a 429): people and posts aren't asked meanwhile, so a
/// search per keystroke doesn't keep it busy.
fn nexus_wait() -> &'static Mutex<Option<Instant>> {
    static WAIT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    WAIT.get_or_init(|| Mutex::new(None))
}

const NEXUS_BUSY: &str = "Pubky search is busy: try again in a few seconds";

fn nexus_ready() -> Result<(), String> {
    match *nexus_wait().lock().expect("nexus wait") {
        Some(until) if Instant::now() < until => Err(NEXUS_BUSY.into()),
        _ => Ok(()),
    }
}

fn nexus_error(e: pm_pubky::nexus::NexusError) -> String {
    if let pm_pubky::nexus::NexusError::RateLimited(after) = e {
        let wait = after
            .unwrap_or(Duration::from_secs(10))
            .min(Duration::from_secs(120));
        *nexus_wait().lock().expect("nexus wait") = Some(Instant::now() + wait);
        return NEXUS_BUSY.into();
    }
    e.to_string()
}

async fn people(s: &Arc<NodeState>, q: &str, limit: u32) -> Result<Found, String> {
    nexus_ready()?;
    let d = s
        .discovery()
        .ok_or_else(|| "People on Pubky need the upstream contact (Settings)".to_string())?;
    let term = text_words(q);
    if term.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if term.chars().count() < 2 {
        return Ok((Vec::new(), Vec::new()));
    }
    let users = match d.nexus().users_by_name(&term, limit.min(10)).await {
        Ok(u) => u,
        Err(pm_pubky::nexus::NexusError::NotFound) => Vec::new(),
        Err(e) => return Err(nexus_error(e)),
    };
    let hits = users
        .into_iter()
        .map(|u| {
            let id = u.details.id.clone();
            let bio = u.details.bio.clone().unwrap_or_default();
            let bio = bio.trim();
            SearchHit {
                kind: "person".into(),
                title: u.details.name.clone(),
                subtitle: Some(if bio.is_empty() {
                    "on Pubky".to_string()
                } else {
                    bio.chars().take(80).collect()
                }),
                image: u
                    .details
                    .image
                    .as_ref()
                    .map(|_| format!("/v1/profiles/{id}/avatar")),
                id: id.clone(),
                page: Some(format!("https://pubky.app/profile/{id}")),
                uri: Some(format!("pubky://{id}")),
                ..Default::default()
            }
        })
        .collect();
    Ok((hits, Vec::new()))
}

/// Posts on Pubky whose text matches the term, or that carry the `#tag` (Nexus).
async fn posts(s: &Arc<NodeState>, q: &str, limit: u32) -> Result<Found, String> {
    nexus_ready()?;
    let d = s
        .discovery()
        .ok_or_else(|| "Posts on Pubky need the upstream contact (Settings)".to_string())?;
    let nexus = d.nexus();
    let found = match hash_tags(q).first() {
        Some(label) => nexus.posts_by_tag(label, limit.min(10)).await,
        None => {
            // Nexus searches 2 to 30 characters and at most 4 words
            let term: String = text_words(q)
                .split_whitespace()
                .take(4)
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(30)
                .collect();
            if term.trim().chars().count() < 2 {
                return Ok((Vec::new(), Vec::new()));
            }
            nexus.posts_by_content(term.trim(), limit.min(10)).await
        }
    };
    let keys = match found {
        Ok(k) => k,
        Err(pm_pubky::nexus::NexusError::NotFound) => Vec::new(),
        Err(e) => return Err(nexus_error(e)),
    };
    let views = nexus.posts_by_ids(&keys).await.map_err(|e| e.to_string())?;
    let authors: Vec<String> = {
        let mut a: Vec<String> = views.iter().map(|p| p.details.author.clone()).collect();
        a.sort();
        a.dedup();
        a
    };
    let names: HashMap<String, String> = nexus
        .users_by_ids(&authors, None)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|u| (u.details.id.clone(), u.details.name.clone()))
        .collect();
    let hits = views
        .into_iter()
        .map(|p| {
            let d = p.details;
            let first = d
                .content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            let title: String = if first.is_empty() {
                "Untitled post".to_string()
            } else if first.chars().count() > 120 {
                format!("{}…", first.chars().take(119).collect::<String>())
            } else {
                first.to_string()
            };
            let by = names.get(&d.author).cloned().unwrap_or_else(|| "someone".into());
            SearchHit {
                kind: "post".into(),
                title,
                // The kind when it says more than "a post" (Nexus' social kind is "" when unknown)
                subtitle: Some(match d.kind.as_str() {
                    "" | "short" | "long" => by,
                    k => format!("{by} · {k}"),
                }),
                id: format!("post-{}:{}", d.author, d.id),
                page: Some(format!("https://pubky.app/post/{}/{}", d.author, d.id)),
                uri: Some(d.uri.clone()).filter(|u| !u.is_empty()),
                ..Default::default()
            }
        })
        .collect();
    Ok((hits, Vec::new()))
}

/// Pubky: the text query and, in parallel, the term as tags (STEWARD-FIX-search-tags.md); a
/// `#tag` term searches tags only (several: all of them).
async fn pubky(s: &Arc<NodeState>, q: &str, limit: u32) -> Result<Found, String> {
    let d = s
        .discovery()
        .ok_or_else(|| "Pubky search needs the upstream contact (Settings)".to_string())?;
    let hashed = hash_tags(q);
    let tags_only = !hashed.is_empty();
    let labels = if tags_only { hashed } else { tag_labels(q) };
    let text = async {
        if tags_only {
            return Ok(None);
        }
        d.pinakes()
            .search(&pm_pubky::pinakes::SearchQuery {
                q: Some(q.to_string()),
                collapse: Some("group".into()),
                facet: Some("type".into()),
                limit: Some(limit as usize),
                ..Default::default()
            })
            .await
            .map(Some)
    };
    let by_tag = async {
        if labels.is_empty() {
            return Ok(None);
        }
        d.pinakes()
            .search(&pm_pubky::pinakes::SearchQuery {
                tags: Some(labels.join(",")),
                match_: (tags_only && labels.len() > 1).then(|| "all".to_string()),
                collapse: Some("group".into()),
                facet: tags_only.then(|| "type".to_string()),
                limit: Some(limit as usize),
                ..Default::default()
            })
            .await
            .map(Some)
    };
    let (text, by_tag) = tokio::join!(text, by_tag);
    // The text query decides failure; a tag query that fails only loses the tag hits
    let text = text.map_err(|e| e.to_string())?;
    let by_tag = match by_tag {
        Ok(p) => p,
        Err(e) if tags_only => return Err(e.to_string()),
        Err(_) => None,
    };
    let facet_page = text.as_ref().or(by_tag.as_ref());
    let counts = facet_page
        .and_then(|p| p.facets.as_ref())
        .map(|f| {
            f.types
                .iter()
                .filter_map(|t| Some((kind_of_type(Some(&t.type_))?.to_string(), t.count)))
                .collect()
        })
        .unwrap_or_default();
    let mut hits: Vec<SearchHit> = text
        .as_ref()
        .map(|p| p.items.iter().filter_map(card_hit).collect())
        .unwrap_or_default();
    let shown = if tags_only {
        labels
            .iter()
            .map(|l| format!("#{l}"))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        format!("#{}", labels.first().cloned().unwrap_or_default())
    };
    for mut h in by_tag
        .as_ref()
        .map(|p| p.items.iter().filter_map(card_hit).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        // One row per group: a text hit that is also tagged says so
        if let Some(prev) = hits
            .iter_mut()
            .find(|x| x.page == h.page || (!h.keys.is_empty() && x.keys.iter().any(|k| h.keys.contains(k))))
        {
            prev.tagged.get_or_insert(shown.clone());
            continue;
        }
        h.tagged = Some(shown.clone());
        hits.push(h);
    }
    Ok((hits, counts))
}

/// `GET /v1/search/tags?prefix=`: tag labels as you type: yours first (with how many items you
/// tagged), then everyone's on Pubky (Nexus).
pub(crate) async fn tag_suggestions(
    State(s): State<Arc<NodeState>>,
    Query(p): Query<std::collections::HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    let prefix: String = p
        .get("prefix")
        .map(|x| x.trim().trim_start_matches('#').to_lowercase())
        .unwrap_or_default();
    if prefix.is_empty() {
        return axum::Json(serde_json::json!({"mine": [], "others": []}));
    }
    let mut mine: Vec<(String, usize)> = Vec::new();
    if let Ok(rows) = s.library.lock().expect("library poisoned").list_tags(None) {
        for t in rows.into_iter().filter(|t| t.label.starts_with(&prefix)) {
            match mine.iter_mut().find(|(l, _)| *l == t.label) {
                Some(m) => m.1 += 1,
                None => mine.push((t.label, 1)),
            }
        }
    }
    mine.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let others: Vec<String> = match s.discovery() {
        Some(d) => tokio::time::timeout(
            std::time::Duration::from_secs(2),
            d.nexus().tags_by_prefix(&prefix, 8),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|l| !mine.iter().any(|(m, _)| m == l))
        .collect(),
        None => Vec::new(),
    };
    axum::Json(serde_json::json!({
        "mine": mine.into_iter().take(8).map(|(label, count)| serde_json::json!({"label": label, "count": count})).collect::<Vec<_>>(),
        "others": others,
    }))
}

async fn service(
    s: &Arc<NodeState>,
    svc: &dyn StreamingService,
    q: &str,
    limit: u32,
) -> Result<Found, String> {
    let hits = svc.search(s, q, limit).await.map_err(|e| e.message)?;
    let lib = s.library.lock().expect("library poisoned");
    let mut out = Vec::new();
    for mut h in hits {
        crate::streaming::mark_library(&lib, &mut h);
        let kind = match h.kind.as_str() {
            "track" => "song",
            "album" | "item" => "album",
            "artist" => "artist",
            "playlist" => "playlist",
            _ => continue,
        };
        let mut keys = Vec::new();
        if let Some(i) = &h.isrc {
            keys.push(format!("isrc:{i}"));
        }
        if let Some(u) = &h.upc {
            keys.push(format!("upc:{u}"));
        }
        let id = match (kind, h.library_track_id, h.library_album_id) {
            ("song", Some(t), _) => format!("lib-{t}"),
            ("album", _, Some(a)) => format!("lib-{a}"),
            _ => crate::discover::page_id(&h.url),
        };
        out.push(SearchHit {
            kind: kind.into(),
            title: h.title.clone(),
            subtitle: h.creator.clone(),
            image: h.image.clone(),
            id,
            page: Some(h.url.clone()),
            uri: h.uri.clone(),
            track_id: h.library_track_id,
            duration_ms: h.duration_ms,
            year: h.year,
            keys,
            ..Default::default()
        });
    }
    Ok((out, Vec::new()))
}
