//! Collections (mixed items, layouts, the public switch) and the listen history (the setting,
//! the page, stats, moving and deleting it) over the router, without a homeserver
//! (STEWARD-SOCIAL-V1-MUSIC.md Part A; `testnet_sync.rs` has the homeserver side).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use id3::TagLike;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use std::sync::Arc;
use tower::ServiceExt;

const SONG: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
const RELEASE: &str = "833f00e1-781f-4edd-90e4-e52712618862";
const KEY: &str = "operrr8wsbpr3ue9d4qj41ge1kcc6r7fdiy6o3ugjrrhi4y77rdo";

fn write_wav(path: &std::path::Path, title: &str) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..2205 {
        w.write_sample((i % 50) as i16).unwrap();
        w.write_sample(-((i % 50) as i16)).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    t.set_title(title);
    t.set_artist("Tester");
    t.set_album("Lists");
    for (d, v) in [("MusicBrainz Track Id", SONG), ("MusicBrainz Album Id", RELEASE)] {
        t.add_frame(id3::frame::ExtendedText {
            description: d.into(),
            value: v.into(),
        });
    }
    t.write_to_path(path, id3::Version::Id3v24).unwrap();
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(match body {
            Some(b) => Body::from(b.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

/// A node with one tagged song (and so its album) in the library; returns it and the track id.
async fn node(dir: &std::path::Path) -> (Arc<NodeState>, axum::Router, i64) {
    let music = dir.join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("song.wav"), "Song");
    let db = dir.join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![music.clone()],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: None,
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        outputs,
    );
    s.zones_for_all_outputs();
    let app = router(s.clone());
    let (st, _) = call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::OK);
    for _ in 0..500 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, found) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
    let id = found["tracks"][0]["id"].as_i64().unwrap();
    (s, app, id)
}

#[tokio::test]
async fn a_collection_mixes_items_has_a_layout_and_a_switch() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, app, track_id) = node(dir.path()).await;
    let song = format!("https://musicbrainz.org/recording/{SONG}");
    let album = format!("https://musicbrainz.org/release/{RELEASE}");
    let artist = "https://musicbrainz.org/artist/056e4f3e-d505-4dad-8ec1-d04f521cbb56";
    let post = format!("pubky://{KEY}/pub/social/v1/posts/0033SCZXVEPNG");

    // Refused: a link that is neither music nor a public post, a layout nobody draws
    let (st, _) = call(
        &app,
        "POST",
        "/v1/collections",
        Some(serde_json::json!({"name": "x", "uris": ["https://example.com/a"]})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(
        &app,
        "POST",
        "/v1/collections",
        Some(serde_json::json!({"name": "x", "layout": "carousel"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, c) = call(
        &app,
        "POST",
        "/v1/collections",
        Some(serde_json::json!({
            "name": "Mixed",
            "uris": [song, album, artist, format!("pubky{KEY}/pub/social/v1/posts/0033SCZXVEPNG")],
            "layout": "Grid",
            "public": true
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{c}");
    let id = c["summary"]["id"].as_str().unwrap().to_string();
    assert_eq!(c["summary"]["layout"], "grid");
    assert_eq!(c["summary"]["visibility"], "public");
    assert_eq!(c["summary"]["tracks_only"], false);
    let kinds: Vec<&str> = c["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["track", "album", "artist", "post"]);
    assert_eq!(c["entries"][0]["track"]["id"], track_id);
    assert_eq!(c["entries"][1]["album"]["release_mbid"], RELEASE);
    assert_eq!(c["entries"][3]["uri"], post, "stored in the canonical form");
    assert_eq!(
        c["entries"][3]["post"],
        serde_json::json!({"user": KEY, "id": "0033SCZXVEPNG"})
    );

    // One resource, two names
    let (_, as_playlist) = call(&app, "GET", &format!("/v1/playlists/{id}"), None).await;
    assert_eq!(as_playlist["summary"], c["summary"]);
    let (_, list) = call(&app, "GET", "/v1/collections", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Layout and the switch change; a songs-only collection is a playlist
    let (st, c2) = call(
        &app,
        "PUT",
        &format!("/v1/collections/{id}"),
        Some(serde_json::json!({"layout": "visual"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{c2}");
    assert_eq!(c2["summary"]["layout"], "visual");
    let (_, c3) = call(
        &app,
        "PUT",
        &format!("/v1/collections/{id}/visibility"),
        Some(serde_json::json!({"public": false})),
    )
    .await;
    assert_eq!(c3["summary"]["visibility"], "private");

    // Playing: the song and the album's songs queue; an artist and a post don't play
    let (st, played) = call(
        &app,
        "POST",
        &format!("/v1/collections/{id}/play"),
        Some(serde_json::json!({"zone": "null-output"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{played}");
    assert_eq!(played["queued"], 2);
    assert_eq!(played["missing"], serde_json::json!([artist, post]));

    let (_, songs) = call(
        &app,
        "POST",
        "/v1/collections",
        Some(serde_json::json!({"name": "Songs", "track_ids": [track_id]})),
    )
    .await;
    assert_eq!(songs["summary"]["tracks_only"], true);
    assert_eq!(songs["summary"]["layout"], "list");

    // Someone's collection, bookmarked by its link
    let (st, b) = call(
        &app,
        "PUT",
        "/v1/bookmarks",
        Some(serde_json::json!({"uri": post})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        (b["uri"].as_str(), b["bookmarked"].as_bool()),
        (Some(post.as_str()), Some(true))
    );
    let (_, all) = call(&app, "GET", "/v1/bookmarks", None).await;
    assert_eq!(all["posts"], serde_json::json!([post]));
    let (st, _) = call(&app, "DELETE", &format!("/v1/bookmarks?uri={post}"), None).await;
    assert_eq!(st, StatusCode::OK);
    let (_, all) = call(&app, "GET", "/v1/bookmarks", None).await;
    assert_eq!(all["posts"], serde_json::json!([]));

    // A duplicate records its original; a link that is not a public post is refused
    let (st, d) = call(
        &app,
        "POST",
        "/v1/collections",
        Some(serde_json::json!({"name": "Copy", "original": format!("pubky{KEY}/pub/social/v1/posts/0033SCZXVEPNG")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{d}");
    assert_eq!(d["playlist"]["original"], post);
    let (st, _) = call(
        &app,
        "POST",
        "/v1/collections",
        Some(serde_json::json!({"name": "Copy", "original": "https://example.com"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_listen_history_follows_its_setting() {
    let dir = tempfile::tempdir().unwrap();
    let (s, app, track_id) = node(dir.path()).await;
    let song = format!("https://musicbrainz.org/recording/{SONG}");
    let play = |source: &'static str| serde_json::json!({"track": {"title": "Song", "uri": format!("https://musicbrainz.org/recording/{SONG}")}, "source": source});

    // Private by default
    let (_, set) = call(&app, "GET", "/v1/me/listens/settings", None).await;
    assert_eq!(set["history"], "private");
    let (st, r) = call(&app, "POST", "/v1/activity", Some(play("roon"))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["recorded"], true);
    // The node's own play of the same song a moment later is the same listen, shown once
    let (st, _) = call(
        &app,
        "POST",
        "/v1/library/history",
        Some(serde_json::json!({"track_id": track_id})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, h) = call(&app, "GET", "/v1/me/listens", None).await;
    let listens = h["listens"].as_array().unwrap();
    assert_eq!(listens.len(), 1, "{h}");
    assert_eq!(listens[0]["uri"], song);
    assert_eq!(listens[0]["visibility"], "private");
    assert_eq!(listens[0]["track"]["id"], track_id);
    assert_eq!(listens[0]["synced"], false);
    assert!(h["next"].is_null());

    // Public from now on: older listens stay where they are
    let (_, set) = call(
        &app,
        "PUT",
        "/v1/me/listens/settings",
        Some(serde_json::json!({"history": "public"})),
    )
    .await;
    assert_eq!(set["history"], "public");
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    call(&app, "POST", "/v1/activity", Some(play("tidal"))).await;
    let (_, set) = call(&app, "GET", "/v1/me/listens/settings", None).await;
    assert_eq!(
        (set["private"].as_u64(), set["public"].as_u64()),
        (Some(1), Some(1))
    );
    // Paged newest first
    let (_, p1) = call(&app, "GET", "/v1/me/listens?limit=1", None).await;
    assert_eq!(p1["listens"][0]["visibility"], "public");
    let next = p1["next"].as_i64().unwrap();
    let (_, p2) = call(
        &app,
        "GET",
        &format!("/v1/me/listens?limit=1&before={next}"),
        None,
    )
    .await;
    assert_eq!(p2["listens"][0]["visibility"], "private");

    // Stats
    let (st, stats) = call(&app, "GET", "/v1/me/listens/stats?period=week", None).await;
    assert_eq!(st, StatusCode::OK, "{stats}");
    assert_eq!(stats["listens"], 2);
    assert_eq!(stats["top_tracks"][0]["uri"], song);
    assert_eq!(stats["top_tracks"][0]["plays"], 2);
    assert_eq!(stats["top_artists"][0]["name"], "Tester");
    let (st, _) = call(&app, "GET", "/v1/me/listens/stats?period=decade", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Move the whole history private; the next sync moves them
    let (_, moved) = call(
        &app,
        "POST",
        "/v1/me/listens/visibility",
        Some(serde_json::json!({"public": false})),
    )
    .await;
    assert_eq!(moved["moved"], 1);
    let (_, set) = call(&app, "GET", "/v1/me/listens/settings", None).await;
    assert_eq!(
        (set["private"].as_u64(), set["public"].as_u64()),
        (Some(2), Some(0))
    );

    // Off: nothing is written
    call(
        &app,
        "PUT",
        "/v1/me/listens/settings",
        Some(serde_json::json!({"history": "off"})),
    )
    .await;
    let (st, r) = call(&app, "POST", "/v1/activity", Some(play("roon"))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        (r["recorded"].as_bool(), r["id"].as_str()),
        (Some(false), Some(""))
    );
    let (_, set) = call(&app, "GET", "/v1/me/listens/settings", None).await;
    assert_eq!(set["private"], 2);
    // The setting is one of the owner's settings the homeserver copy keeps
    assert_eq!(
        s.library
            .lock()
            .unwrap()
            .get_setting("listens.history")
            .unwrap()
            .as_deref(),
        Some("off")
    );

    // Delete: listen posts go; the node's own plays only with local=true
    let (_, del) = call(&app, "DELETE", "/v1/me/listens", None).await;
    assert_eq!(
        (del["deleted"].as_u64(), del["plays_cleared"].as_u64()),
        (Some(2), Some(0))
    );
    let (_, h) = call(&app, "GET", "/v1/me/listens", None).await;
    assert_eq!(
        h["listens"].as_array().unwrap().len(),
        1,
        "the node's own play: {h}"
    );
    assert_eq!(h["listens"][0]["visibility"], "node");
    let (_, del) = call(&app, "DELETE", "/v1/me/listens?local=true", None).await;
    assert_eq!(del["plays_cleared"], 1);
    let (_, h) = call(&app, "GET", "/v1/me/listens", None).await;
    assert!(h["listens"].as_array().unwrap().is_empty());
}
