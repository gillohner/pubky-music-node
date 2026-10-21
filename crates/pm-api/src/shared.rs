//! Playlists other people share on Pubky (`/pub/social/v1/posts/`, published by any v1 client):
//! list someone's, open one with where each song plays from on this node, play it, or save a
//! private copy. Their data is hostile input: [`pm_pubky::v1::read_public_playlist`] validates it.

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_pubky::homeserver::NodeHomeserver;
use pm_pubky::{v1, Playlist, Visibility};
use std::sync::Arc;

/// At most this many shared playlists are read per person.
const LIST_MAX: usize = 50;

/// A person (and maybe one post) from what someone pasted: `pubky://<key>/pub/social/v1/posts/<id>`,
/// `pubky<key>` or the bare key.
pub fn parse_link(input: &str) -> Option<(String, Option<String>)> {
    let t = input.trim();
    let rest = t.strip_prefix("pubky://").unwrap_or(t);
    let (key, path) = rest.split_once('/').unwrap_or((rest, ""));
    let key = key.strip_prefix("pubky").filter(|k| k.len() == 52).unwrap_or(key);
    if key.len() != 52 || !key.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let id = path
        .strip_prefix("pub/social/v1/posts/")
        .map(|p| p.split('/').next().unwrap_or("").to_string())
        .filter(|id| v1::micros_of(id).is_some());
    Some((key.to_ascii_lowercase(), id))
}

fn homeserver(s: &NodeState) -> Result<NodeHomeserver, ApiError> {
    match s.sync_handle() {
        Some(h) => Ok(h.homeserver().clone()),
        None => NodeHomeserver::new(s.pubky.network)
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string())),
    }
}

fn user_of(raw: &str) -> Result<(String, v1::PubkyId), ApiError> {
    let (key, _) = parse_link(raw).ok_or_else(|| ApiError::bad_request("not a Pubky key or link"))?;
    let owner =
        v1::PubkyId::try_from(key.as_str()).map_err(|e| ApiError::bad_request(format!("{key}: {e}")))?;
    Ok((key, owner))
}

/// Someone's shared playlists, newest edit first.
async fn read_all(
    s: &NodeState,
    key: &str,
    owner: &v1::PubkyId,
) -> Result<Vec<(String, Playlist)>, ApiError> {
    let hs = homeserver(s)?;
    let heads = hs
        .v1_heads(key, v1::Root::Pub, v1::SOCIAL_NAMESPACE, None)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("their homeserver: {e}")))?;
    let mut newest: Vec<(&String, &v1::Head)> = heads.iter().collect();
    newest.sort_by(|a, b| b.1.edit_id.cmp(&a.1.edit_id));
    let mut out = Vec::new();
    for (id, head) in newest.into_iter().take(LIST_MAX) {
        let Ok(bytes) = hs.get_bytes(key, &head.path, Visibility::Public, None).await else {
            continue;
        };
        if let Some(p) = v1::read_public_playlist(&bytes, id, &head.edit_id, owner) {
            out.push((id.clone(), p));
        }
    }
    Ok(out)
}

async fn read_one(s: &NodeState, key: &str, owner: &v1::PubkyId, id: &str) -> Result<Playlist, ApiError> {
    let hs = homeserver(s)?;
    let heads = hs
        .v1_heads(key, v1::Root::Pub, v1::SOCIAL_NAMESPACE, None)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("their homeserver: {e}")))?;
    let head = heads
        .get(id)
        .ok_or_else(|| ApiError::not_found("no shared playlist with that id (it may be private again)"))?;
    let bytes = hs
        .get_bytes(key, &head.path, Visibility::Public, None)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("their homeserver: {e}")))?;
    v1::read_public_playlist(&bytes, id, &head.edit_id, owner).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "that post is not a playlist this node can read",
        )
    })
}

fn summary(key: &str, id: &str, p: &Playlist) -> SharedPlaylistSummary {
    SharedPlaylistSummary {
        user: key.to_string(),
        id: id.to_string(),
        link: format!("pubky://{key}/pub/social/v1/posts/{id}"),
        name: p.name.clone(),
        description: p.description.clone(),
        items: p.items.len(),
        updated_at: p.updated_at,
    }
}

/// `GET /v1/shared/resolve?link=`: who (and which playlist) a pasted link names.
pub(crate) async fn resolve(Query(q): Query<SharedLink>) -> ApiResult<SharedTarget> {
    let (user, id) =
        parse_link(&q.link).ok_or_else(|| ApiError::bad_request("paste a pubky:// link or a Pubky key"))?;
    Ok(Json(SharedTarget { user, id }))
}

/// `GET /v1/shared/{user}/playlists`.
pub(crate) async fn list(
    State(s): State<Arc<NodeState>>,
    Path(user): Path<String>,
) -> ApiResult<Vec<SharedPlaylistSummary>> {
    let (key, owner) = user_of(&user)?;
    Ok(Json(
        read_all(&s, &key, &owner)
            .await?
            .iter()
            .map(|(id, p)| summary(&key, id, p))
            .collect(),
    ))
}

/// `GET /v1/shared/{user}/playlists/{id}`: the playlist and where each song plays from here.
pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    Path((user, id)): Path<(String, String)>,
) -> ApiResult<SharedPlaylist> {
    let (key, owner) = user_of(&user)?;
    let p = read_one(&s, &key, &owner, &id).await?;
    let (order, own_order, items) = crate::play_from::resolve_playlist(&s, &p).await;
    let acc = crate::discover::service_access(&s);
    Ok(Json(SharedPlaylist {
        summary: summary(&key, &id, &p),
        play_from: PlaylistPlayFrom {
            order: crate::play_from::play_order_of(order, &acc),
            own_order,
            items: items.into_iter().map(|r| r.view).collect(),
        },
    }))
}

/// `POST /v1/shared/{user}/playlists/{id}/copy`: a private copy on this node (pins kept as the
/// suggestions they are; the copy is yours to change).
pub(crate) async fn copy(
    State(s): State<Arc<NodeState>>,
    Path((user, id)): Path<(String, String)>,
) -> ApiResult<PlaylistDetail> {
    let (key, owner) = user_of(&user)?;
    let p = read_one(&s, &key, &owner, &id).await?;
    let Json(d) = crate::playlists::create(
        State(s.clone()),
        Json(CreatePlaylist {
            name: p.name.clone(),
            description: p.description.clone(),
            image: None,
            uris: p.items.iter().map(|i| i.uri.clone()).collect(),
            track_ids: Vec::new(),
            sources: p.items.iter().map(|i| i.source.clone()).collect(),
        }),
    )
    .await?;
    if let Some(order) = p.source_order.clone() {
        let Json(d) = crate::playlists::set_order(
            State(s.clone()),
            Path(d.summary.id.clone()),
            Json(SetPlaylistOrder { order: Some(order) }),
        )
        .await?;
        return Ok(Json(d));
    }
    Ok(Json(d))
}

#[cfg(test)]
mod tests {
    use super::parse_link;

    const KEY: &str = "zswn37ir9t8ge4r4posn4r7ow3eh384c8ud4kkxm585igt38x31o";

    #[test]
    fn links_name_a_person_and_maybe_a_playlist() {
        assert_eq!(parse_link(KEY), Some((KEY.into(), None)));
        assert_eq!(parse_link(&format!("pubky{KEY}")), Some((KEY.into(), None)));
        assert_eq!(
            parse_link(&format!(" pubky://{KEY}/pub/social/v1/posts/0035WKVKQDHSM ")),
            Some((KEY.into(), Some("0035WKVKQDHSM".into())))
        );
        assert_eq!(
            parse_link(&format!(
                "pubky://{KEY}/pub/social/v1/posts/0035WKVKQDHSM/0035WKVKQDHSM.json"
            )),
            Some((KEY.into(), Some("0035WKVKQDHSM".into())))
        );
        assert_eq!(
            parse_link(&format!("pubky://{KEY}/pub/social/v1/posts/nope")),
            Some((KEY.into(), None))
        );
        assert_eq!(parse_link("https://example.com"), None);
        assert_eq!(parse_link("short"), None);
    }
}
