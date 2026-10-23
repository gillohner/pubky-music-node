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
    pub q: String,
    /// `everywhere` (default), `library`, `pubky`, or a service id.
    #[serde(default, rename = "in")]
    pub scope: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// The sources a scope asks.
pub fn sources_for(s: &NodeState, scope: &str) -> Vec<String> {
    let svcs: Vec<String> = services()
        .iter()
        .filter(|x| s.source_enabled(x.id()) && x.unified(s))
        .map(|x| x.id().to_string())
        .collect();
    match scope {
        "" | "everywhere" => {
            let mut v = vec!["library".to_string(), "pubky".to_string()];
            v.extend(svcs);
            v
        }
        "library" | "pubky" => vec![scope.to_string()],
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
pub(crate) async fn search(State(s): State<Arc<NodeState>>, Query(p): Query<SearchParams>) -> Response {
    let q = p.q.trim().to_string();
    let scope = p.scope.clone().unwrap_or_default();
    let limit = p.limit.unwrap_or(20).clamp(1, 50);
    let sources = if q.is_empty() {
        Vec::new()
    } else if !hash_tags(&q).is_empty() {
        // `#tag`: tags only, yours and everyone's
        vec!["library".to_string(), "pubky".to_string()]
    } else {
        sources_for(&s, &scope)
    };
    let announced = sources.join(",");
    let (tx, rx) = tokio::sync::mpsc::channel::<SourceAnswer>(sources.len().max(1));
    for src in sources {
        let (s, q, tx) = (s.clone(), q.clone(), tx.clone());
        tokio::spawn(async move {
            let key = (q.to_lowercase(), src.clone());
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
                    let res = tokio::time::timeout(SOURCE_TIMEOUT, one_source(&s, &src, &q, limit)).await;
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

async fn one_source(s: &Arc<NodeState>, src: &str, q: &str, limit: u32) -> Result<Found, String> {
    match src {
        "library" => Ok((library(s, q, limit).map_err(|e| e.to_string())?, Vec::new())),
        "pubky" => pubky(s, q, limit).await,
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
fn song_hit(t: &pm_model::Track) -> Option<SearchHit> {
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
        ..Default::default()
    })
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

fn library(s: &NodeState, q: &str, limit: u32) -> pm_library::Result<Vec<SearchHit>> {
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
        out.push(SearchHit {
            kind: "artist".into(),
            title: a.name.clone(),
            subtitle: Some(format!("{} · your library", crate::plural(a.albums, "album"))),
            id: format!("lib-{}", a.id),
            keys: a.mbid.map(|m| vec![format!("mb:{m}")]).unwrap_or_default(),
            ..Default::default()
        });
    }
    for a in lib.search_albums(q, 12)? {
        let Some(id) = a.id else { continue };
        out.push(SearchHit {
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
        });
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

/// `#chill #90s` → `["chill", "90s"]`; none when the term has no `#` word.
pub(crate) fn hash_tags(q: &str) -> Vec<String> {
    q.split_whitespace()
        .filter(|w| w.starts_with('#') && w.len() > 1)
        .flat_map(tag_labels)
        .collect()
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
        ..Default::default()
    })
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
