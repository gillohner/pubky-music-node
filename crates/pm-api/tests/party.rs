//! Party mode (party.rs): a guest link, the guest section of the queue, limits checked on the
//! node, and what a guest token may not do.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use id3::TagLike;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OWNER: &str = "owner-token";

/// A 20 s WAV, long enough to still be playing (or paused) while guests add songs.
fn write_wav(path: &std::path::Path, title: &str) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..44_100 * 20 {
        w.write_sample((i % 50) as i16).unwrap();
        w.write_sample(-((i % 50) as i16)).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    t.set_title(title);
    t.set_artist("Band");
    t.set_album("Party Songs");
    t.write_to_path(path, id3::Version::Id3v24).unwrap();
}

async fn call(
    app: &axum::Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = req
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn titles(q: &Value) -> Vec<String> {
    q.as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn guests_add_songs_in_their_section_within_the_limits() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    for t in ["A", "B", "C", "D", "E", "F"] {
        write_wav(&music.join(format!("{t}.wav")), &format!("Song {t}"));
    }
    let db = dir.path().join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![music.clone()],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some(OWNER.into()),
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://music.local:7788".into(),
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
    let own = Some(OWNER);
    call(&app, own, "POST", "/v1/library/scan", Some(json!({}))).await;
    for _ in 0..1000 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, found) = call(&app, own, "GET", "/v1/library/tracks?limit=10", None).await;
    let id = |title: &str| {
        found["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["title"] == title)
            .unwrap_or_else(|| panic!("{title} not scanned: {found}"))["id"]
            .as_i64()
            .unwrap()
    };
    let (_, zones) = call(&app, own, "GET", "/v1/zones", None).await;
    let zone = zones[0]["id"].as_str().unwrap().to_string();

    // The owner opens a party: 3 songs per guest per hour, 2 waiting at once
    let (st, party) = call(
        &app,
        own,
        "POST",
        "/v1/party",
        Some(json!({"zone": zone, "name": "Friday", "adds_per_hour": 3, "waiting_per_guest": 2})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{party}");
    let url = party["url"].as_str().unwrap();
    assert!(url.starts_with("http://music.local:7788/party/"), "{url}");
    assert!(party["qr_svg"].as_str().unwrap().starts_with("<svg"));
    let code = url.rsplit('/').next().unwrap().to_string();

    // The link's page needs no token; joining needs the right code
    let page = app
        .clone()
        .oneshot(
            Request::get(format!("/party/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let (st, _) = call(
        &app,
        None,
        "POST",
        "/v1/party/join",
        Some(json!({"code": "wrong"})),
    )
    .await;
    assert_eq!(st, StatusCode::GONE);
    let join = |name: &'static str| {
        let app = app.clone();
        let code = code.clone();
        async move {
            let (st, j) = call(
                &app,
                None,
                "POST",
                "/v1/party/join",
                Some(json!({"code": code, "name": name})),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{j}");
            j["token"].as_str().unwrap().to_string()
        }
    };
    let ann = join("Ann").await;
    let ben = join("Ben").await;
    assert!(ann.starts_with("pmg_"));

    // A guest token opens the party and nothing else
    for (m, path) in [
        ("GET", "/v1/zones"),
        ("GET", "/v1/party"),
        ("GET", "/v1/library/tracks"),
        ("POST", "/v1/library/scan"),
    ] {
        let (st, _) = call(&app, Some(&ann), m, path, Some(json!({}))).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {path}");
    }
    let (st, _) = call(
        &app,
        Some(&ann),
        "POST",
        &format!("/v1/zones/{zone}/transport"),
        Some(json!({"action": "next"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "no transport for guests");
    let (st, v) = call(&app, Some(&ann), "GET", "/v1/party/guest", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        (v["party"].as_str(), v["you"].as_str()),
        (Some("Friday"), Some("Ann"))
    );
    let (_, hits) = call(&app, Some(&ann), "GET", "/v1/party/guest/search?q=Song%20C", None).await;
    assert!(
        hits.as_array().unwrap().iter().any(|h| h["title"] == "Song C"),
        "{hits}"
    );

    // The owner's queue: A playing (paused, so it stays current), then B
    call(
        &app,
        own,
        "POST",
        &format!("/v1/zones/{zone}/queue"),
        Some(json!({"track_ids": [id("Song A"), id("Song B")], "play": true, "replace": true})),
    )
    .await;
    call(
        &app,
        own,
        "POST",
        &format!("/v1/zones/{zone}/transport"),
        Some(json!({"action": "pause"})),
    )
    .await;
    let add = |tok: &str, title: &str| {
        let app = app.clone();
        let tok = tok.to_string();
        let tid = id(title);
        async move {
            call(
                &app,
                Some(&tok),
                "POST",
                "/v1/party/guest/queue",
                Some(json!({"track_id": tid})),
            )
            .await
        }
    };
    let queue = || {
        let app = app.clone();
        let zone = zone.clone();
        async move {
            titles(
                &call(&app, Some(OWNER), "GET", &format!("/v1/zones/{zone}/queue"), None)
                    .await
                    .1,
            )
        }
    };

    // Guests' songs go after the current one and after each other, before the owner's B
    assert_eq!(add(&ann, "Song C").await.0, StatusCode::OK);
    assert_eq!(add(&ben, "Song D").await.0, StatusCode::OK);
    let (st, v) = add(&ann, "Song E").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(queue().await, ["Song A", "Song C", "Song D", "Song E", "Song B"]);
    let waiting: Vec<(&str, &str)> = v["waiting"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| (w["title"].as_str().unwrap(), w["by"].as_str().unwrap()))
        .collect();
    assert_eq!(waiting, [("Song C", "Ann"), ("Song D", "Ben"), ("Song E", "Ann")]);
    assert_eq!(
        v["waiting"][0]["track_id"],
        id("Song C"),
        "library songs keep their id: {v}"
    );
    assert_eq!(v["playing"]["track_id"], id("Song A"), "{v}");
    assert_eq!(v["waiting_left"], 0);

    // Ann has 2 waiting: refused on the node
    let (st, e) = add(&ann, "Song F").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{e}");
    assert!(e["error"].as_str().unwrap().contains("waiting"), "{e}");

    // Once one has played, Ann may add again; her third add this hour is her last
    call(
        &app,
        own,
        "POST",
        &format!("/v1/zones/{zone}/transport"),
        Some(json!({"action": "next"})),
    )
    .await; // C starts
    call(
        &app,
        own,
        "POST",
        &format!("/v1/zones/{zone}/transport"),
        Some(json!({"action": "pause"})),
    )
    .await;
    let (st, v) = add(&ann, "Song F").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["adds_left"], 0);
    assert_eq!(
        queue().await,
        ["Song A", "Song C", "Song D", "Song E", "Song F", "Song B"]
    );
    call(
        &app,
        own,
        "POST",
        &format!("/v1/zones/{zone}/transport"),
        Some(json!({"action": "next"})),
    )
    .await; // D
    call(
        &app,
        own,
        "POST",
        &format!("/v1/zones/{zone}/transport"),
        Some(json!({"action": "next"})),
    )
    .await; // E
    call(
        &app,
        own,
        "POST",
        &format!("/v1/zones/{zone}/transport"),
        Some(json!({"action": "pause"})),
    )
    .await;
    let (st, e) = add(&ann, "Song A").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{e}");
    assert!(e["error"].as_str().unwrap().contains("this hour"), "{e}");

    // The owner ends the party: guests' tokens stop
    let (st, list) = call(&app, own, "GET", "/v1/party", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list[0]["guests"], json!(["Ann", "Ben"]));
    assert_eq!(list[0]["added"], 4);
    let pid = list[0]["id"].as_str().unwrap();
    let (st, _) = call(&app, own, "DELETE", &format!("/v1/party/{pid}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(&app, Some(&ben), "GET", "/v1/party/guest", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = call(&app, None, "POST", "/v1/party/join", Some(json!({"code": code}))).await;
    assert_eq!(st, StatusCode::GONE);
}

#[tokio::test]
async fn a_guest_song_starts_an_idle_zone() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("solo.wav"), "Solo");
    let db = dir.path().join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![music.clone()],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some(OWNER.into()),
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
    call(&app, Some(OWNER), "POST", "/v1/library/scan", Some(json!({}))).await;
    for _ in 0..1000 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, found) = call(&app, Some(OWNER), "GET", "/v1/library/tracks?limit=1", None).await;
    let tid = found["tracks"][0]["id"].as_i64().unwrap();
    let (_, zones) = call(&app, Some(OWNER), "GET", "/v1/zones", None).await;
    let zone = zones[0]["id"].as_str().unwrap().to_string();
    let (_, party) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/party",
        Some(json!({"zone": zone})),
    )
    .await;
    let code = party["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    let (_, j) = call(&app, None, "POST", "/v1/party/join", Some(json!({"code": code}))).await;
    let tok = j["token"].as_str().unwrap();
    let (st, v) = call(
        &app,
        Some(tok),
        "POST",
        "/v1/party/guest/queue",
        Some(json!({"track_id": tid})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["you"], "Guest 1");
    let (_, z) = call(&app, Some(OWNER), "GET", &format!("/v1/zones/{zone}"), None).await;
    assert_eq!(z["transport"], "playing", "{z}");
    assert_eq!(v["playing"]["title"], "Solo", "{v}");
}
