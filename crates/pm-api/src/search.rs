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
        .filter(|x| x.unified(s))
        .map(|x| x.id().to_string())
        .collect();
    match scope {
        "" | "everywhere" => {
            let mut v = vec!["library".to_string(), "pubky".to_string()];
            v.extend(svcs);
            v
        }
        "library" | "pubky" => vec![scope.to_string()],
        other if services().iter().any(|x| x.id() == other) => vec![other.to_string()],
        _ => vec!["library".to_string()],
    }
}

/// Answers by (query, source), with when they were fetched.
type AnswerCache = Mutex<HashMap<(String, String), (Instant, SourceAnswer)>>;

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

fn library(s: &NodeState, q: &str, limit: u32) -> pm_library::Result<Vec<SearchHit>> {
    let lib = s.library.lock().expect("library poisoned");
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
        let Some(tid) = t.id else { continue };
        let mut keys = Vec::new();
        if let Some(i) = &t.isrc {
            keys.push(format!("isrc:{i}"));
        }
        if let Some(m) = t.recording_mbid {
            keys.push(format!("mb:{m}"));
        }
        out.push(SearchHit {
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
            id: crate::item::track_page_id(&t),
            track_id: Some(tid.0),
            artist_id: t.artists.first().and_then(|a| a.id).map(|i| format!("lib-{i}")),
            album: t.album.as_ref().map(|a| a.title.clone()),
            album_id: t.album.as_ref().and_then(|a| a.id).map(|i| format!("lib-{i}")),
            duration_ms: t.duration_ms,
            keys,
            ..Default::default()
        });
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

async fn pubky(s: &Arc<NodeState>, q: &str, limit: u32) -> Result<Found, String> {
    let d = s
        .discovery()
        .ok_or_else(|| "Pubky search needs the upstream contact (Settings)".to_string())?;
    let page = d
        .pinakes()
        .search(&pm_pubky::pinakes::SearchQuery {
            q: Some(q.to_string()),
            collapse: Some("group".into()),
            facet: Some("type".into()),
            limit: Some(limit as usize),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;
    let counts = page
        .facets
        .as_ref()
        .map(|f| {
            f.types
                .iter()
                .filter_map(|t| Some((kind_of_type(Some(&t.type_))?.to_string(), t.count)))
                .collect()
        })
        .unwrap_or_default();
    let hits = page
        .items
        .into_iter()
        .filter(|c| c.is_ok())
        .filter_map(|c| {
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
        })
        .collect();
    Ok((hits, counts))
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
