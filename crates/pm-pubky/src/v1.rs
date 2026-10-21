//! The node's objects as pubky-social-specs v1 objects (ADR 0014).
//!
//! The node keeps its working copy in its own shape ([`MusicContent`], [`Favourite`](crate::Favourite)); this module
//! converts at the homeserver boundary, both ways:
//!
//! | Node object | v1 object | Path |
//! | :-- | :-- | :-- |
//! | [`Playlist`] (up to 100 songs) | social `collection`, `layout: list` | `/{root}/social/v1/posts/{id}/{editId}.json` |
//! | [`Review`] with a rating | music `review` (rating 1–10) | `/{root}/music/v1/posts/…` |
//! | [`Review`] without a rating | social `note` embedding the music URL | `/{root}/social/v1/posts/…` |
//! | [`Activity`] | music `listen` | `/priv/music/v1/posts/…` |
//! | [`Favourite`](crate::Favourite) | social `bookmark` | `/priv/social/v1/bookmarks/{filename}.json` |
//!
//! The local id (a timestamp id of the creation time) is the v1 post id, so a migrated object
//! keeps its identity. Creation writes `editId == id`; every later write is a new version above
//! the newest one on the homeserver, and readers take the newest.

use crate::ids;
use crate::schemas::{Activity, ActivitySource, ItemSource, MusicContent, Playlist, PlaylistItem, Review};
use pm_model::MusicUri;
use pubky_music_specs::ext;
use pubky_music_specs::{MusicPost, MusicPostKind, MusicSource, MUSIC_NAMESPACE};
use pubky_social_specs::traits::{HasIdPath, Validatable};
use pubky_social_specs::{
    bookmark_target, create_bookmark, namespace_path, validate_timestamp_id_format, PubkySocialBookmark,
    PubkySocialCollectionContent, PubkySocialCollectionItem, PubkySocialCollectionLayout, PubkySocialPost,
    PubkySocialPostKind, VALIDATION_LIMITS,
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

/// Why a node object has no v1 form (yet).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotV1 {
    /// Longer than a collection may be; the playlist stays in the old private envelope.
    #[error("{0} songs; a v1 collection holds at most {COLLECTION_MAX_ITEMS}")]
    TooLong(usize),
    #[error("{0}")]
    Invalid(String),
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
            (V1Post::Social(p), Some(head)) => Ok(p.edit_version(id, head, root, owner, None)?.path),
            (V1Post::Music(p), Some(head)) => {
                Ok(pubky_music_specs::edit_version(p, id, head, root, owner)?.path)
            }
            (V1Post::Social(p), None) => {
                p.validate(Some(id), &ctx)?;
                p.check_references(&ctx, Some(owner))?;
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
    if p.items.len() > COLLECTION_MAX_ITEMS {
        return Err(NotV1::TooLong(p.items.len()));
    }
    let items = p
        .items
        .iter()
        .map(|it| {
            let mut item = PubkySocialCollectionItem::new(it.uri.clone(), None);
            ext::set_item_added_at(&mut item, it.added_at / 1000);
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
    let mut post = PubkySocialPost::new_collection(
        p.name.clone(),
        p.description.clone(),
        items,
        p.image.clone(),
        Some(PubkySocialCollectionLayout::List),
    );
    if let Some(order) = p.source_order.as_ref().filter(|o| !o.is_empty()) {
        let mut content: PubkySocialCollectionContent =
            serde_json::from_str(&post.content).expect("the collection was just written");
        ext::set_source_order(&mut content, order);
        post.content = serde_json::to_string(&content).expect("a collection serializes");
    }
    // The content cap binds before the item cap for long URLs
    if post.content.chars().count() > VALIDATION_LIMITS.collection_content_max_length {
        return Err(NotV1::TooLong(p.items.len()));
    }
    Ok(post)
}

fn review_to_v1(r: &Review) -> V1Post {
    match r.rating {
        Some(rating) => V1Post::Music(pubky_music_specs::review(
            rating,
            Some(r.text.clone()),
            r.target_uri.clone(),
        )),
        None => V1Post::Social(PubkySocialPost::new(
            r.text.clone(),
            PubkySocialPostKind::Note,
            None,
            Some(r.target_uri.clone()),
            Vec::new(),
        )),
    }
}

fn activity_to_listen(a: &Activity) -> MusicPost {
    let source = match &a.source_kind {
        ActivitySource::Local => Some(MusicSource::service("library")),
        ActivitySource::Roon => Some(MusicSource::service("roon")),
        ActivitySource::Service { name } => {
            let name = name.to_ascii_lowercase();
            pubky_music_specs::is_service_name(&name).then(|| MusicSource::service(name))
        }
        ActivitySource::Unknown => None,
    };
    pubky_music_specs::listen(
        a.played_at / 1000,
        a.duration_ms.and_then(|d| i64::try_from(d).ok()),
        source,
        a.uri.clone(),
    )
}

/// Is this collection a music playlist? It is when it carries `ext.music`, or when every item is
/// a canonical music URL (a playlist another client made).
pub fn is_music_collection(content: &PubkySocialCollectionContent) -> bool {
    let has_ext = content
        .extra
        .get(ext::EXT)
        .and_then(|e| e.get(ext::MUSIC))
        .is_some();
    has_ext || (!content.items.is_empty() && content.items.iter().all(|i| MusicUri::parse(&i.uri).is_ok()))
}

/// Someone's public playlist, read from the bytes of one version of a social post: it must
/// pass the spec's public-root validation (with its author in scope), be a music collection,
/// and fit the node's own playlist rules. Anything else is `None`: other people's data is
/// hostile input.
pub fn read_public_playlist(bytes: &[u8], id: &str, edit_id: &str, owner: &PubkyId) -> Option<Playlist> {
    let post: PubkySocialPost = serde_json::from_slice(bytes).ok()?;
    let ctx = pubky_social_specs::ValidationCtx { root: Root::Pub };
    post.validate(Some(id), &ctx).ok()?;
    post.check_references(&ctx, Some(owner)).ok()?;
    let MusicContent::Playlist(p) = from_social(&post, id, edit_id)? else {
        return None;
    };
    let p = <Playlist as crate::schemas::Validatable>::sanitize(p);
    crate::schemas::Validatable::validate(&p, Some(id)).ok()?;
    Some(p)
}

/// A node object read back from a v1 social post (`id` and `edit_id` from its path): a playlist
/// from a music collection, a text review from a note that embeds a music URL. `None` for
/// anything else, or anything that does not validate.
pub fn from_social(post: &PubkySocialPost, id: &str, edit_id: &str) -> Option<MusicContent> {
    let created_at = micros_of(id)?;
    match post.kind {
        PubkySocialPostKind::Collection => {
            let content: PubkySocialCollectionContent = serde_json::from_str(&post.content).ok()?;
            if !is_music_collection(&content) {
                return None;
            }
            let items = content
                .items
                .iter()
                .filter_map(|it| {
                    let uri = MusicUri::parse(&it.uri).ok()?.to_url();
                    let added_at = ext::item_added_at(it).map_or(created_at, |ms| ms * 1000);
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
            Some(MusicContent::Playlist(Playlist {
                name: content.name,
                description: content.description,
                image: content.cover_image,
                items,
                created_at,
                updated_at: micros_of(edit_id)?.max(created_at),
                source_order: Some(order).filter(|o| !o.is_empty()),
            }))
        }
        PubkySocialPostKind::Note => {
            let target = MusicUri::parse(post.embed.as_deref()?).ok()?;
            let text = post.content.trim();
            if text.is_empty() {
                return None;
            }
            Some(MusicContent::Review(Review {
                target_uri: target.to_url(),
                rating: None,
                text: text.to_string(),
                created_at,
            }))
        }
        _ => None,
    }
}

/// A node review read back from a music `review` post.
pub fn from_music(post: &MusicPost, id: &str) -> Option<MusicContent> {
    if post.kind != MusicPostKind::Review {
        return None;
    }
    let review = pubky_music_specs::read_review(post).ok()?;
    let target = MusicUri::parse(post.embed.as_deref()?).ok()?;
    Some(MusicContent::Review(Review {
        target_uri: target.to_url(),
        rating: Some(review.rating),
        text: review.text.unwrap_or_default(),
        created_at: micros_of(id)?,
    }))
}

/// The bookmark of a favourite: the object and its path. `created_at` is kept.
pub fn favourite_bookmark(uri: &MusicUri, created_at: i64) -> Result<(PubkySocialBookmark, String), String> {
    let mut created = create_bookmark(&uri.to_url())?;
    created.bookmark.created_at = created_at;
    Ok((created.bookmark, created.path))
}

/// The path a favourite's bookmark lives at.
pub fn favourite_bookmark_path(uri: &MusicUri) -> Result<String, String> {
    let filename = pubky_social_specs::bookmark_filename(&uri.to_url())?;
    Ok(PubkySocialBookmark::create_path(&filename))
}

/// The music URL a stored bookmark names, `None` for a bookmark of something else or an invalid
/// one. `path` is the bookmark's path; `body` its JSON.
pub fn favourite_from_bookmark(path: &str, body: &[u8]) -> Option<(MusicUri, i64)> {
    let filename = path.rsplit('/').next()?.strip_suffix(".json")?;
    let content: PubkySocialBookmark = serde_json::from_slice(body).ok()?;
    let target = bookmark_target(filename, &content).ok()?;
    Some((MusicUri::parse(&target).ok()?, content.created_at))
}

/// A timestamp id for `micros`, the same encoding as v1 ids.
pub fn id_of(micros: i64) -> String {
    ids::timestamp_id(micros)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::MusicContent;

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
            (pl.items[1].added_at / 1000)
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
        assert_eq!(back.items[2].added_at, pl.items[2].added_at / 1000 * 1000);
    }

    #[test]
    fn long_playlists_stay_out_of_v1() {
        assert!(to_v1(&MusicContent::Playlist(playlist(COLLECTION_MAX_ITEMS))).is_ok());
        assert_eq!(
            to_v1(&MusicContent::Playlist(playlist(COLLECTION_MAX_ITEMS + 1))),
            Err(NotV1::TooLong(COLLECTION_MAX_ITEMS + 1))
        );
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
    fn rated_reviews_are_music_reviews_and_text_only_ones_are_notes() {
        let created = t0();
        let id = id_of(created);
        let rated = Review {
            target_uri: SONG.into(),
            rating: Some(4),
            text: "Lovely".into(),
            created_at: created,
        };
        let v1 = to_v1(&MusicContent::Review(rated.clone())).unwrap();
        assert_eq!(v1.namespace(), "music");
        let path = v1.version_path(&id, None, Root::Pub, &owner()).unwrap();
        assert_eq!(path, format!("/pub/music/v1/posts/{id}/{id}.json"));
        let V1Post::Music(post) = &v1 else { panic!() };
        assert_eq!(pubky_music_specs::read_review(post).unwrap().rating, 4);
        let MusicContent::Review(back) = from_music(post, &id).unwrap() else {
            panic!()
        };
        assert_eq!(back, rated);

        let plain = Review {
            rating: None,
            ..rated.clone()
        };
        let v1 = to_v1(&MusicContent::Review(plain.clone())).unwrap();
        assert_eq!(v1.namespace(), "social");
        v1.version_path(&id, None, Root::Priv, &owner()).unwrap();
        let V1Post::Social(note) = &v1 else { panic!() };
        assert_eq!(note.embed.as_deref(), Some(SONG));
        let MusicContent::Review(back) = from_social(note, &id, &id).unwrap() else {
            panic!()
        };
        assert_eq!(back, plain);

        // A rating alone, from another client
        let seven = pubky_music_specs::review(7, None, SONG.into());
        let MusicContent::Review(r) = from_music(&seven, &id).unwrap() else {
            panic!()
        };
        assert_eq!((r.rating, r.text.as_str()), (Some(7), ""));
        crate::schemas::MusicPost::new(MusicContent::Review(r))
            .validate(Some(&id))
            .unwrap();
    }

    #[test]
    fn plays_are_private_listens() {
        let a = Activity {
            uri: SONG.into(),
            played_at: t0(),
            source_kind: ActivitySource::Service { name: "TIDAL".into() },
            duration_ms: Some(200_000),
        };
        let id = id_of(a.played_at);
        let v1 = to_v1(&MusicContent::Activity(a.clone())).unwrap();
        v1.version_path(&id, None, Root::Priv, &owner()).unwrap();
        assert!(v1.version_path(&id, None, Root::Pub, &owner()).is_err());
        let V1Post::Music(post) = &v1 else { panic!() };
        let listen = pubky_music_specs::read_listen(post).unwrap();
        assert_eq!(listen.played_at, a.played_at / 1000);
        assert_eq!(listen.source.unwrap().service, "tidal");
    }

    #[test]
    fn favourites_are_v1_bookmarks() {
        let uri = MusicUri::parse(SONG).unwrap();
        let (b, path) = favourite_bookmark(&uri, 1_792_540_800_000_000).unwrap();
        assert!(path.starts_with("/priv/social/v1/bookmarks/"), "{path}");
        assert_eq!(favourite_bookmark_path(&uri).unwrap(), path);
        let body = serde_json::to_vec(&b).unwrap();
        assert_eq!(
            favourite_from_bookmark(&path, &body),
            Some((uri, 1_792_540_800_000_000))
        );
        // A bookmark of something that is not music
        let (other, opath) = {
            let c = create_bookmark("https://example.com/").unwrap();
            (c.bookmark, c.path)
        };
        assert_eq!(
            favourite_from_bookmark(&opath, &serde_json::to_vec(&other).unwrap()),
            None
        );
    }
}
