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
    /// What the source can do, declared (docs/compare-music-assistant.md, "port these" #14), so
    /// the app shows only that: a subset of [`FEATURES`].
    fn features(&self) -> &'static [&'static str] {
        &["search", "play"]
    }
    /// How settled the source is: one of [`STAGES`].
    fn stage(&self) -> &'static str {
        "stable"
    }
    /// Whether a phone or laptop may keep offline copies of its tracks (see [`DOWNLOADABLE`]).
    fn downloadable(&self) -> bool {
        DOWNLOADABLE.contains(&self.id())
    }
}

/// The sources whose tracks may be kept offline on a person's devices (STEWARD-DOWNLOADS.md; the
/// owner can override): free and open catalogues whose terms allow copies (the Internet Archive,
/// Jamendo), podcasts, and the person's own media servers (Plex, Jellyfin, OpenSubsonic,
/// Audiobookshelf: their own files, like a mount).
///
/// Not downloadable: the subscription services (TIDAL, Qobuz, Deezer, Spotify, YouTube),
/// official or unofficial mode, since their terms allow offline copies only in their own apps. A
/// stream that isn't encrypted is still not ours to keep. Also not: radio (live), SoundCloud and
/// Audius (only some tracks allow it, per track), and Bandcamp streams. Bandcamp purchases become
/// library files, and the library is always downloadable (`/v1/library/tracks/{id}/download`).
pub const DOWNLOADABLE: &[&str] = &[
    "archive",
    "jamendo",
    "podcasts",
    "plex",
    "jellyfin",
    "subsonic",
    "audiobookshelf",
];

/// Every feature a source may declare:
/// - `search`: its own search (the scoped search and the Sources tab);
/// - `play`: its tracks play on zones or in the browser;
/// - `connect`: plays on the service's own devices, not through the node (Spotify Connect);
/// - `artists`: artist pages;
/// - `library`: the signed-in user's collection or likes, with an import as favourites;
/// - `playlists`: the user's playlists, with an import as Pubky playlists;
/// - `downloads`: files the user owns can be downloaded into the library;
/// - `lyrics`: lyrics for its tracks;
/// - `scrobble`: plays are reported back to the service.
pub const FEATURES: &[&str] = &[
    "search",
    "play",
    "connect",
    "artists",
    "library",
    "playlists",
    "downloads",
    "lyrics",
    "scrobble",
];

/// `stable` (an official API we expect to keep working), `beta` (new, or an API still moving) and
/// `unofficial` (a private API the service has not approved; it can stop working at any time).
pub const STAGES: &[&str] = &["stable", "beta", "unofficial"];

/// The built-in services, in display order.
pub fn services() -> Vec<Box<dyn StreamingService>> {
    vec![
        Box::new(crate::archive::InternetArchive),
        Box::new(crate::audius::Audius),
        Box::new(crate::tidal::Tidal),
        Box::new(crate::qobuz::Qobuz),
        Box::new(crate::deezer::Deezer),
        Box::new(crate::spotify::Spotify),
        Box::new(crate::jamendo::Jamendo),
        Box::new(crate::bandcamp::Bandcamp),
        Box::new(crate::soundcloud::SoundCloud),
        Box::new(crate::opensubsonic::OpenSubsonic),
        Box::new(crate::jellyfin::Jellyfin),
        Box::new(crate::plex::PlexSource),
        Box::new(crate::podcasts::Podcasts),
        Box::new(crate::radio_sources::Radio),
        Box::new(crate::audiobookshelf::Audiobookshelf),
        Box::new(crate::youtube::YouTube),
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
    /// What it can do, from [`FEATURES`]; the app shows only these.
    pub features: Vec<&'static str>,
    /// One of [`STAGES`].
    pub stage: &'static str,
    /// Its tracks may be kept offline on a device ([`DOWNLOADABLE`]); the app shows the control
    /// only where this is true.
    pub downloadable: bool,
    /// Which sign-in way is in use, in one line, for sources that sign people in
    /// (`crate::signin`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<String>,
}

impl SourceRow {
    pub(crate) fn of(svc: &dyn StreamingService, s: &NodeState, enabled: bool) -> Self {
        Self {
            info: svc.info(s),
            enabled,
            features: svc.features().to_vec(),
            stage: svc.stage(),
            downloadable: svc.downloadable(),
            sign_in: crate::signin::line(s, svc.id()),
        }
    }
}

/// `GET /v1/sources`.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SourceRow>> {
    Ok(Json(
        services()
            .iter()
            .map(|svc| SourceRow::of(svc.as_ref(), &s, s.source_enabled(svc.id())))
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
    Ok(Json(SourceRow::of(svc.as_ref(), &s, b.enabled)))
}

/// For a source's own routes: 404 "… is turned off on Sources" while it is off.
pub(crate) fn ensure_on(s: &NodeState, id: &str) -> Result<(), ApiError> {
    if s.source_enabled(id) {
        Ok(())
    } else {
        Err(ApiError::not_found(format!("{id} is turned off on Sources")))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_downloadable_policy() {
        let ids: Vec<&str> = services().iter().map(|s| s.id()).collect();
        for id in DOWNLOADABLE {
            assert!(ids.contains(id), "{id} isn't a source");
        }
        let downloadable = |id: &str| {
            services()
                .into_iter()
                .find(|s| s.id() == id)
                .unwrap()
                .downloadable()
        };
        // Subscription services, official or unofficial mode, and live radio: never
        for id in [
            "tidal",
            "qobuz",
            "deezer",
            "spotify",
            "youtube",
            "radio",
            "soundcloud",
            "audius",
            "bandcamp",
        ] {
            assert!(!downloadable(id), "{id} must not be downloadable");
        }
        // Free catalogues, podcasts, the person's own servers
        for id in [
            "archive",
            "jamendo",
            "podcasts",
            "plex",
            "jellyfin",
            "subsonic",
            "audiobookshelf",
        ] {
            assert!(downloadable(id), "{id} should be downloadable");
        }
    }
}
