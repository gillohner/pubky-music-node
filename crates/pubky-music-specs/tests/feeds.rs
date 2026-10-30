//! Music feeds (`/{priv,pub}/music/v1/feeds/{id}.json`): `tests/vectors/feed-v1.json`, for other
//! implementations to check against, and the writer's canonical forms.

use pubky_music_specs::{
    feed_tags, MusicFeed, MusicFeedConfig, MusicFeedContent, MusicFeedMatch, MusicFeedSort,
    PubkySocialFeedLayout, PubkySocialFeedReach, Root,
};

fn vectors() -> serde_json::Value {
    let raw =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/feed-v1.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn read(case: &serde_json::Value) -> MusicFeed {
    serde_json::from_value(case["feed"].clone()).unwrap_or_else(|e| panic!("{case}: {e}"))
}

#[test]
fn feed_vectors() {
    let v = vectors();
    let id = v["id"].as_str().unwrap();
    for case in v["valid"].as_array().unwrap() {
        let f = read(case);
        f.validate(Some(id), true)
            .unwrap_or_else(|e| panic!("{}: {e}", case["about"]));
        // Read back as written, unknown members and all
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            case["feed"],
            "{}",
            case["about"]
        );
    }
    for case in v["reader_only"].as_array().unwrap() {
        let f = read(case);
        f.validate(Some(id), false)
            .unwrap_or_else(|e| panic!("{}: a reader keeps it: {e}", case["about"]));
        assert!(
            f.validate(Some(id), true).is_err(),
            "{}: a writer refuses it",
            case["about"]
        );
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            case["feed"],
            "{}: kept as read",
            case["about"]
        );
    }
    for case in v["invalid"].as_array().unwrap() {
        let f = read(case);
        let err = f
            .validate(Some(id), false)
            .expect_err(case["about"].as_str().unwrap());
        let want = case["error"].as_str().unwrap();
        assert!(err.contains(want), "{}: {err} (wanted {want})", case["about"]);
    }
    for case in v["canonical"].as_array().unwrap() {
        let given: Vec<String> = serde_json::from_value(case["given"].clone()).unwrap();
        let stored: Vec<String> = serde_json::from_value(case["stored"].clone()).unwrap();
        assert_eq!(feed_tags(&given).unwrap(), Some(stored));
    }
}

#[test]
fn a_new_feed_is_canonical_and_lives_in_the_music_namespace() {
    let config = MusicFeedConfig {
        tags: Some(vec!["Techno".into(), "techno".into()]),
        tags_match: MusicFeedMatch::All,
        reach: PubkySocialFeedReach::Friends,
        sort: MusicFeedSort::Recent,
        content: MusicFeedContent::Songs,
        layout: PubkySocialFeedLayout::List,
        rules: Vec::new(),
        rules_match: None,
        leave_out: None,
        limit: None,
        seeds: vec![" https://musicbrainz.org/artist/056e4f3e-d505-4dad-8ec1-d04f521cbb56 ".into()],
        extra: Default::default(),
    };
    let f = MusicFeed::new(config, "  From friends  ", Some(" Disc "), 1_792_540_800_000_000).unwrap();
    assert_eq!(f.name, "From friends");
    assert_eq!(f.icon.as_deref(), Some("disc"));
    assert_eq!(f.feed.tags.as_deref(), Some(&["techno".to_string()][..]));
    assert_eq!(
        f.feed.seeds,
        ["https://musicbrainz.org/artist/056e4f3e-d505-4dad-8ec1-d04f521cbb56"]
    );
    f.validate(Some("0035Y0C10FNRP"), true).unwrap();
    let json = serde_json::to_value(&f).unwrap();
    assert_eq!(json["feed"]["match"], "all");
    assert_eq!(json["feed"]["reach"], "friends");
    assert_eq!(
        MusicFeed::paths("0035Y0C10FNRP").unwrap(),
        (
            "/priv/music/v1/feeds/0035Y0C10FNRP.json".to_string(),
            "/pub/music/v1/feeds/0035Y0C10FNRP.json".to_string()
        )
    );
    assert_eq!(MusicFeed::path(Root::Pub, "x"), "/pub/music/v1/feeds/x.json");
    assert!(MusicFeed::paths("not-an-id").is_err());
    assert!(MusicFeed::new(
        MusicFeedConfig {
            seeds: vec!["not a link".into()],
            ..f.feed.clone()
        },
        "x",
        None,
        1
    )
    .is_err());
}
