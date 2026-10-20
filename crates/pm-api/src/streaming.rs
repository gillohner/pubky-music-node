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
    /// Stable id used in URLs (`archive`, `audius`, `tidal`).
    fn id(&self) -> &'static str;
    fn info(&self, s: &NodeState) -> SourceInfo;
    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError>;
    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError>;
}

/// The built-in services, in display order.
pub fn services() -> Vec<Box<dyn StreamingService>> {
    vec![
        Box::new(crate::archive::InternetArchive),
        Box::new(crate::audius::Audius),
        Box::new(crate::tidal::Tidal),
    ]
}

fn find(id: &str) -> Result<Box<dyn StreamingService>, ApiError> {
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

/// `GET /v1/sources`.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SourceInfo>> {
    Ok(Json(services().iter().map(|svc| svc.info(&s)).collect()))
}

/// `GET /v1/sources/{source}/search?q=&limit=`.
pub(crate) async fn search(
    State(s): State<Arc<NodeState>>,
    Path(source): Path<String>,
    Query(q): Query<SourceSearch>,
) -> ApiResult<SourceResults> {
    let svc = find(&source)?;
    let term = q.q.trim();
    if term.is_empty() {
        return Err(ApiError::bad_request("q is required"));
    }
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    let items = svc.search(&s, term, limit).await?;
    Ok(Json(SourceResults { source, items }))
}

/// `GET /v1/sources/{source}/items/{id}`.
pub(crate) async fn item(
    State(s): State<Arc<NodeState>>,
    Path((source, id)): Path<(String, String)>,
) -> ApiResult<SourceItem> {
    let svc = find(&source)?;
    Ok(Json(svc.item(&s, &id).await?))
}
