//! Streaming sources (handoff M12): anything that can be searched and
//! yields tracks with stream URLs the zones can play. Account-free services
//! are built in; subscription services plug in here once their accounts
//! and terms exist. One personal account per service, never proxied for
//! several users (constraint 3).

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::Json;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait StreamingService: Send + Sync {
    /// Stable id used in URLs (`archive`, `audius`, `tidal`, `deezer`).
    fn id(&self) -> &'static str;
    fn info(&self, s: &NodeState) -> SourceInfo;
    /// Whether the main search should query this service alongside the library.
    fn unified(&self, _s: &NodeState) -> bool {
        false
    }
    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError>;
    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError>;
    /// An artist page (services that have artists).
    async fn artist(&self, _s: &NodeState, id: &str) -> Result<SourceArtist, ApiError> {
        Err(ApiError::not_found(format!("no artist {id} on this source")))
    }
}

/// The built-in services, in display order.
pub fn services() -> Vec<Box<dyn StreamingService>> {
    vec![
        Box::new(crate::archive::InternetArchive),
        Box::new(crate::audius::Audius),
        Box::new(crate::tidal::Tidal),
        Box::new(crate::deezer::Deezer),
        Box::new(crate::spotify::Spotify),
        Box::new(crate::jamendo::Jamendo),
        Box::new(crate::opensubsonic::OpenSubsonic),
        Box::new(crate::jellyfin::Jellyfin),
        Box::new(crate::plex::PlexSource),
        Box::new(crate::podcasts::Podcasts),
        Box::new(crate::radio_sources::Radio),
    ]
}

pub(crate) fn find(id: &str) -> Result<Box<dyn StreamingService>, ApiError> {
    services()
        .into_iter()
        .find(|svc| svc.id() == id)
        .ok_or_else(|| ApiError::not_found(format!("no source {id}")))
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct SourceSearch {
    pub q: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A source as the Sources page lists it: its info and whether it is switched on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceRow {
    #[serde(flatten)]
    pub info: SourceInfo,
    pub enabled: bool,
}

/// `GET /v1/sources`.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SourceRow>> {
    Ok(Json(
        services()
            .iter()
            .map(|svc| SourceRow {
                info: svc.info(&s),
                enabled: s.source_enabled(svc.id()),
            })
            .collect(),
    ))
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct EnabledBody {
    pub enabled: bool,
}

/// `PUT /v1/sources/{source}/enabled` `{enabled}`: switch a source on or off. Off hides it
/// everywhere; its sign-in and settings are kept.
pub(crate) async fn set_enabled(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    Json(b): Json<EnabledBody>,
) -> ApiResult<SourceRow> {
    let svc = find(&source)?;
    s.set_source_enabled(svc.id(), b.enabled)
        .map_err(|e| ApiError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    crate::search::forget_answers();
    Ok(Json(SourceRow {
        info: svc.info(&s),
        enabled: b.enabled,
    }))
}

/// A source that is switched on, for routes that search or browse it.
fn find_on(s: &NodeState, id: &str) -> Result<Box<dyn StreamingService>, ApiError> {
    let svc = find(id)?;
    if !s.source_enabled(id) {
        return Err(ApiError::not_found(format!("{id} is turned off on Sources")));
    }
    Ok(svc)
}

/// `GET /v1/sources/{source}/search?q=&limit=`.
pub(crate) async fn search(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    Query(q): Query<SourceSearch>,
) -> ApiResult<SourceResults> {
    let svc = find_on(&s, &source)?;
    let term = q.q.trim();
    if term.is_empty() {
        return Err(ApiError::bad_request("q is required"));
    }
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    let items = svc.search(&s, term, limit).await?;
    Ok(Json(SourceResults { source, items }))
}

/// Library search companion: every unified service, queried concurrently with a
/// time budget, hits flagged with the library items that share their identity.
pub(crate) async fn unified_search(s: &Arc<NodeState>, term: &str, limit: u32) -> Vec<SourceResults> {
    let svcs: Vec<Box<dyn StreamingService>> = services()
        .into_iter()
        .filter(|x| s.source_enabled(x.id()) && x.unified(s))
        .collect();
    let mut out = Vec::new();
    let futs = svcs.iter().map(|svc| {
        let s = s.clone();
        let term = term.to_string();
        async move {
            let id = svc.id().to_string();
            match tokio::time::timeout(std::time::Duration::from_secs(6), svc.search(&s, &term, limit)).await
            {
                Ok(Ok(items)) => Some(SourceResults { source: id, items }),
                Ok(Err(e)) => {
                    tracing::debug!(source = %id, "unified search skipped: {}", e.message);
                    None
                }
                Err(_) => {
                    tracing::debug!(source = %id, "unified search timed out");
                    None
                }
            }
        }
    });
    for r in futures_util::future::join_all(futs).await.into_iter().flatten() {
        out.push(r);
    }
    let lib = s.library.lock().expect("library poisoned");
    for r in &mut out {
        for h in &mut r.items {
            mark_library(&lib, h);
        }
    }
    out
}

/// Fill `library_track_id` / `library_album_id` from the hit's ISRC or UPC.
pub(crate) fn mark_library(lib: &pm_library::Library, h: &mut SourceHit) {
    if h.kind == "artist" && h.library_artist_id.is_none() {
        h.library_artist_id = lib
            .list_artists()
            .ok()
            .and_then(|all| all.into_iter().find(|x| x.name.eq_ignore_ascii_case(&h.title)))
            .map(|x| x.id);
        return;
    }
    if let Some(isrc) = h.isrc.as_deref().and_then(|i| pm_model::Isrc::parse(i).ok()) {
        if let Ok(ts) = lib.tracks_by_canonical(&pm_model::MusicUri::Isrc { isrc }) {
            h.library_track_id = ts.first().and_then(|t| t.id.map(|i| i.0));
            if h.library_album_id.is_none() {
                h.library_album_id = ts.first().and_then(|t| t.album.as_ref().and_then(|a| a.id));
            }
        }
    }
    if let Some(upc) = h.upc.as_deref().and_then(|u| pm_model::Upc::parse(u).ok()) {
        if let Ok(Some(a)) = lib.album_by_upc(&upc) {
            h.library_album_id = a.id;
        }
    }
}

/// `GET /v1/sources/{source}/artists/{id}`.
pub(crate) async fn artist(
    State(s): State<Arc<NodeState>>,
    Path((source, id)): Path<(String, String)>,
) -> ApiResult<SourceArtist> {
    let svc = find(&source)?;
    let mut a = svc.artist(&s, &id).await?;
    let lib = s.library.lock().expect("library poisoned");
    for h in &mut a.albums {
        mark_library(&lib, h);
    }
    if a.library_artist_id.is_none() {
        a.library_artist_id = lib
            .list_artists()
            .ok()
            .and_then(|all| all.into_iter().find(|x| x.name.eq_ignore_ascii_case(&a.name)))
            .map(|x| x.id);
    }
    Ok(Json(a))
}

/// `GET /v1/sources/{source}/items/{id}`.
pub(crate) async fn item(
    State(s): State<Arc<NodeState>>,
    Path((source, id)): Path<(String, String)>,
) -> ApiResult<SourceItem> {
    let svc = find(&source)?;
    Ok(Json(svc.item(&s, &id).await?))
}
