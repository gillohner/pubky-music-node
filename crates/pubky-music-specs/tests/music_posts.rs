//! Reviews, listens and the playlist `ext.music` hints against the v1 envelope.

use pubky_music_specs::ext::{
    item_source, set_item_source, set_source_order, source_order, SOURCE_ORDER_MAX,
};
use pubky_music_specs::{
    create_version, listen, read_listen, read_review, review, MusicPost, MusicPostKind, MusicSource, PubkyId,
    Root, ValidationCtx,
};
use pubky_social_specs::traits::Validatable;
use pubky_social_specs::{PubkySocialCollectionContent, PubkySocialCollectionItem};
use serde_json::json;

const OWNER: &str = "operrr8wsbpr3ue9d4qj41ge1kcc6r7fdiy6o3ugjrrhi4y77rdo";
const SONG: &str = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69";
const PUB: ValidationCtx = ValidationCtx { root: Root::Pub };
const PLAYED: i64 = 1_792_540_800_000;

fn owner() -> PubkyId {
    PubkyId::try_from(OWNER).unwrap()
}

/// Validates the post as it would be written: minted id, then the written bytes read back.
fn written(post: &MusicPost, root: Root) -> Result<MusicPost, String> {
    let minted = create_version(post, root, &owner())?;
    let ctx = ValidationCtx { root };
    post.validate(Some(&minted.id), &ctx)?;
    let bytes = serde_json::to_vec(post).unwrap();
    <MusicPost as Validatable>::try_from(&bytes, &minted.id, &ctx)
}

#[test]
fn review_round_trips_under_the_music_namespace() {
    let post = review(7, Some("  Still great.  ".into()), SONG.into());
    let minted = create_version(&post, Root::Pub, &owner()).unwrap();
    assert!(minted.path.starts_with("/pub/music/v1/posts/"), "{}", minted.path);
    assert!(minted
        .path
        .ends_with(&format!("{}/{}.json", minted.id, minted.id)));

    let back = written(&post, Root::Pub).unwrap();
    assert_eq!(back.kind, MusicPostKind::Review);
    let content = read_review(&back).unwrap();
    assert_eq!(content.rating, 7);
    assert_eq!(content.text.as_deref(), Some("Still great."));
    let wire: serde_json::Value = serde_json::to_value(&back).unwrap();
    assert_eq!(wire["kind"], "review");
    assert_eq!(wire["embed"], SONG);

    // A private draft is a review too
    written(&post, Root::Priv).unwrap();
}

#[test]
fn review_rules() {
    for rating in [0, 11, 255] {
        let err = written(&review(rating, None, SONG.into()), Root::Pub).unwrap_err();
        assert!(err.contains("rating"), "{rating}: {err}");
    }
    for rating in [1, 10] {
        written(&review(rating, None, SONG.into()), Root::Pub).unwrap();
    }
    // A blank text is left out by the builder, refused on the wire
    assert_eq!(
        read_review(&review(5, Some("  ".into()), SONG.into()))
            .unwrap()
            .text,
        None
    );
    let mut post = review(5, None, SONG.into());
    post.content = json!({"rating": 5, "text": " "}).to_string();
    assert!(written(&post, Root::Pub).unwrap_err().contains("blank"));
    post.content = json!({"rating": 5, "text": "x".repeat(2001)}).to_string();
    assert!(written(&post, Root::Pub).unwrap_err().contains("2000"));

    // No embed, a parent, or no JSON envelope
    let mut post = review(5, None, SONG.into());
    post.embed = None;
    assert!(written(&post, Root::Pub).unwrap_err().contains("embed"));
    let mut post = review(5, None, SONG.into());
    post.parent = Some(format!("pubky://{OWNER}/pub/social/v1/posts/0033SCZXVEPNG"));
    assert!(written(&post, Root::Pub).unwrap_err().contains("parent"));
    let mut post = review(5, None, SONG.into());
    post.content = "five stars".into();
    assert!(written(&post, Root::Pub).unwrap_err().contains("JSON"));
}

#[test]
fn unknown_members_are_kept_but_may_not_shadow_or_overflow() {
    let mut post = review(8, None, SONG.into());
    post.content = json!({"rating": 8, "mood": "rainy", "ext": {"x": 1}}).to_string();
    let back = written(&post, Root::Pub).unwrap();
    let content = read_review(&back).unwrap();
    assert_eq!(content.extra["mood"], "rainy");
    // Re-serialized, the unknown members are still there
    let again = serde_json::to_value(&content).unwrap();
    assert_eq!(again["ext"]["x"], 1);

    post.content = json!({"rating": 8, "big": 9_007_199_254_740_993_u64}).to_string();
    assert!(written(&post, Root::Pub).unwrap_err().contains("JSON-safe"));
}

#[test]
fn unknown_kind_reads_and_fails_validation() {
    let raw = json!({"content": "{}", "kind": "scrobble", "parent": null, "embed": SONG,
        "attachments": []});
    let post: MusicPost = serde_json::from_value(raw).unwrap();
    assert_eq!(post.kind, MusicPostKind::Unknown("scrobble".into()));
    assert_eq!(serde_json::to_value(&post).unwrap()["kind"], "scrobble");
    assert!(written(&post, Root::Pub).unwrap_err().contains("unknown"));
}

#[test]
fn listen_is_private_only() {
    let source = MusicSource::new("tidal", "https://tidal.com/track/50850120");
    let post = listen(PLAYED, Some(213_000), Some(source.clone()), SONG.into());
    let minted = create_version(&post, Root::Priv, &owner()).unwrap();
    assert!(minted.path.starts_with("/priv/music/v1/posts/"));

    let back = written(&post, Root::Priv).unwrap();
    let content = read_listen(&back).unwrap();
    assert_eq!(content.played_at, PLAYED);
    assert_eq!(content.duration_ms, Some(213_000));
    assert_eq!(content.source, Some(source));

    let err = written(&post, Root::Pub).unwrap_err();
    assert!(err.contains("private"), "{err}");
    assert!(read_review(&back).is_err());
}

#[test]
fn listen_rules() {
    // Seconds instead of milliseconds
    let err = written(&listen(1_792_540_800, None, None, SONG.into()), Root::Priv).unwrap_err();
    assert!(err.contains("milliseconds"), "{err}");
    let err = written(&listen(PLAYED, Some(-1), None, SONG.into()), Root::Priv).unwrap_err();
    assert!(err.contains("duration_ms"), "{err}");
    let err = written(
        &listen(PLAYED, Some(25 * 3_600_000), None, SONG.into()),
        Root::Priv,
    )
    .unwrap_err();
    assert!(err.contains("duration_ms"), "{err}");
    // Service names are lowercase tokens
    let bad = MusicSource::new("TIDAL", "https://tidal.com/track/1");
    let err = written(&listen(PLAYED, None, Some(bad), SONG.into()), Root::Priv).unwrap_err();
    assert!(err.contains("service"), "{err}");
    // The source uri goes through the reference gate
    let bad = MusicSource::new("tidal", "not a uri");
    assert!(written(&listen(PLAYED, None, Some(bad), SONG.into()), Root::Priv).is_err());
    let bad = MusicSource::new("tidal", "javascript:alert(1)");
    assert!(written(&listen(PLAYED, None, Some(bad), SONG.into()), Root::Priv).is_err());
    // No attachments on a listen
    let mut post = listen(PLAYED, None, None, SONG.into());
    post.attachments = serde_json::from_value(json!([{"uri": format!(
        "pubky://{OWNER}/priv/social/v1/files/0033SCZXVEPNG"
    )}]))
    .unwrap();
    assert!(written(&post, Root::Priv).is_err());
}

fn item(uri: &str, extra: serde_json::Value) -> PubkySocialCollectionItem {
    let mut v = json!({"uri": uri});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

#[test]
fn item_source_round_trips_and_keeps_other_extensions() {
    let mut it = item(
        SONG,
        json!({"ext": {"badge": {"gold": true}, "music": {"mood": "x"}}}),
    );
    let source = MusicSource::new("tidal", "https://tidal.com/track/50850120");
    set_item_source(&mut it, Some(&source));
    assert_eq!(item_source(&it, &PUB, None), Some(source));
    let wire = serde_json::to_value(&it).unwrap();
    assert_eq!(wire["ext"]["badge"]["gold"], true);
    assert_eq!(wire["ext"]["music"]["mood"], "x");
    assert_eq!(wire["ext"]["music"]["source"]["service"], "tidal");

    set_item_source(&mut it, None);
    let wire = serde_json::to_value(&it).unwrap();
    assert!(wire["ext"]["music"].get("source").is_none());
    assert_eq!(wire["ext"]["music"]["mood"], "x");

    // Clearing the only member leaves no empty ext behind
    let mut plain = item(SONG, json!({}));
    set_item_source(
        &mut plain,
        Some(&MusicSource::new("deezer", "https://www.deezer.com/track/1")),
    );
    set_item_source(&mut plain, None);
    assert!(serde_json::to_value(&plain).unwrap().get("ext").is_none());
}

#[test]
fn hostile_item_sources_are_dropped() {
    for ext in [
        json!({"ext": "music"}),
        json!({"ext": {"music": []}}),
        json!({"ext": {"music": {"source": "tidal"}}}),
        json!({"ext": {"music": {"source": {"service": "Tidal", "uri": "https://tidal.com/track/1"}}}}),
        json!({"ext": {"music": {"source": {"service": "tidal", "uri": "javascript:alert(1)"}}}}),
        json!({"ext": {"music": {"source": {"service": "tidal"}}}}),
        json!({"ext": {"music": {"source": {"service": "tidal", "uri": "https://t/1", "service ": 1, "n": 9_007_199_254_740_993_u64}}}}),
    ] {
        let it = item(SONG, ext.clone());
        assert_eq!(item_source(&it, &PUB, None), None, "{ext}");
    }
    // A malformed ext is replaced only when music writes into it
    let mut it = item(SONG, json!({"ext": 5}));
    set_item_source(
        &mut it,
        Some(&MusicSource::new("tidal", "https://tidal.com/track/1")),
    );
    assert!(item_source(&it, &PUB, None).is_some());
}

#[test]
fn source_order_is_filtered_and_bounded() {
    let mut collection: PubkySocialCollectionContent = serde_json::from_value(json!({
        "name": "Mix",
        "items": [],
        "ext": {"music": {"source_order": ["tidal", "TIDAL", 3, "library", "tidal", "", "a b"]}}
    }))
    .unwrap();
    assert_eq!(source_order(&collection), vec!["tidal", "library"]);

    let many: Vec<String> = (0..40).map(|i| format!("s{i}")).collect();
    set_source_order(&mut collection, &many);
    assert_eq!(source_order(&collection).len(), SOURCE_ORDER_MAX);

    set_source_order(&mut collection, &[]);
    assert!(serde_json::to_value(&collection).unwrap().get("ext").is_none());
    assert!(source_order(&collection).is_empty());
}
