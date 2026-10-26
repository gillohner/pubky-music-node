//! The user's music tags: social v1 tags on canonical music URLs in
//! `/pub/social/v1/tags/` (ADR 0011, 0014). Tags are public by nature, so the node writes
//! one only on an explicit action, keeps the working copy in the library's
//! `tags` table and pushes it with the sync. Nexus indexes them as resource tags and Pinakes
//! reads them, which is what Discover reads back.

use crate::bookmarks::canonical_for;
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::{Library, TagRow};
use pm_model::MusicUri;
use pubky_social_specs::PubkySocialTag;
use std::sync::Arc;

/// Trim and lowercase a label, then validate it as a social v1 tag; returns the tag object
/// (with the normalized label) and its id (the hash of `uri:label`).
pub(crate) fn make_tag(uri: &MusicUri, label: &str) -> Result<(PubkySocialTag, String), ApiError> {
    let label = label.trim().to_lowercase();
    let (tag, id, _) = pm_pubky::v1::music_tag(&uri.to_url(), &label, pm_pubky::ids::now_micros())
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    Ok((tag, id))
}

fn target(r: &TagRef) -> ItemRef {
    ItemRef {
        uri: r.uri.clone(),
        track_id: r.track_id,
        album_id: r.album_id,
        artist_id: r.artist_id,
        at: None,
    }
}

fn rows_for(lib: &Library, uri: Option<&str>) -> Result<Vec<TagEntry>, ApiError> {
    Ok(lib
        .list_tags(uri)?
        .into_iter()
        .map(|t: TagRow| TagEntry {
            uri: t.uri,
            label: t.label,
            created_at: t.created_at,
            synced: t.synced_at.is_some(),
            pending: false,
        })
        .collect())
}

// ---- tags on songs not matched yet: ordinary Pubky tags, published once matched ----

/// `{track id: [(label, created_at)]}`.
const PENDING_KEY: &str = "tags.pending";

type Pending = std::collections::BTreeMap<i64, Vec<(String, i64)>>;

fn pending(lib: &Library) -> Pending {
    lib.get_setting(PENDING_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn store_pending(lib: &Library, p: &Pending) -> Result<(), ApiError> {
    if p.is_empty() {
        lib.delete_setting(PENDING_KEY)?;
    } else {
        lib.set_setting(
            PENDING_KEY,
            &serde_json::to_string(p).expect("pending serializes"),
        )?;
    }
    Ok(())
}

/// The library song a reference names when it has no identity yet.
fn unmatched_track(lib: &Library, r: &TagRef) -> Result<Option<i64>, ApiError> {
    if r.uri.as_deref().is_some_and(|u| !u.trim().is_empty()) {
        return Ok(None);
    }
    let Some(id) = r.track_id else { return Ok(None) };
    let t = lib
        .get_track(pm_model::TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("track {id}")))?;
    Ok(t.publishable_uri().is_none().then_some(id))
}

/// A label as pubky.app keeps it (lowercase, trimmed, validated).
fn normal_label(label: &str) -> Result<String, ApiError> {
    let probe = MusicUri::parse("https://musicbrainz.org/recording/00000000-0000-0000-0000-000000000000")
        .expect("valid");
    Ok(make_tag(&probe, label)?.0.label)
}

/// Pending tags whose song has been matched since become ordinary tags (pushed by the sync).
pub(crate) fn promote_pending(lib: &Library) -> Result<usize, ApiError> {
    let mut p = pending(lib);
    if p.is_empty() {
        return Ok(0);
    }
    let mut done = 0;
    let ids: Vec<i64> = p.keys().copied().collect();
    for id in ids {
        match lib.get_track(pm_model::TrackId(id))? {
            None => {
                p.remove(&id);
            }
            Some(t) => {
                if let Some(uri) = t.publishable_uri() {
                    for (label, _) in p.remove(&id).unwrap_or_default() {
                        let (tag, tid) = make_tag(uri, &label)?;
                        lib.add_tag(&tid, &tag.uri, &tag.label, tag.created_at)?;
                        done += 1;
                    }
                }
            }
        }
    }
    store_pending(lib, &p)?;
    Ok(done)
}

// ---- the files' genres as Pubky tags (opt-in) ----

/// "Publish my files' genres as tags" (off by default; public, like the Roon import).
pub const GENRE_TAGS_KEY: &str = "tags.publish_genres";

pub(crate) fn genre_tags_on(lib: &Library) -> bool {
    lib.get_setting(GENRE_TAGS_KEY)
        .ok()
        .flatten()
        .is_some_and(|v| v == "1")
}

/// Tag every matched song with its files' genres (once each; idempotent). Returns how many
/// tags were added.
pub(crate) fn publish_genres(lib: &Library) -> Result<usize, ApiError> {
    let existing: std::collections::HashSet<(String, String)> = lib
        .list_tags(None)?
        .into_iter()
        .map(|t| (t.uri, t.label))
        .collect();
    let mut added = 0;
    let mut after = None;
    loop {
        let page = lib.list_tracks(after, 1000)?;
        if page.is_empty() {
            break;
        }
        after = page.last().and_then(|t| t.id);
        for t in page {
            let Some(uri) = t.publishable_uri() else { continue };
            let url = uri.to_url();
            for g in &t.genres {
                let Ok((tag, id)) = make_tag(uri, g) else { continue };
                if existing.contains(&(url.clone(), tag.label.clone())) {
                    continue;
                }
                lib.add_tag(&id, &tag.uri, &tag.label, tag.created_at)?;
                added += 1;
            }
        }
    }
    Ok(added)
}

/// Before each sync: pending tags of songs matched since, and (when switched on) new genre tags.
pub(crate) fn before_sync(lib: &Library) {
    if let Err(e) = promote_pending(lib) {
        tracing::warn!("pending tags: {}", e.message);
    }
    if genre_tags_on(lib) {
        if let Err(e) = publish_genres(lib) {
            tracing::warn!("genre tags: {}", e.message);
        }
    }
}

/// `GET /v1/tags` (all of mine) or `?uri=|track_id=|album_id=|artist_id=` (one object).
pub(crate) async fn list(State(s): State<Arc<NodeState>>, Query(r): Query<TagRef>) -> ApiResult<Tags> {
    let lib = s.library.lock().expect("library poisoned");
    let has_target = r.uri.is_some() || r.track_id.is_some() || r.album_id.is_some() || r.artist_id.is_some();
    if !has_target {
        return Ok(Json(Tags {
            uri: None,
            tags: rows_for(&lib, None)?,
        }));
    }
    promote_pending(&lib)?;
    if let Some(id) = unmatched_track(&lib, &r)? {
        let tags = pending(&lib)
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .map(|(label, created_at)| TagEntry {
                uri: format!("track:{id}"),
                label,
                created_at,
                synced: false,
                pending: true,
            })
            .collect();
        return Ok(Json(Tags { uri: None, tags }));
    }
    let uri = canonical_for(&lib, &target(&r))?;
    let url = uri.to_url();
    Ok(Json(Tags {
        tags: rows_for(&lib, Some(&url))?,
        uri: Some(url),
    }))
}

/// `PUT /v1/tags {uri|track_id|album_id|artist_id, label}`.
pub(crate) async fn put(State(s): State<Arc<NodeState>>, Json(r): Json<TagRef>) -> ApiResult<TagChanged> {
    let label = r.label.clone().unwrap_or_default();
    let (url, label) = {
        let lib = s.library.lock().expect("library poisoned");
        // A song not matched yet: the tag waits and is published once it is
        if let Some(id) = unmatched_track(&lib, &r)? {
            let label = normal_label(&label)?;
            let mut p = pending(&lib);
            let list = p.entry(id).or_default();
            if !list.iter().any(|(l, _)| *l == label) {
                list.push((label.clone(), crate::playlists::now_micros()));
            }
            store_pending(&lib, &p)?;
            drop(lib);
            s.notify_library();
            return Ok(Json(TagChanged {
                uri: format!("track:{id}"),
                label,
                tagged: true,
            }));
        }
        let uri = canonical_for(&lib, &target(&r))?;
        let (tag, id) = make_tag(&uri, &label)?;
        lib.add_tag(&id, &tag.uri, &tag.label, tag.created_at)?;
        (tag.uri, tag.label)
    };
    s.notify_library();
    Ok(Json(TagChanged {
        uri: url,
        label,
        tagged: true,
    }))
}

/// `POST /v1/tags/bulk {label, album_id?|artist_id?}`: tag every track in
/// scope that has a publishable identity. One social v1 tag per canonical
/// URL, pushed by the sync; Pinakes then joins each to its group across
/// services (`also_on`). An explicit action, never automatic (ADR 0011).
pub(crate) async fn bulk(State(s): State<Arc<NodeState>>, Json(r): Json<BulkTag>) -> ApiResult<BulkTagged> {
    let probe = MusicUri::parse("https://musicbrainz.org/recording/00000000-0000-0000-0000-000000000000")
        .expect("a MusicBrainz URL");
    let label = make_tag(&probe, &r.label)?.0.label;
    let mut out = BulkTagged {
        label: label.clone(),
        ..Default::default()
    };
    {
        let lib = s.library.lock().expect("library poisoned");
        let tracks: Vec<pm_model::Track> = if let Some(a) = r.album_id {
            lib.album_tracks(a)?
        } else if let Some(ar) = r.artist_id {
            lib.artist_tracks(ar)?
        } else {
            let mut v = Vec::new();
            let mut after = None;
            loop {
                let page = lib.list_tracks(after, 1000)?;
                if page.is_empty() {
                    break;
                }
                after = page.last().and_then(|t| t.id);
                v.extend(page);
            }
            v
        };
        let existing: std::collections::HashSet<String> = lib
            .list_tags(None)?
            .into_iter()
            .filter(|t| t.label == label)
            .map(|t| t.uri)
            .collect();
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
        for t in tracks {
            let Some(uri) = t.publishable_uri() else {
                out.skipped_no_identity += 1;
                continue;
            };
            let url = uri.to_url();
            if !done.insert(url.clone()) {
                continue;
            }
            if existing.contains(&url) {
                out.already += 1;
                continue;
            }
            let (tag, id) = make_tag(uri, &label)?;
            lib.add_tag(&id, &tag.uri, &tag.label, tag.created_at)?;
            out.tagged += 1;
        }
    }
    if out.tagged > 0 {
        s.notify_library();
    }
    Ok(Json(out))
}

/// `DELETE /v1/tags?...&label=`.
pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Query(r): Query<TagRef>,
) -> ApiResult<TagChanged> {
    let label = r.label.clone().unwrap_or_default().trim().to_lowercase();
    if label.is_empty() {
        return Err(ApiError::bad_request("label is required"));
    }
    let url = {
        let lib = s.library.lock().expect("library poisoned");
        if let Some(id) = unmatched_track(&lib, &r)? {
            let mut p = pending(&lib);
            if let Some(list) = p.get_mut(&id) {
                list.retain(|(l, _)| *l != label);
                if list.is_empty() {
                    p.remove(&id);
                }
            }
            store_pending(&lib, &p)?;
            drop(lib);
            s.notify_library();
            return Ok(Json(TagChanged {
                uri: format!("track:{id}"),
                label,
                tagged: false,
            }));
        }
        let uri = canonical_for(&lib, &target(&r))?;
        let url = uri.to_url();
        lib.remove_tag(&url, &label)?;
        url
    };
    s.notify_library();
    Ok(Json(TagChanged {
        uri: url,
        label,
        tagged: false,
    }))
}
