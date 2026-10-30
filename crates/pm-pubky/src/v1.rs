//! The node's objects as pubky-social-specs v1 objects (ADR 0014).
//!
//! The node keeps its working copy in its own shape ([`MusicContent`], [`Bookmark`](crate::Bookmark)); this module
//! converts at the homeserver boundary, both ways:
//!
//! | Node object | v1 object | Path |
//! | :-- | :-- | :-- |
//! | [`Playlist`] (a collection: songs, albums, artists, posts) | social `collection` with its `layout` | `/{root}/social/v1/posts/{id}/{editId}.json` |
//! | [`Review`] with a rating | social `rating` (1.1, 1–10) whose `parent` is the music URL | `/{root}/social/v1/posts/…` |
//! | [`Review`] without a rating (a comment) | social `note` whose `parent` is the music URL | `/{root}/social/v1/posts/…` |
//! | [`Activity`] | music `listen` | `/{root}/music/v1/posts/…` (the listen history setting picks the root) |
//! | [`Bookmark`](crate::Bookmark) | social `bookmark` | `/priv/social/v1/bookmarks/{filename}.json` |
//! | a tag (library `tags` row) | social `tag` (id = hash of `uri:label`) | `/pub/social/v1/tags/{id}.json` |
//!
//! A rating or comment is private (`/priv/`) until its owner publishes it, like a collection.
//! Before pubky-social-specs 1.1 a rated review was a music `review` under `/priv/music/v1/` and
//! a comment a note *embedding* the music URL; both are read once more and rewritten in place
//! ([`from_music`], [`from_embed_note`]).
//!
//! The local id (a timestamp id of the creation time) is the v1 post id, so a migrated object
//! keeps its identity. Creation writes `editId == id`; every later write is a new version above
//! the newest one on the homeserver, and readers take the newest.

use crate::ids;
use crate::schemas::{
    item_uri, Activity, ActivitySource, ItemSource, MusicContent, Playlist, PlaylistItem, Review, LAYOUTS,
};
use pm_model::MusicUri;
use pubky_music_specs::ext;
use pubky_music_specs::{MusicPost, MusicPostKind, MusicSource, MUSIC_NAMESPACE};
use pubky_social_specs::traits::{HasIdPath, HashId, Validatable};
use pubky_social_specs::{
    bookmark_target, create_bookmark, namespace_path, validate_timestamp_id_format, PubkySocialBookmark,
    PubkySocialCollectionContent, PubkySocialCollectionItem, PubkySocialCollectionLayout, PubkySocialPost,
    PubkySocialPostKind, PubkySocialRatingContent, PubkySocialTag, VALIDATION_LIMITS,
};
use std::collections::BTreeMap;

pub use pubky_social_specs::{PubkyId, Root, SOCIAL_NAMESPACE};

/// The most songs a playlist can have and still be a v1 collection.
pub const COLLECTION_MAX_ITEMS: usize = VALIDATION_LIMITS.collection_items_max_count;

/// `/{root}/{namespace}/v1/posts/`.
pub fn posts_dir(root: Root, namespace: &str) -> String {
    namespace_path(root, namespace, "posts/")
}

/// `/priv/social/v1/bookmarks/`.
pub fn bookmarks_dir() -> String {
    namespace_path(Root::Priv, SOCIAL_NAMESPACE, "bookmarks/")
}

/// The root a node visibility maps to.
pub fn root_of(public: bool) -> Root {
    if public {
        Root::Pub
    } else {
        Root::Priv
    }
}

/// Microseconds of a timestamp id, `None` when it is not one.
pub fn micros_of(id: &str) -> Option<i64> {
    validate_timestamp_id_format(id).ok()
}

/// `(id, editId)` of a version path `…/posts/{id}/{editId}[-{slug}].json`.
pub fn parse_version(path: &str) -> Option<(String, String)> {
    let mut parts = path.trim_end_matches('/').rsplit('/');
    let leaf = parts.next()?.strip_suffix(".json")?;
    let id = parts.next()?;
    if parts.next()? != "posts" {
        return None;
    }
    let edit_id = leaf.split('-').next()?;
    micros_of(id)?;
    micros_of(edit_id)?;
    Some((id.to_string(), edit_id.to_string()))
}

/// The newest version of one post on the homeserver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub edit_id: String,
    pub path: String,
    /// Every version path of the post, for a delete.
    pub versions: Vec<String>,
}

impl Head {
    /// Microseconds of the newest edit.
    pub fn edited_micros(&self) -> i64 {
        micros_of(&self.edit_id).unwrap_or(0)
    }
}

/// Group version paths by post id, newest version first wins (bytewise order of the editId, which
/// is time order for timestamp ids).
pub fn heads(paths: impl IntoIterator<Item = String>) -> BTreeMap<String, Head> {
    let mut out: BTreeMap<String, Head> = BTreeMap::new();
    for path in paths {
        let Some((id, edit_id)) = parse_version(&path) else {
            continue;
        };
        let head = out.entry(id).or_insert_with(|| Head {
            edit_id: String::new(),
            path: String::new(),
            versions: Vec::new(),
        });
        if edit_id.as_bytes() > head.edit_id.as_bytes() {
            head.edit_id = edit_id;
            head.path = path.clone();
        }
        head.versions.push(path);
    }
    for head in out.values_mut() {
        head.versions.sort();
    }
    out
}

/// Why a node object has no v1 form.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotV1 {
    #[error("{0}")]
    Invalid(String),
}

/// Long playlists (owner, 2026-10-21: "make collections but remove the limit"): a playlist is a
/// v1 collection however long it is. pubky-social-specs caps a collection at
/// [`COLLECTION_MAX_ITEMS`] items and its content at `collection_content_max_length`; a longer one
/// is checked as consecutive parts that each fit, so every rule but the size still applies (item
/// references, `ext`, the envelope). `None` when the post is not an oversized collection.
fn size_parts(post: &PubkySocialPost) -> Option<Vec<PubkySocialPost>> {
    if post.kind != PubkySocialPostKind::Collection {
        return None;
    }
    let cap = VALIDATION_LIMITS.collection_content_max_length;
    if post.content.chars().count() <= cap
        && serde_json::from_str::<PubkySocialCollectionContent>(&post.content)
            .is_ok_and(|c| c.items.len() <= COLLECTION_MAX_ITEMS)
    {
        return None;
    }
    let content: PubkySocialCollectionContent = serde_json::from_str(&post.content).ok()?;
    let mut parts = Vec::new();
    let mut start = 0;
    while start < content.items.len() {
        let mut take = COLLECTION_MAX_ITEMS;
        loop {
            let end = (start + take).min(content.items.len());
            let mut part = content.clone();
            part.items = content.items[start..end].to_vec();
            let json = serde_json::to_string(&part).ok()?;
            if json.chars().count() <= cap || take == 1 {
                let mut p = post.clone();
                p.content = json;
                parts.push(p);
                start = end;
                break;
            }
            take /= 2;
        }
    }
    Some(parts)
}

/// Whether this collection is longer than pubky-social-specs alone allows.
pub fn is_oversized(post: &PubkySocialPost) -> bool {
    size_parts(post).is_some()
}

/// The spec's validation and reference checks, with the collection size cap lifted
/// (a longer collection is checked in parts that each fit).
pub fn validate_social(post: &PubkySocialPost, id: &str, root: Root, owner: &PubkyId) -> Result<(), String> {
    let ctx = pubky_social_specs::ValidationCtx { root };
    for p in size_parts(post).unwrap_or_else(|| vec![post.clone()]) {
        p.validate(Some(id), &ctx)?;
        p.check_references(&ctx, Some(owner))?;
    }
    Ok(())
}

/// Publish one version of a private social post: `(public path, body)`. pubky-social-specs
/// `plan_publish`, or for an oversized playlist the same leaf under `/pub/` after the lifted
/// checks (music collections carry no private media to copy).
pub fn publish_plan(
    post: &PubkySocialPost,
    id: &str,
    edit_id: &str,
    owner: &PubkyId,
) -> Result<(String, serde_json::Value), String> {
    if is_oversized(post) {
        validate_social(post, id, Root::Pub, owner)?;
        let body = serde_json::to_value(post).map_err(|e| e.to_string())?;
        return Ok((
            PubkySocialPost::create_path_in(Root::Pub, id, edit_id, None),
            body,
        ));
    }
    let plan = pubky_social_specs::plan_publish(id, edit_id, post, None, owner)?;
    let body = serde_json::from_str(&plan.rewritten_post_json).map_err(|e| e.to_string())?;
    Ok((plan.dest_path, body))
}

/// A node object as the v1 post it is written as.
#[derive(Debug, Clone, PartialEq)]
pub enum V1Post {
    Social(PubkySocialPost),
    Music(MusicPost),
}

impl V1Post {
    pub fn namespace(&self) -> &'static str {
        match self {
            V1Post::Social(_) => SOCIAL_NAMESPACE,
            V1Post::Music(_) => MUSIC_NAMESPACE,
        }
    }

    /// The path to write for post `id`: creation (`editId == id`) when the homeserver has no
    /// version yet, else a new version above `head`. Validates the post with the owner in scope.
    pub fn version_path(
        &self,
        id: &str,
        head: Option<&str>,
        root: Root,
        owner: &PubkyId,
    ) -> Result<String, String> {
        let ctx = pubky_social_specs::ValidationCtx { root };
        match (self, head) {
            (V1Post::Social(p), Some(head)) => match size_parts(p) {
                // The edit id is minted from the first part (it only has to sort above the head)
                Some(parts) => {
                    validate_social(p, id, root, owner)?;
                    Ok(parts[0].edit_version(id, head, root, owner, None)?.path)
                }
                None => Ok(p.edit_version(id, head, root, owner, None)?.path),
            },
            (V1Post::Music(p), Some(head)) => {
                Ok(pubky_music_specs::edit_version(p, id, head, root, owner)?.path)
            }
            (V1Post::Social(p), None) => {
                validate_social(p, id, root, owner)?;
                Ok(PubkySocialPost::create_path_in(root, id, id, None))
            }
            (V1Post::Music(p), None) => {
                p.validate(Some(id), &ctx)?;
                p.check_references(&ctx, Some(owner))?;
                Ok(MusicPost::version_path(MUSIC_NAMESPACE, root, id, id, None))
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            V1Post::Social(p) => serde_json::to_value(p),
            V1Post::Music(p) => serde_json::to_value(p),
        }
        .expect("a post always serializes")
    }
}

/// The v1 form of a node object.
pub fn to_v1(content: &MusicContent) -> Result<V1Post, NotV1> {
    match content {
        MusicContent::Playlist(p) => playlist_to_collection(p).map(V1Post::Social),
        MusicContent::Review(r) => Ok(review_to_v1(r)),
        MusicContent::Activity(a) => Ok(V1Post::Music(activity_to_listen(a))),
    }
}

fn playlist_to_collection(p: &Playlist) -> Result<PubkySocialPost, NotV1> {
    let items = p
        .items
        .iter()
        .map(|it| {
            let mut item = PubkySocialCollectionItem::new(it.uri.clone(), None);
            ext::set_item_added_at(&mut item, it.added_at);
            if let Some(src) = &it.source {
                let source = MusicSource {
                    service: src.service.clone(),
                    uri: src.uri.clone(),
                    extra: Default::default(),
                };
                ext::set_item_source(&mut item, Some(&source));
            }
            item
        })
        .collect();
    let layout = p
        .layout
        .clone()
        .map(PubkySocialCollectionLayout::from)
        .filter(PubkySocialCollectionLayout::is_known)
        .unwrap_or(PubkySocialCollectionLayout::List);
    let mut post = PubkySocialPost::new_collection(
        p.name.clone(),
        p.description.clone(),
        items,
        p.image.clone(),
        Some(layout),
    );
    let order = p.source_order.as_ref().filter(|o| !o.is_empty());
    if order.is_some() || p.original.is_some() {
        let mut content: PubkySocialCollectionContent =
            serde_json::from_str(&post.content).expect("the collection was just written");
        if let Some(order) = order {
            ext::set_source_order(&mut content, order);
        }
        ext::set_original(&mut content, p.original.as_deref());
        post.content = serde_json::to_string(&content).expect("a collection serializes");
    }
    Ok(post)
}

fn review_to_v1(r: &Review) -> V1Post {
    V1Post::Social(match r.rating {
        Some(rating) => PubkySocialPost::new_rating(
            r.target_uri.clone(),
            i64::from(rating),
            Some(r.text.clone()),
            Vec::new(),
        ),
        None => PubkySocialPost::new(
            r.text.clone(),
            PubkySocialPostKind::Note,
            Some(r.target_uri.clone()),
            None,
            Vec::new(),
        ),
    })
}

fn activity_to_listen(a: &Activity) -> MusicPost {
    let source = match &a.source_kind {
        ActivitySource::Local => Some(MusicSource::service("library")),
        ActivitySource::Roon => Some(MusicSource::service("roon")),
        ActivitySource::Service { name } => {
            // The spec's spelling (`apple-music`, never `Apple_Music`)
            pubky_music_specs::service_name(name).map(MusicSource::service)
        }
        ActivitySource::Unknown => None,
    };
    pubky_music_specs::listen(
        a.played_at,
        a.duration_ms.and_then(|d| i64::try_from(d).ok()),
        source,
        a.uri.clone(),
    )
}

/// Is this a music collection? It is when it or one of its items carries `ext.music` (the node
/// writes `added_at` on every item), when one of its items is a canonical music URL (a playlist
/// another client made, maybe with posts in it), or when it is empty (a new one, still to fill).
/// A collection of links or posts only is someone's reading list: not music.
pub fn is_music_collection(content: &PubkySocialCollectionContent) -> bool {
    let has_music_ext = |extra: &serde_json::Map<String, serde_json::Value>| {
        extra.get(ext::EXT).and_then(|e| e.get(ext::MUSIC)).is_some()
    };
    content.items.is_empty()
        || has_music_ext(&content.extra)
        || content.items.iter().any(|i| has_music_ext(&i.extra))
        || content.items.iter().any(|i| MusicUri::parse(&i.uri).is_ok())
}

/// Someone's public playlist, read from the bytes of one version of a social post: it must
/// pass the spec's public-root validation (with its author in scope), be a music collection,
/// and fit the node's own playlist rules. Anything else is `None`: other people's data is
/// hostile input.
pub fn read_public_playlist(bytes: &[u8], id: &str, edit_id: &str, owner: &PubkyId) -> Option<Playlist> {
    let post: PubkySocialPost = serde_json::from_slice(bytes).ok()?;
    validate_social(&post, id, Root::Pub, owner).ok()?;
    let MusicContent::Playlist(p) = from_social(&post, id, edit_id)? else {
        return None;
    };
    let p = <Playlist as crate::schemas::Validatable>::sanitize(p);
    crate::schemas::Validatable::validate(&p, Some(id)).ok()?;
    Some(p)
}

/// A node object read back from a v1 social post (`id` and `edit_id` from its path): a playlist
/// from a music collection, a rated review from a `rating` of a music URL, a comment from a note
/// replying to one. `None` for anything else, or anything that does not validate: other notes,
/// and a rating or comment carrying what the node cannot keep (an embed, attachments, a lock),
/// so editing it never drops them.
pub fn from_social(post: &PubkySocialPost, id: &str, edit_id: &str) -> Option<MusicContent> {
    let created_at = micros_of(id)?;
    match post.kind {
        PubkySocialPostKind::Collection => {
            let content: PubkySocialCollectionContent = serde_json::from_str(&post.content).ok()?;
            if !is_music_collection(&content) {
                return None;
            }
            // Music URLs and public posts; any other link is not something the node can show
            let items = content
                .items
                .iter()
                .filter_map(|it| {
                    let uri = item_uri(&it.uri).ok()?;
                    let added_at = ext::item_added_at(it).unwrap_or(created_at);
                    let source =
                        ext::item_source(it, &pubky_social_specs::ValidationCtx { root: Root::Priv }, None)
                            .map(|s| ItemSource {
                                service: s.service,
                                uri: s.uri,
                            });
                    Some(PlaylistItem {
                        uri,
                        added_at,
                        source,
                    })
                })
                .collect();
            let order = ext::source_order(&content);
            let original = ext::original(&content, &pubky_social_specs::PUB_CTX);
            let layout = content
                .layout
                .as_ref()
                .map(|l| l.wire_name().to_string())
                .filter(|l| LAYOUTS.contains(&l.as_str()));
            Some(MusicContent::Playlist(Playlist {
                name: content.name,
                description: content.description,
                image: content.cover_image,
                items,
                created_at,
                updated_at: micros_of(edit_id)?.max(created_at),
                source_order: Some(order).filter(|o| !o.is_empty()),
                layout,
                original,
            }))
        }
        PubkySocialPostKind::Rating | PubkySocialPostKind::Note => {
            if post.embed.is_some() || !post.attachments.is_empty() || post.lock.is_some() {
                return None;
            }
            let target = MusicUri::parse(post.parent.as_deref()?).ok()?.to_url();
            if post.kind == PubkySocialPostKind::Note {
                return comment(target, &post.content, created_at);
            }
            let rating: PubkySocialRatingContent = serde_json::from_str(&post.content).ok()?;
            Some(MusicContent::Review(Review {
                target_uri: target,
                rating: Some(u8::try_from(rating.rating).ok()?),
                text: rating.text.unwrap_or_default().trim().to_string(),
                created_at,
            }))
        }
        _ => None,
    }
}

fn comment(target_uri: String, text: &str, created_at: i64) -> Option<MusicContent> {
    let text = text.trim();
    (!text.is_empty()).then(|| {
        MusicContent::Review(Review {
            target_uri,
            rating: None,
            text: text.to_string(),
            created_at,
        })
    })
}

/// A comment as the node wrote it before pubky-social-specs 1.1: a private note *embedding* the
/// music URL. Any app can write such a note, so the caller asks only for a post the node already
/// holds as a review (its own); the next write turns it into a reply ([`to_v1`]).
pub fn from_embed_note(post: &PubkySocialPost, id: &str) -> Option<MusicContent> {
    if post.kind != PubkySocialPostKind::Note || post.parent.is_some() {
        return None;
    }
    let target = MusicUri::parse(post.embed.as_deref()?).ok()?;
    comment(target.to_url(), &post.content, micros_of(id)?)
}

/// A node object read back from a music post: a play from a `listen`, and a rated review from a
/// retired music `review` (it is written again as a social `rating` with the same id).
pub fn from_music(post: &MusicPost, id: &str) -> Option<MusicContent> {
    if post.kind == MusicPostKind::Listen {
        return listen_to_activity(post);
    }
    let rating = pubky_music_specs::retired::rating_from_review(post).ok()?;
    from_social(&rating, id, id)
}

/// A play read back from a `listen` post (the node's id is the post id, its time the play time).
fn listen_to_activity(post: &MusicPost) -> Option<MusicContent> {
    let listen = pubky_music_specs::read_listen(post).ok()?;
    let target = MusicUri::parse(post.embed.as_deref()?).ok()?;
    let source_kind = match listen.source.map(|s| s.service) {
        Some(s) if s == "library" => ActivitySource::Local,
        Some(s) if s == "roon" => ActivitySource::Roon,
        Some(name) => ActivitySource::Service { name },
        None => ActivitySource::Unknown,
    };
    Some(MusicContent::Activity(Activity {
        uri: target.to_url(),
        played_at: listen.played_at,
        source_kind,
        duration_ms: listen.duration_ms.and_then(|d| u64::try_from(d).ok()),
    }))
}

/// The social v1 bookmark of a music URL or a public post (an item URI in its stored form,
/// [`crate::schemas::item_uri`]): the object and its path. `created_at` is kept.
pub fn bookmark_of(uri: &str, created_at: i64) -> Result<(PubkySocialBookmark, String), String> {
    let mut created = create_bookmark(uri)?;
    created.bookmark.created_at = created_at;
    Ok((created.bookmark, created.path))
}

/// The path the bookmark of a music URL or a public post lives at.
pub fn bookmark_path(uri: &str) -> Result<String, String> {
    let filename = pubky_social_specs::bookmark_filename(uri)?;
    Ok(PubkySocialBookmark::create_path(&filename))
}

/// What a stored bookmark names when the node can hold it: a music URL or a public Pubky post
/// (someone's collection, say), in its stored form, and the bookmark's time. `None` for a
/// bookmark of anything else or an invalid one. `path` is the bookmark's path; `body` its JSON.
pub fn music_bookmark(path: &str, body: &[u8]) -> Option<(String, i64)> {
    let filename = path.rsplit('/').next()?.strip_suffix(".json")?;
    let content: PubkySocialBookmark = serde_json::from_slice(body).ok()?;
    let target = bookmark_target(filename, &content).ok()?;
    Some((item_uri(&target).ok()?, content.created_at))
}

/// `/pub/social/v1/tags/`: where tags live (pubky-social-specs v1). Nexus indexes a tag there on
/// an external URL or an app's object as a resource tag (upstream Nexus #1165).
pub fn tags_dir() -> String {
    namespace_path(Root::Pub, SOCIAL_NAMESPACE, "tags/")
}

/// The social v1 tag `label` on the music URL `uri`: the object, its id (the hash of
/// `uri:label`, the same id a `PubkyAppTag` had) and its path. `created_at` is kept. The label
/// must already be folded; the node lowercases it.
pub fn music_tag(
    uri: &str,
    label: &str,
    created_at: i64,
) -> Result<(PubkySocialTag, String, String), String> {
    let tag = PubkySocialTag {
        uri: uri.to_string(),
        label: label.to_string(),
        created_at,
        extra: Default::default(),
    };
    let id = tag.create_id();
    tag.validate(Some(&id), &pubky_social_specs::PUB_CTX)?;
    let path = PubkySocialTag::create_path(&id);
    Ok((tag, id, path))
}

/// The id a tag file at `path` (`…/tags/{id}.json`) claims.
pub fn tag_id_of(path: &str) -> Option<&str> {
    path.rsplit('/').next()?.strip_suffix(".json")
}

/// A tag read from `path`: its id and the tag when it is a valid social v1 tag on a canonical
/// music URL; `None` for anything else (another app's tag on a post, or junk).
pub fn music_tag_from(path: &str, body: &[u8]) -> Option<(String, PubkySocialTag)> {
    let id = tag_id_of(path)?;
    let tag: PubkySocialTag = serde_json::from_slice(body).ok()?;
    tag.validate(Some(id), &pubky_social_specs::PUB_CTX).ok()?;
    MusicUri::parse(&tag.uri).ok()?;
    Some((id.to_string(), tag))
}

/// A timestamp id for `micros`, the same encoding as v1 ids.
pub fn id_of(micros: i64) -> String {
    ids::timestamp_id(micros)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::MusicContent;
    use pubky_social_specs::PubkySocialAttachment;

    const OWNER: &str = "operrr8wsbpr3ue9d4qj41ge1kcc6r7fdiy6o3ugjrrhi4y77rdo";
    const SONG: &str = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    const SONG2: &str = "https://musicbrainz.org/recording/0c1f0a3f-3c21-4f0d-9c33-2a8d8e36a3b1";

    fn owner() -> PubkyId {
        PubkyId::try_from(OWNER).unwrap()
    }

    fn t0() -> i64 {
        ids::now_micros() - 3_600_000_000
    }

    #[test]
    fn node_ids_are_v1_ids() {
        let t = 1_792_540_800_123_456;
        assert_eq!(micros_of(&id_of(t)), Some(t));
    }

    #[test]
    fn version_paths_parse_and_group() {
        let a = id_of(t0());
        let a2 = id_of(t0() + 5);
        let b = id_of(t0() + 9);
        let paths = vec![
            format!("/priv/social/v1/posts/{a}/{a}.json"),
            format!("/priv/social/v1/posts/{a}/{a2}-draft.json"),
            format!("/priv/social/v1/posts/{b}/{b}.json"),
            "/priv/social/v1/posts/junk/x.json".into(),
            format!("/priv/social/v1/files/{a}"),
        ];
        let h = heads(paths);
        assert_eq!(h.len(), 2);
        assert_eq!(h[&a].edit_id, a2);
        assert!(h[&a].path.ends_with("-draft.json"));
        assert_eq!(h[&a].versions.len(), 2);
        assert_eq!(h[&b].edit_id, b);
        assert_eq!(posts_dir(Root::Priv, "music"), "/priv/music/v1/posts/");
        assert_eq!(bookmarks_dir(), "/priv/social/v1/bookmarks/");
    }

    fn playlist(n: usize) -> Playlist {
        let created = t0();
        Playlist {
            name: "Road trip".into(),
            description: Some("Long drives".into()),
            image: None,
            items: (0..n)
                .map(|i| PlaylistItem {
                    uri: if i % 2 == 0 { SONG.into() } else { SONG2.into() },
                    added_at: created + i as i64 * 1000,
                    source: None,
                })
                .collect(),
            created_at: created,
            updated_at: created + 10_000_000,
            source_order: None,
            layout: None,
            original: None,
        }
    }

    #[test]
    fn pins_and_the_play_order_ride_in_ext_music() {
        let mut pl = playlist(2);
        pl.items[0].source = Some(ItemSource {
            service: "tidal".into(),
            uri: Some("https://tidal.com/track/50850120".into()),
        });
        pl.items[1].source = Some(ItemSource {
            service: "library".into(),
            uri: None,
        });
        pl.source_order = Some(vec!["tidal".into(), "library".into()]);
        let id = id_of(pl.created_at);
        let V1Post::Social(post) = to_v1(&MusicContent::Playlist(pl.clone())).unwrap() else {
            panic!("a playlist is a social collection")
        };
        let content: serde_json::Value = serde_json::from_str(&post.content).unwrap();
        assert_eq!(
            content["ext"]["music"]["source_order"],
            serde_json::json!(["tidal", "library"])
        );
        assert_eq!(content["items"][0]["ext"]["music"]["source"]["service"], "tidal");
        let Some(MusicContent::Playlist(back)) = from_social(&post, &id, &id) else {
            panic!("reads back")
        };
        assert_eq!(back.items[0].source, pl.items[0].source);
        assert_eq!(back.items[1].source, pl.items[1].source);
        assert_eq!(back.source_order, pl.source_order);
    }

    #[test]
    fn playlist_round_trips_as_a_collection() {
        let pl = playlist(3);
        let id = id_of(pl.created_at);
        let v1 = to_v1(&MusicContent::Playlist(pl.clone())).unwrap();
        assert_eq!(v1.namespace(), "social");
        let path = v1.version_path(&id, None, Root::Priv, &owner()).unwrap();
        assert_eq!(path, format!("/priv/social/v1/posts/{id}/{id}.json"));
        let json = v1.to_json();
        assert_eq!(json["kind"], "collection");
        let content: serde_json::Value = serde_json::from_str(json["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["layout"], "list");
        assert_eq!(content["items"][1]["uri"], SONG2);
        assert_eq!(
            content["items"][1]["ext"]["music"]["added_at"],
            pl.items[1].added_at
        );

        // An edit is a new version above the head
        let edit = v1.version_path(&id, Some(&id), Root::Priv, &owner()).unwrap();
        let (eid, edit_id) = parse_version(&edit).unwrap();
        assert_eq!(eid, id);
        assert!(edit_id > id);

        let V1Post::Social(post) = v1 else { panic!() };
        let back = from_social(&post, &id, &edit_id).unwrap();
        let MusicContent::Playlist(back) = back else {
            panic!()
        };
        assert_eq!(back.name, pl.name);
        assert_eq!(back.description, pl.description);
        assert_eq!(back.created_at, micros_of(&id).unwrap());
        assert_eq!(back.updated_at, micros_of(&edit_id).unwrap());
        assert_eq!(back.items.len(), 3);
        assert_eq!(
            back.items[2].added_at, pl.items[2].added_at,
            "microseconds both ways"
        );
    }

    #[test]
    fn long_playlists_are_collections_too() {
        let short = to_v1(&MusicContent::Playlist(playlist(COLLECTION_MAX_ITEMS))).unwrap();
        let V1Post::Social(short) = short else { panic!() };
        assert!(!is_oversized(&short));
        let pl = playlist(2500);
        let id = id_of(pl.created_at);
        let v1 = to_v1(&MusicContent::Playlist(pl.clone())).unwrap();
        let V1Post::Social(post) = &v1 else { panic!() };
        assert!(is_oversized(post));
        assert!(size_parts(post).unwrap().len() >= 25, "checked in parts that fit");
        // Created, edited and published like any collection, and read back whole
        let path = v1.version_path(&id, None, Root::Priv, &owner()).unwrap();
        assert_eq!(path, format!("/priv/social/v1/posts/{id}/{id}.json"));
        let edit = v1.version_path(&id, Some(&id), Root::Priv, &owner()).unwrap();
        let (_, edit_id) = parse_version(&edit).unwrap();
        assert!(edit_id.as_str() > id.as_str());
        let (pub_path, body) = publish_plan(post, &id, &edit_id, &owner()).unwrap();
        assert_eq!(pub_path, format!("/pub/social/v1/posts/{id}/{edit_id}.json"));
        let bytes = serde_json::to_vec(&body).unwrap();
        let back = read_public_playlist(&bytes, &id, &edit_id, &owner()).unwrap();
        assert_eq!(back.items.len(), 2500);
        // The other rules still hold in every part
        let mut bad = post.clone();
        let mut c: PubkySocialCollectionContent = serde_json::from_str(&bad.content).unwrap();
        c.items[2200].uri = "not a uri at all".into();
        bad.content = serde_json::to_string(&c).unwrap();
        assert!(validate_social(&bad, &id, Root::Priv, &owner()).is_err());
    }

    #[test]
    fn foreign_collections_and_posts_are_not_playlists() {
        let id = id_of(t0());
        let links = PubkySocialPost::new_collection(
            "Reading".into(),
            None,
            vec![PubkySocialCollectionItem::new(
                "https://example.com/a".into(),
                None,
            )],
            None,
            None,
        );
        assert_eq!(from_social(&links, &id, &id), None);
        let note = PubkySocialPost::new("hello".into(), PubkySocialPostKind::Note, None, None, Vec::new());
        assert_eq!(from_social(&note, &id, &id), None);
    }

    #[test]
    fn ratings_are_social_ratings_and_comments_reply_to_the_music() {
        let created = t0();
        let id = id_of(created);
        let rated = Review {
            target_uri: SONG.into(),
            rating: Some(4),
            text: "Lovely".into(),
            created_at: created,
        };
        let v1 = to_v1(&MusicContent::Review(rated.clone())).unwrap();
        assert_eq!(v1.namespace(), "social");
        // Private, and publishable like a collection
        let path = v1.version_path(&id, None, Root::Priv, &owner()).unwrap();
        assert_eq!(path, format!("/priv/social/v1/posts/{id}/{id}.json"));
        let V1Post::Social(post) = &v1 else { panic!() };
        assert_eq!(post.kind, PubkySocialPostKind::Rating);
        assert_eq!(
            (post.parent.as_deref(), post.embed.as_deref()),
            (Some(SONG), None)
        );
        assert_eq!(post.content, r#"{"rating":4,"text":"Lovely"}"#);
        publish_plan(post, &id, &id, &owner()).unwrap();
        let MusicContent::Review(back) = from_social(post, &id, &id).unwrap() else {
            panic!()
        };
        assert_eq!(back, rated);

        let plain = Review {
            rating: None,
            ..rated.clone()
        };
        let v1 = to_v1(&MusicContent::Review(plain.clone())).unwrap();
        v1.version_path(&id, None, Root::Priv, &owner()).unwrap();
        let V1Post::Social(note) = &v1 else { panic!() };
        assert_eq!(note.kind, PubkySocialPostKind::Note);
        assert_eq!(
            (note.parent.as_deref(), note.embed.as_deref()),
            (Some(SONG), None)
        );
        let MusicContent::Review(back) = from_social(note, &id, &id).unwrap() else {
            panic!()
        };
        assert_eq!(back, plain);

        // A rating alone, from another client
        let seven = PubkySocialPost::new_rating(SONG.into(), 7, None, Vec::new());
        let MusicContent::Review(r) = from_social(&seven, &id, &id).unwrap() else {
            panic!()
        };
        assert_eq!((r.rating, r.text.as_str()), (Some(7), ""));
        crate::schemas::MusicPost::new(MusicContent::Review(r))
            .validate(Some(&id))
            .unwrap();
    }

    #[test]
    fn the_old_review_forms_read_once_more() {
        let created = t0();
        let id = id_of(created);
        // A rated review: a music `review`, read as a rating
        let old: MusicPost = serde_json::from_value(serde_json::json!({
            "content": r#"{"rating":9,"text":"Old"}"#, "kind": "review", "parent": null,
            "embed": SONG, "attachments": []}))
        .unwrap();
        let MusicContent::Review(r) = from_music(&old, &id).unwrap() else {
            panic!()
        };
        assert_eq!(
            (r.rating, r.text.as_str(), r.created_at),
            (Some(9), "Old", created)
        );
        // A comment: a note embedding the music URL, read only through from_embed_note (the
        // caller asks for the node's own)
        let note = PubkySocialPost::new(
            "Nice".into(),
            PubkySocialPostKind::Note,
            None,
            Some(SONG.into()),
            Vec::new(),
        );
        assert_eq!(from_social(&note, &id, &id), None);
        let MusicContent::Review(r) = from_embed_note(&note, &id).unwrap() else {
            panic!()
        };
        assert_eq!((r.rating, r.text.as_str()), (None, "Nice"));
        let V1Post::Social(again) = to_v1(&MusicContent::Review(r)).unwrap() else {
            panic!()
        };
        assert_eq!((again.parent.as_deref(), again.embed), (Some(SONG), None));
    }

    #[test]
    fn what_the_node_cannot_keep_is_not_a_review() {
        let id = id_of(t0());
        // Ratings and replies of something that is not music
        let place = "https://www.openstreetmap.org/way/481516234";
        let rated = PubkySocialPost::new_rating(place.into(), 8, None, Vec::new());
        assert_eq!(from_social(&rated, &id, &id), None);
        let reply = PubkySocialPost::new(
            "hi".into(),
            PubkySocialPostKind::Note,
            Some(place.into()),
            None,
            Vec::new(),
        );
        assert_eq!(from_social(&reply, &id, &id), None);
        // A rating with a photo, a reply quoting something: an edit would drop it
        let photo = PubkySocialAttachment::new(
            format!("pubky://{OWNER}/pub/social/v1/files/PZBQ010FF079VVZPQG1RNFN6DR.png"),
            None,
            None,
        );
        let rated = PubkySocialPost::new_rating(SONG.into(), 8, None, vec![photo]);
        assert_eq!(from_social(&rated, &id, &id), None);
        let reply = PubkySocialPost::new(
            "hi".into(),
            PubkySocialPostKind::Note,
            Some(SONG.into()),
            Some(place.into()),
            Vec::new(),
        );
        assert_eq!(from_social(&reply, &id, &id), None);
        // A rating off the scale
        let mut rated = PubkySocialPost::new_rating(SONG.into(), 8, None, Vec::new());
        rated.content = r#"{"rating":300}"#.into();
        assert_eq!(from_social(&rated, &id, &id), None);
    }

    #[test]
    fn plays_are_listens_under_either_root() {
        let a = Activity {
            uri: SONG.into(),
            played_at: t0(),
            source_kind: ActivitySource::Service { name: "TIDAL".into() },
            duration_ms: Some(200_000),
        };
        let id = id_of(a.played_at);
        let v1 = to_v1(&MusicContent::Activity(a.clone())).unwrap();
        let path = v1.version_path(&id, None, Root::Priv, &owner()).unwrap();
        assert!(path.starts_with("/priv/music/v1/posts/"), "{path}");
        // A public history writes the same listen under /pub/
        let path = v1.version_path(&id, None, Root::Pub, &owner()).unwrap();
        assert!(path.starts_with("/pub/music/v1/posts/"), "{path}");
        let V1Post::Music(post) = &v1 else { panic!() };
        let listen = pubky_music_specs::read_listen(post).unwrap();
        assert_eq!(listen.played_at, a.played_at, "microseconds");
        assert_eq!(listen.source.unwrap().service, "tidal");
    }

    #[test]
    fn bookmarks_are_v1_bookmarks() {
        let uri = MusicUri::parse(SONG).unwrap();
        let (b, path) = bookmark_of(&uri.to_url(), 1_792_540_800_000_000).unwrap();
        assert!(path.starts_with("/priv/social/v1/bookmarks/"), "{path}");
        assert_eq!(bookmark_path(&uri.to_url()).unwrap(), path);
        let body = serde_json::to_vec(&b).unwrap();
        assert_eq!(
            music_bookmark(&path, &body),
            Some((uri.to_url(), 1_792_540_800_000_000))
        );
        // Someone's public collection, bookmarked
        let post = format!("pubky://{OWNER}/pub/social/v1/posts/{}", id_of(t0()));
        let (pb, ppath) = bookmark_of(&post, 7).unwrap();
        assert_eq!(
            music_bookmark(&ppath, &serde_json::to_vec(&pb).unwrap()),
            Some((post, 7))
        );
        // A bookmark of something that is not music
        let (other, opath) = {
            let c = create_bookmark("https://example.com/").unwrap();
            (c.bookmark, c.path)
        };
        assert_eq!(music_bookmark(&opath, &serde_json::to_vec(&other).unwrap()), None);
    }

    #[test]
    fn a_collection_keeps_its_layout_and_mixed_items() {
        let mut pl = playlist(1);
        let post = format!("pubky://{OWNER}/pub/social/v1/posts/0033SCZXVEPNG");
        pl.items.push(PlaylistItem {
            uri: "https://musicbrainz.org/release/1b021cc5-ab1f-4a96-8a4b-a1a9a5e4bd0a".into(),
            added_at: pl.created_at,
            source: None,
        });
        pl.items.push(PlaylistItem {
            uri: post.clone(),
            added_at: pl.created_at,
            source: None,
        });
        pl.layout = Some("visual".into());
        pl.original = Some(format!("pubky://{OWNER}/pub/social/v1/posts/{}", id_of(t0() - 7)));
        let id = id_of(pl.created_at);
        let v1 = to_v1(&MusicContent::Playlist(pl.clone())).unwrap();
        // Valid under either root (the post is public)
        v1.version_path(&id, None, Root::Priv, &owner()).unwrap();
        v1.version_path(&id, None, Root::Pub, &owner()).unwrap();
        let V1Post::Social(post_v1) = &v1 else { panic!() };
        let content: serde_json::Value = serde_json::from_str(&post_v1.content).unwrap();
        assert_eq!(content["layout"], "visual");
        assert_eq!(content["items"][2]["uri"], post);
        let Some(MusicContent::Playlist(back)) = from_social(post_v1, &id, &id) else {
            panic!("reads back")
        };
        assert_eq!(back.layout.as_deref(), Some("visual"));
        assert_eq!(back.original, pl.original, "a duplicate names its original");
        assert_eq!(content["ext"]["music"]["original"], pl.original.clone().unwrap());
        assert_eq!(
            back.items.iter().map(|i| i.uri.as_str()).collect::<Vec<_>>(),
            pl.items.iter().map(|i| i.uri.as_str()).collect::<Vec<_>>()
        );
        // A newer writer's layout reads as a list; a link the node cannot show is left out
        let mut c: PubkySocialCollectionContent = serde_json::from_str(&post_v1.content).unwrap();
        c.layout = Some(PubkySocialCollectionLayout::from("carousel".to_string()));
        c.items.push(PubkySocialCollectionItem::new(
            "https://example.com/a".into(),
            None,
        ));
        let mut other = post_v1.clone();
        other.content = serde_json::to_string(&c).unwrap();
        let Some(MusicContent::Playlist(back)) = from_social(&other, &id, &id) else {
            panic!("still music")
        };
        assert_eq!((back.layout, back.items.len()), (None, 3));
        // Posts only, no ext.music: someone's reading list, not music
        let posts = PubkySocialPost::new_collection(
            "Reading".into(),
            None,
            vec![PubkySocialCollectionItem::new(post, None)],
            None,
            None,
        );
        assert_eq!(from_social(&posts, &id, &id), None);
        // An empty one is a new collection
        let empty = PubkySocialPost::new_collection("New".into(), None, Vec::new(), None, None);
        assert!(matches!(
            from_social(&empty, &id, &id),
            Some(MusicContent::Playlist(_))
        ));
    }

    #[test]
    fn listens_read_back_as_plays() {
        let a = Activity {
            uri: SONG.into(),
            played_at: t0(),
            source_kind: ActivitySource::Local,
            duration_ms: Some(200_000),
        };
        let id = id_of(a.played_at);
        let V1Post::Music(post) = to_v1(&MusicContent::Activity(a.clone())).unwrap() else {
            panic!()
        };
        let Some(MusicContent::Activity(back)) = from_music(&post, &id) else {
            panic!("a listen reads back")
        };
        assert_eq!(back, a);
        // Another app's listen without a source
        // In milliseconds, as an older writer put it
        let bare = pubky_music_specs::listen(1_792_540_800_000, None, None, SONG.into());
        let Some(MusicContent::Activity(back)) = from_music(&bare, &id) else {
            panic!()
        };
        assert_eq!(
            (back.played_at, back.source_kind),
            (1_792_540_800_000_000, ActivitySource::Unknown)
        );
    }

    #[test]
    fn tags_are_social_v1_tags_with_the_old_ids() {
        use pubky_app_specs::traits::HashId as _;
        let (tag, id, path) = music_tag(SONG, "late-night", 1_792_540_800_000_000).unwrap();
        assert_eq!(path, format!("{}{id}.json", tags_dir()));
        assert_eq!(tags_dir(), "/pub/social/v1/tags/");
        // The id a PubkyAppTag had for the same uri and label: the library's ids carry over
        let old = pubky_app_specs::PubkyAppTag {
            uri: SONG.into(),
            label: "late-night".into(),
            created_at: 1,
        };
        assert_eq!(old.create_id(), id);
        let body = serde_json::to_vec(&tag).unwrap();
        let (back_id, back) = music_tag_from(&path, &body).unwrap();
        assert_eq!(
            (back_id.as_str(), back.label.as_str()),
            (id.as_str(), "late-night")
        );
        // Refused: an unfolded label, a wrong id, a tag on a post (not music)
        assert!(music_tag(SONG, "Loud", 1).is_err());
        assert!(music_tag_from(&format!("{}{}.json", tags_dir(), "X".repeat(26)), &body).is_none());
        let post = format!("pubky://{OWNER}/pub/social/v1/posts/0033SCZXVEPNG");
        let (t, _, p) = music_tag(&post, "nice", 1).unwrap();
        assert!(music_tag_from(&p, &serde_json::to_vec(&t).unwrap()).is_none());
    }

    /// pubky-social-migrator (npm) copies a person's `/pub/pubky.app/` tree to v1 with these
    /// transforms and never deletes. Where it and the node's own move touch the same object, they
    /// write the same path, so the two converge instead of making duplicates: a tag on a music
    /// URL keeps its id, a bookmark is keyed by its target, a follow by the followee.
    #[test]
    fn the_spec_migrator_writes_where_the_node_does() {
        use pubky_app_specs::traits::HashId as _;
        use pubky_social_specs::migrate::MigrationCtx;
        let mut ctx = MigrationCtx::new(owner());
        let migrated = |ctx: &mut MigrationCtx, path: &str, body: Vec<u8>| {
            let m = ctx.migrate(path, &body).unwrap();
            assert_eq!(m.writes.len(), 1, "{path}");
            format!("/{}", m.writes[0].0)
        };
        // A tag the node wrote under pubky.app/tags/ before 2026-10-21
        let old = pubky_app_specs::PubkyAppTag {
            uri: SONG.into(),
            label: "calm".into(),
            created_at: 1_792_540_800_000_000,
        };
        let (_, _, node_path) = music_tag(SONG, "calm", old.created_at).unwrap();
        let path = migrated(
            &mut ctx,
            &format!("pub/pubky.app/tags/{}", old.create_id()),
            serde_json::to_vec(&old).unwrap(),
        );
        assert_eq!(path, node_path);
        // A bookmark of a music URL made in pubky.app
        let b = pubky_app_specs::PubkyAppBookmark {
            uri: SONG.into(),
            created_at: 1_792_540_800_000_000,
        };
        let path = migrated(
            &mut ctx,
            &format!("pub/pubky.app/bookmarks/{}", b.create_id()),
            serde_json::to_vec(&b).unwrap(),
        );
        assert_eq!(path, bookmark_path(SONG).unwrap());
        // A follow the node wrote in the older form
        let followee = "pxnu33x7jtpx9ar1ytsi4yxbp6a5o36gwhffs8zoxmbuptici1jy";
        let path = migrated(
            &mut ctx,
            &format!("pub/pubky.app/follows/{followee}"),
            br#"{"created_at":1792540800000000}"#.to_vec(),
        );
        assert_eq!(path, pubky_social_specs::PubkySocialFollow::create_path(followee));
        // The node's older music objects were private: the migrator never reads /priv
        assert!(ctx
            .migrate(&format!("priv/pubky.app/posts/{}", id_of(t0())), b"{}")
            .is_err());
        // A post both could move keeps its id: v0 posts/{id} becomes v1 posts/{id}/{id}.json,
        // the path the node's first version of the same id takes
        let id = id_of(t0());
        let note = pubky_app_specs::PubkyAppPost::new(
            "hello".into(),
            pubky_app_specs::PubkyAppPostKind::Short,
            None,
            None,
            None,
        );
        let path = migrated(
            &mut ctx,
            &format!("pub/pubky.app/posts/{id}"),
            serde_json::to_vec(&note).unwrap(),
        );
        assert_eq!(path, PubkySocialPost::create_path_in(Root::Pub, &id, &id, None));
    }
}
