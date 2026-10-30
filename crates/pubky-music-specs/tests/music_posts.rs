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
/// 2026-10-20 in Unix microseconds.
const PLAYED: i64 = 1_792_540_800_000_000;

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
fn listen_is_private_or_public() {
    let source = MusicSource::new("tidal", "https://tidal.com/track/50850120");
    let post = listen(PLAYED, Some(213_000), Some(source.clone()), SONG.into());
    let minted = create_version(&post, Root::Priv, &owner()).unwrap();
    assert!(minted.path.starts_with("/priv/music/v1/posts/"));

    let back = written(&post, Root::Priv).unwrap();
    let content = read_listen(&back).unwrap();
    assert_eq!(content.played_at, PLAYED);
    assert_eq!(content.duration_ms, Some(213_000));
    assert_eq!(content.source, Some(source));

    // A public history: the same listen under /pub/
    let minted = create_version(&post, Root::Pub, &owner()).unwrap();
    assert!(minted.path.starts_with("/pub/music/v1/posts/"), "{}", minted.path);
    let back = written(&post, Root::Pub).unwrap();
    assert_eq!(read_listen(&back).unwrap().played_at, PLAYED);

    // A source known only by its service
    let post = listen(PLAYED, None, Some(MusicSource::service("library")), SONG.into());
    let back = written(&post, Root::Priv).unwrap();
    assert_eq!(read_listen(&back).unwrap().source.unwrap().uri, None);
    written(&post, Root::Pub).unwrap();
    assert!(read_review(&back).is_err());
}

#[test]
fn a_public_listen_never_points_at_a_private_object() {
    // The person's own private file is fine in a private history
    let mine = MusicSource::new(
        "library",
        format!("pubky://{OWNER}/priv/social/v1/files/0033SCZXVEPNG"),
    );
    let post = listen(PLAYED, None, Some(mine), SONG.into());
    written(&post, Root::Priv).unwrap();
    // and refused under /pub/, where everyone reads it
    let err = written(&post, Root::Pub).unwrap_err();
    assert!(err.contains("private"), "{err}");
}

#[test]
fn listen_rules() {
    // Seconds instead of microseconds
    let err = written(&listen(1_792_540_800, None, None, SONG.into()), Root::Priv).unwrap_err();
    assert!(err.contains("microseconds"), "{err}");
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
fn a_source_may_name_only_a_service() {
    let it = item(
        SONG,
        json!({"ext": {"music": {"source": {"service": "library"}}}}),
    );
    assert_eq!(
        item_source(&it, &PUB, None),
        Some(MusicSource::service("library"))
    );
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

#[test]
fn item_added_at_round_trips_and_ignores_junk() {
    use pubky_music_specs::ext::{item_added_at, set_item_added_at};
    let mut it = item(SONG, json!({}));
    assert_eq!(item_added_at(&it), None);
    set_item_added_at(&mut it, PLAYED);
    assert_eq!(item_added_at(&it), Some(PLAYED));
    for junk in [
        json!("soon"),
        json!(12),
        json!(1.5e15),
        json!(9_007_199_254_740_993_u64),
    ] {
        let it = item(SONG, json!({"ext": {"music": {"added_at": junk}}}));
        assert_eq!(item_added_at(&it), None);
    }
    // An older writer's milliseconds read as microseconds
    let it = item(SONG, json!({"ext": {"music": {"added_at": PLAYED / 1000}}}));
    assert_eq!(item_added_at(&it), Some(PLAYED));
}

/// Times are microseconds; a time below 3000-01-01 in milliseconds is an older millisecond
/// value; anything before 2000 (seconds, say) is refused.
#[test]
fn times_are_microseconds_and_older_milliseconds_still_read() {
    use pubky_music_specs::{micros_of, MILLIS_BEFORE, TIME_MIN_MICROS};
    assert_eq!(micros_of(PLAYED), Some(PLAYED));
    assert_eq!(micros_of(PLAYED / 1000), Some(PLAYED));
    assert_eq!(micros_of(MILLIS_BEFORE - 1), Some((MILLIS_BEFORE - 1) * 1000));
    assert_eq!(micros_of(TIME_MIN_MICROS), Some(TIME_MIN_MICROS));
    // The two ranges never meet
    const { assert!(MILLIS_BEFORE < TIME_MIN_MICROS) };
    for bad in [MILLIS_BEFORE, TIME_MIN_MICROS - 1, 1_792_540_800, -1] {
        assert_eq!(micros_of(bad), None, "{bad}");
    }
    // A listen an older node wrote in milliseconds reads in microseconds
    let old = listen(PLAYED / 1000, None, None, SONG.into());
    let back = written(&old, Root::Priv).unwrap();
    assert_eq!(read_listen(&back).unwrap().played_at, PLAYED);
}

#[test]
fn listening_is_live() {
    use pubky_music_specs::{listening, read_listening};
    let src = MusicSource::new("tidal", "https://tidal.com/track/50850120");
    let post = listening(
        PLAYED,
        Some(PLAYED + 213_000_000),
        None,
        Some(213_000),
        Some(src),
        SONG.into(),
    );
    let back = written(&post, Root::Pub).unwrap();
    assert_eq!(back.kind, MusicPostKind::Listening);
    let c = read_listening(&back).unwrap();
    assert_eq!((c.started_at, c.expires_at), (PLAYED, Some(PLAYED + 213_000_000)));
    assert_eq!(serde_json::to_value(&back).unwrap()["embed"], SONG);
    // Expires after its song starts, within a day; embed names the music
    assert!(written(
        &listening(PLAYED, Some(PLAYED), None, None, None, SONG.into()),
        Root::Pub
    )
    .is_err());
    let day = pubky_music_specs::LIVE_WINDOW_MAX_MICROS;
    assert!(written(
        &listening(PLAYED, Some(PLAYED + day + 1), None, None, None, SONG.into()),
        Root::Pub
    )
    .is_err());
    let mut no_embed = listening(PLAYED, Some(PLAYED + 1), None, None, None, SONG.into());
    no_embed.embed = None;
    assert!(written(&no_embed, Root::Pub).is_err());
    // No expiry at all is fine: readers stop a day after started_at (which every version moves)
    let open = written(&listening(PLAYED, None, None, None, None, SONG.into()), Root::Pub).unwrap();
    let open = read_listening(&open).unwrap();
    assert!(
        open.is_live(PLAYED + day - 1) && !open.is_live(PLAYED + day),
        "not forever"
    );
}

/// Listening together (2026-10-29): `started_at` is when this song started; a pause and the end
/// are versions with `paused_at` or `ended_at`; a reader listens along at `position_ms`.
#[test]
fn listening_along_follows_the_position() {
    use pubky_music_specs::{listening_post, read_listening, MusicListeningContent};
    let base = MusicListeningContent {
        started_at: PLAYED,
        expires_at: Some(PLAYED + 3_600_000_000),
        duration_ms: Some(200_000),
        ..Default::default()
    };
    let playing = read_listening(&written(&listening_post(&base, SONG.into()), Root::Pub).unwrap()).unwrap();
    assert_eq!(playing.position_ms(PLAYED + 30_000_000), Some(30_000));
    assert_eq!(
        playing.position_ms(PLAYED + 900_000_000),
        Some(200_000),
        "never past the song"
    );
    assert_eq!(playing.position_ms(PLAYED + 3_600_000_000), None, "expired");
    let paused = MusicListeningContent {
        paused_at: Some(PLAYED + 42_000_000),
        ..base.clone()
    };
    let paused = read_listening(&written(&listening_post(&paused, SONG.into()), Root::Pub).unwrap()).unwrap();
    assert_eq!(
        paused.position_ms(PLAYED + 100_000_000),
        Some(42_000),
        "paused stays put"
    );
    let ended = MusicListeningContent {
        ended_at: Some(PLAYED + 60_000_000),
        ..base.clone()
    };
    let ended = read_listening(&written(&listening_post(&ended, SONG.into()), Root::Pub).unwrap()).unwrap();
    assert!(ended.is_live(PLAYED + 59_000_000) && !ended.is_live(PLAYED + 60_000_000));
    assert_eq!(ended.position_ms(PLAYED + 61_000_000), None);
    // Never before the song started
    for bad in [
        MusicListeningContent {
            paused_at: Some(PLAYED - 1),
            ..base.clone()
        },
        MusicListeningContent {
            ended_at: Some(PLAYED - 1),
            ..base.clone()
        },
    ] {
        let err = written(&listening_post(&bad, SONG.into()), Root::Pub).unwrap_err();
        assert!(err.contains("before started_at"), "{err}");
    }
}

/// `session` of the earlier drafts is retired: a listening covers it. An old one reads as an
/// unknown kind and fails validation.
#[test]
fn a_session_is_retired() {
    let kind = MusicPostKind::from("session".to_string());
    assert!(matches!(kind, MusicPostKind::Unknown(ref k) if k == "session"));
    let old = MusicPost::new(
        json!({"started_at": PLAYED, "host": format!("pubky://{OWNER}")}).to_string(),
        kind,
        None,
        Some(SONG.into()),
        Vec::new(),
    );
    assert!(written(&old, Root::Pub).is_err());
}

#[test]
fn a_listening_may_have_a_name() {
    use pubky_music_specs::{generic_listening_name, listening, read_listening, LIVE_NAME_MAX_LENGTH};
    let named = listening(
        PLAYED,
        None,
        Some("  Friday vinyl night  ".into()),
        None,
        None,
        SONG.into(),
    );
    let back = read_listening(&written(&named, Root::Pub).unwrap()).unwrap();
    assert_eq!(back.name.as_deref(), Some("Friday vinyl night"));
    assert_eq!(back.name_or_generic("Alice"), "Friday vinyl night");

    // Absent: the constructor drops a blank one, and a reader shows the generic name
    let unnamed = listening(PLAYED, None, Some(" ".into()), None, None, SONG.into());
    let back = read_listening(&written(&unnamed, Root::Pub).unwrap()).unwrap();
    assert_eq!(back.name, None);
    assert!(!unnamed.content.contains("name"), "{}", unnamed.content);
    assert_eq!(back.name_or_generic("Alice"), "Alice is listening");
    assert_eq!(back.name_or_generic("Alice"), generic_listening_name("Alice"));

    // The generic name is always a valid one: a blank display name, a long one
    assert_eq!(generic_listening_name("  "), "Someone is listening");
    let long = generic_listening_name(&"é".repeat(300));
    assert_eq!(long.chars().count(), LIVE_NAME_MAX_LENGTH);
    written(
        &listening(PLAYED, None, Some(long), None, None, SONG.into()),
        Root::Pub,
    )
    .unwrap();

    // Written by hand: untrimmed, blank or too long is refused
    for bad in [
        " x".to_string(),
        "x ".into(),
        "".into(),
        "  ".into(),
        "a".repeat(101),
    ] {
        let mut post = listening(PLAYED, None, None, None, None, SONG.into());
        let mut content: serde_json::Value = serde_json::from_str(&post.content).unwrap();
        content["name"] = json!(bad);
        post.content = content.to_string();
        let err = written(&post, Root::Pub).unwrap_err();
        assert!(err.contains("name"), "{bad:?}: {err}");
    }
}

/// `tests/vectors/live-v1.json`: `listening` contents with what each is shown as and its position at
/// `now`, the ones refused, and the generic names, for other implementations to check against.
#[test]
fn live_vectors() {
    use pubky_music_specs::{generic_listening_name, read_listening};
    let raw =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/live-v1.json")).unwrap();
    let vectors: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let post = |case: &serde_json::Value| {
        let kind = MusicPostKind::from(case["kind"].as_str().unwrap().to_string());
        let content = serde_json::to_string(&case["content"]).unwrap();
        MusicPost::new(content, kind, None, Some(SONG.into()), Vec::new())
    };
    let valid = vectors["valid"].as_array().unwrap();
    assert!(!valid.is_empty());
    for case in valid {
        let back = written(&post(case), Root::Pub).unwrap_or_else(|e| panic!("{case}: {e}"));
        let l = read_listening(&back).unwrap();
        assert_eq!(
            l.name_or_generic(case["display_name"].as_str().unwrap()),
            case["shown_as"].as_str().unwrap(),
            "{case}"
        );
        if let Some(now) = case["now"].as_i64() {
            assert_eq!(l.position_ms(now), case["position_ms"].as_i64(), "{case}");
        }
    }
    let invalid = vectors["invalid"].as_array().unwrap();
    assert!(!invalid.is_empty());
    for case in invalid {
        let err = written(&post(case), Root::Pub).unwrap_err();
        assert!(err.contains(case["error"].as_str().unwrap()), "{case}: {err}");
    }
    for case in vectors["generic_names"].as_array().unwrap() {
        let who = case["display_name"].as_str().unwrap();
        assert_eq!(
            generic_listening_name(who),
            case["listening"].as_str().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn service_names_are_hyphenated() {
    use pubky_music_specs::{service_name, SERVICE_NAMES};
    assert_eq!(service_name("Apple_Music").as_deref(), Some("apple-music"));
    assert_eq!(service_name(" youtube music ").as_deref(), Some("youtube-music"));
    assert_eq!(service_name("TIDAL").as_deref(), Some("tidal"));
    assert_eq!(service_name("tidal!"), None);
    assert!(SERVICE_NAMES
        .iter()
        .all(|n| pubky_music_specs::is_service_name(n) && !n.contains('_')));
    assert!(SERVICE_NAMES.contains(&"library") && SERVICE_NAMES.contains(&"roon"));
}

#[test]
fn a_duplicate_names_its_original() {
    use pubky_music_specs::ext::{original, set_original};
    let mut c: PubkySocialCollectionContent =
        serde_json::from_value(json!({"name": "Copy", "items": []})).unwrap();
    assert_eq!(original(&c, &PUB), None);
    let theirs = format!("pubky://{OWNER}/pub/social/v1/posts/0033SCZXVEPNG");
    set_original(&mut c, Some(&theirs));
    assert_eq!(original(&c, &PUB), Some(theirs.clone()));
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["ext"]["music"]["original"], theirs);
    // Only a versionless post reference reads; anything else is absent
    for bad in [
        "https://example.com/list".to_string(),
        format!("pubky://{OWNER}/pub/social/v1/posts/0033SCZXVEPNG/0033SCZXVEPNG.json"),
        format!("pubky://{OWNER}/pub/social/v1/profile.json"),
    ] {
        set_original(&mut c, Some(&bad));
        assert_eq!(original(&c, &PUB), None, "{bad}");
    }
    set_original(&mut c, None);
    assert!(
        serde_json::to_value(&c).unwrap().get("ext").is_none(),
        "tidied away"
    );
}
