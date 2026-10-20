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
use pm_pubky::{DiscoveredItem, Discovery};
use std::sync::Arc;

fn discovery(s: &NodeState) -> Result<Arc<Discovery>, ApiError> {
    s.discovery.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "discovery is off: set `contact` (an email or URL) in the node config",
        )
    })
}

fn item(lib: &Library, d: &DiscoveredItem) -> DiscoverItem {
    let in_library = lib
        .tracks_by_canonical(&d.uri)
        .ok()
        .and_then(|v| v.into_iter().next());
    DiscoverItem {
        uri: d.uri.to_url(),
        title: d.card.name.clone(),
        artist: d.card.artist.clone(),
        description: d.card.description.clone(),
        image: d.card.image.clone(),
        taggers: d.taggers,
        in_library,
    }
}

fn page(lib: &Library, facets: Vec<pm_pubky::pinakes::TagFacet>, items: &[DiscoveredItem]) -> DiscoverPage {
    DiscoverPage {
        facets: facets.into_iter().map(|f| (f.label, f.count)).collect(),
        items: items.iter().map(|d| item(lib, d)).collect(),
    }
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, e.to_string())
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
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, facets, &items)))
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
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, Vec::new(), &p.items)))
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
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(page(&lib, Vec::new(), &p.items)))
}
