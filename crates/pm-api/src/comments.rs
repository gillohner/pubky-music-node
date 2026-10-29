//! Comments on a song, an album, an artist or a post (TASK-pinakes-social-ux.md B1): social v1
//! `note` posts on the signed-in person's homeserver whose `parent` is the item's canonical URI
//! (a MusicBrainz or service URL), or for a reply the comment's own `pubky://` post URI. No new
//! kind and nothing client-specific: any social v1 app shows them as replies.
//!
//! The thread is read from Nexus by parent: the Resource's replies (the fork's
//! `/v0/resource/{id}/replies`), and a comment's replies by its post. Nexus indexes a new post
//! after a moment, so the node adds its own comments of the last ten minutes until Nexus has them.
//! Deleting removes every stored version of one of your own comments.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pubky_social_specs::{PubkyId, PubkySocialPost, PubkySocialPostKind, Root};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the node shows its own new comments while Nexus catches up.
const RECENT: Duration = Duration::from_secs(600);

/// One comment.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Comment {
    /// The comment's `pubky://…/pub/social/v1/posts/<id>` (what a reply's `reply_to` takes).
    pub post: String,
    pub id: String,
    /// The author's key.
    pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: String,
    /// Unix milliseconds.
    pub at: i64,
    pub replies: u64,
    /// The signed-in person wrote it (they may delete it).
    pub mine: bool,
    /// What it comments on.
    pub parent: String,
}

#[derive(Debug, Serialize)]
pub struct Thread {
    /// The canonical URI the comments are about.
    pub parent: String,
    pub comments: Vec<Comment>,
    /// Nexus answered (false: only the node's own new comments, if any).
    pub indexed: bool,
}

#[derive(Debug, Deserialize)]
pub struct ThreadQuery {
    /// The item (a song's MusicBrainz URL, a service URL).
    pub uri: Option<String>,
    /// A comment, for its replies.
    pub post: Option<String>,
    #[serde(default)]
    pub skip: u32,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct NewComment {
    pub uri: Option<String>,
    /// A comment's `post`, to reply to it.
    pub reply_to: Option<String>,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct WhichComment {
    pub post: String,
}

fn recent() -> &'static Mutex<Vec<(Instant, Comment)>> {
    static R: std::sync::OnceLock<Mutex<Vec<(Instant, Comment)>>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// The bare key of a z32 (with or without its `pubky` prefix).
fn bare(z32: &str) -> &str {
    z32.strip_prefix("pubky").filter(|k| k.len() == 52).unwrap_or(z32)
}

/// `(author, id)` of a social v1 post URI.
fn post_of(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("pubky://")?;
    let (author, path) = rest.split_once('/')?;
    let id = path.strip_prefix("pub/social/v1/posts/")?.trim_end_matches('/');
    (!id.is_empty() && !id.contains('/')).then(|| (bare(author).to_string(), id.to_string()))
}

fn post_uri(author: &str, id: &str) -> String {
    format!("pubky://{}/pub/social/v1/posts/{id}", bare(author))
}

/// The canonical form of what a comment is about, or a 400.
fn canonical(uri: &str) -> Result<String, ApiError> {
    let uri = uri.trim();
    if let Some((author, id)) = post_of(uri) {
        return Ok(post_uri(&author, &id));
    }
    pubky_social_specs::canonicalize_universal(uri)
        .map_err(|()| ApiError::bad_request(format!("{uri}: not a link a comment can be about")))
}

fn signed_in(s: &NodeState) -> Result<Arc<crate::sync::PubkySync>, ApiError> {
    s.sync_handle().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Sign in with Pubky first: a comment is written to your homeserver",
        )
    })
}

/// `GET /v1/comments?uri=|post=&skip=&limit=`: the comments on an item, or a comment's replies,
/// newest first.
pub(crate) async fn list(State(s): State<Arc<NodeState>>, Query(q): Query<ThreadQuery>) -> ApiResult<Thread> {
    let parent = match (&q.uri, &q.post) {
        (Some(u), None) => canonical(u)?,
        (None, Some(p)) => {
            let (author, id) = post_of(p)
                .ok_or_else(|| ApiError::bad_request("post is a pubky://…/pub/social/v1/posts/<id> link"))?;
            post_uri(&author, &id)
        }
        _ => return Err(ApiError::bad_request("give uri (an item) or post (a comment)")),
    };
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let me = s.sync_handle().map(|h| bare(h.z32()).to_string());
    let (mut comments, indexed) = match s.discovery() {
        None => (Vec::new(), false),
        Some(d) => {
            let nexus = d.nexus();
            let posts = match post_of(&parent) {
                Some((author, id)) => nexus.post_replies(&author, &id, q.skip, limit).await,
                None => match nexus.resource_by_uri(&parent).await {
                    Ok(view) => nexus.resource_replies(&view.resource.id, q.skip, limit).await,
                    Err(pm_pubky::NexusError::NotFound) => Ok(Vec::new()),
                    Err(e) => Err(e),
                },
            };
            match posts {
                Ok(posts) => {
                    let authors: Vec<String> = {
                        let mut a: Vec<String> = posts.iter().map(|p| p.details.author.clone()).collect();
                        a.sort();
                        a.dedup();
                        a
                    };
                    let names: std::collections::HashMap<String, String> = nexus
                        .users_by_ids(&authors, None)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|u| !u.details.name.trim().is_empty())
                        .map(|u| (u.details.id, u.details.name))
                        .collect();
                    let list = posts
                        .into_iter()
                        // Comments are notes and articles; a rating or a collection that names the
                        // item as its parent is not a comment
                        .filter(|p| matches!(p.details.kind.as_str(), "note" | "article" | "short" | "long"))
                        .map(|p| Comment {
                            post: post_uri(&p.details.author, &p.details.id),
                            mine: me.as_deref() == Some(bare(&p.details.author)),
                            name: names.get(&p.details.author).cloned(),
                            author: bare(&p.details.author).to_string(),
                            id: p.details.id,
                            content: p.details.content,
                            at: p.details.indexed_at,
                            replies: p.counts.replies,
                            parent: parent.clone(),
                        })
                        .collect();
                    (list, true)
                }
                Err(e) => {
                    tracing::debug!(error = %e, "comments: Nexus didn't answer");
                    (Vec::new(), false)
                }
            }
        }
    };
    // The node's own new comments Nexus hasn't indexed yet (first page only)
    if q.skip == 0 {
        let mut r = recent().lock().expect("recent comments");
        r.retain(|(t, _)| t.elapsed() < RECENT);
        for (_, c) in r.iter() {
            if c.parent == parent
                && me.as_deref() == Some(c.author.as_str())
                && !comments.iter().any(|x| x.post == c.post)
            {
                comments.push(c.clone());
            }
        }
    }
    comments.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.id.cmp(&a.id)));
    Ok(Json(Thread {
        parent,
        comments,
        indexed,
    }))
}

/// `POST /v1/comments {uri | reply_to, content}`: a comment on an item, or a reply to a comment,
/// written as a public social v1 note.
pub(crate) async fn create(State(s): State<Arc<NodeState>>, Json(c): Json<NewComment>) -> ApiResult<Comment> {
    let sync = signed_in(&s)?;
    let parent = match (&c.uri, &c.reply_to) {
        (Some(u), None) => canonical(u)?,
        (None, Some(p)) => {
            let (author, id) =
                post_of(p).ok_or_else(|| ApiError::bad_request("reply_to is a comment's post link"))?;
            post_uri(&author, &id)
        }
        _ => {
            return Err(ApiError::bad_request(
                "give uri (an item) or reply_to (a comment)",
            ))
        }
    };
    let content = pubky_social_specs::frozen_trim(&c.content).to_string();
    let max = pubky_social_specs::VALIDATION_LIMITS.post_note_content_max_length;
    if content.is_empty() {
        return Err(ApiError::bad_request("A comment needs some words"));
    }
    if content.chars().count() > max {
        return Err(ApiError::bad_request(format!(
            "A comment is at most {max} characters"
        )));
    }
    let key = bare(sync.z32()).to_string();
    let owner = PubkyId::try_from(key.as_str())
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("your key: {e}")))?;
    let post = PubkySocialPost::new(
        content,
        PubkySocialPostKind::Note,
        Some(parent.clone()),
        None,
        Vec::new(),
    );
    let micros = pm_pubky::ids::now_micros();
    let id = pm_pubky::v1::id_of(micros);
    pm_pubky::v1::validate_social(&post, &id, Root::Pub, &owner).map_err(ApiError::bad_request)?;
    let path = PubkySocialPost::create_path_in(Root::Pub, &id, &id, None);
    sync.homeserver()
        .write_object(sync.session(), &path, &post)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("your homeserver: {e}")))?;
    let comment = Comment {
        post: post_uri(&key, &id),
        id,
        author: key,
        name: None,
        content: post.content,
        at: micros / 1000,
        replies: 0,
        mine: true,
        parent,
    };
    {
        let mut r = recent().lock().expect("recent comments");
        r.retain(|(t, _)| t.elapsed() < RECENT);
        r.push((Instant::now(), comment.clone()));
    }
    Ok(Json(comment))
}

#[derive(Debug, Serialize)]
pub struct Deleted {
    pub deleted: usize,
}

/// `DELETE /v1/comments?post=`: one of your own comments, every stored version of it.
pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Query(w): Query<WhichComment>,
) -> ApiResult<Deleted> {
    let sync = signed_in(&s)?;
    let (author, id) =
        post_of(&w.post).ok_or_else(|| ApiError::bad_request("post is a comment's post link"))?;
    let key = bare(sync.z32()).to_string();
    if author != key {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Only your own comments can be deleted",
        ));
    }
    let owner = PubkyId::try_from(key.as_str())
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("your key: {e}")))?;
    let dir = format!("/pub/social/v1/posts/{id}/");
    let listed = sync
        .homeserver()
        .public_paths(sync.z32(), &dir)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("your homeserver: {e}")))?;
    let copies: Vec<(Root, String)> = listed
        .into_iter()
        .map(|p| {
            let p = if p.starts_with('/') {
                p
            } else {
                format!("{dir}{p}")
            };
            (Root::Pub, p)
        })
        .collect();
    let plan =
        pubky_social_specs::plan_delete(&id, &[], &copies, &[], &owner).map_err(ApiError::bad_request)?;
    let mut deleted = 0;
    for path in &plan.deletes {
        match sync.homeserver().delete(sync.session(), path).await {
            Ok(()) => deleted += 1,
            Err(pm_pubky::homeserver::HomeserverError::Pubky(e)) if e.to_string().contains("404") => {}
            Err(e) => {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("your homeserver: {e}"),
                ))
            }
        }
    }
    let post = post_uri(&key, &id);
    recent()
        .lock()
        .expect("recent comments")
        .retain(|(_, c)| c.post != post);
    Ok(Json(Deleted { deleted }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_links_and_canonical_items() {
        let key = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";
        let uri = format!("pubky://{key}/pub/social/v1/posts/0033SCZXVEPNG");
        assert_eq!(
            post_of(&uri),
            Some((key.to_string(), "0033SCZXVEPNG".to_string()))
        );
        assert_eq!(
            post_of(&format!("pubky://pubky{key}/pub/social/v1/posts/0033SCZXVEPNG"))
                .unwrap()
                .0,
            key
        );
        assert_eq!(
            post_of(&format!(
                "pubky://{key}/pub/social/v1/posts/0033SCZXVEPNG/0033SCZXVEPNG.json"
            )),
            None
        );
        assert_eq!(post_of("https://musicbrainz.org/recording/x"), None);
        assert_eq!(canonical(&uri).unwrap(), uri);
        assert!(canonical("https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69").is_ok());
        assert!(canonical("not a link").is_err());
    }
}
