//! "Share what I'm listening to" (listening together, L2): off by default; on, the owner's
//! playing is one public `listening` post, a new version per song, pause or resume, and a last
//! version with `ended_at` on stop. Through the real path (a zone playing a tagged song in real
//! time) against a testnet homeserver (`PM_TESTNET=1`); the sign-in and owner rules without one.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::sync::PubkySync;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_pubky::homeserver::NodeHomeserver;
use pm_pubky::Visibility;
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use pubky::{ClientId, Keypair, Pubky, PublicKey};
use std::sync::Arc;
use tower::ServiceExt;

const SONG: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
const OWNER: &str = "owner-token";
const TESTNET_HOMESERVER: &str = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";

/// A 20-second song tagged with its MusicBrainz recording.
fn write_wav(path: &std::path::Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 8_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..(8_000 * 20) {
        w.write_sample((i % 50) as i16).unwrap();
        w.write_sample(-((i % 50) as i16)).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    id3::TagLike::set_title(&mut t, "Song");
    id3::TagLike::set_artist(&mut t, "Tester");
    id3::TagLike::add_frame(
        &mut t,
        id3::frame::ExtendedText {
            description: "MusicBrainz Track Id".into(),
            value: SONG.into(),
        },
    );
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
        .header("authorization", format!("Bearer {OWNER}"))
        .header("content-type", "application/json")
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn node(dir: &std::path::Path) -> (Arc<NodeState>, axum::Router, i64) {
    let music = dir.join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("song.wav"));
    let db = dir.join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::realtime()));
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![music.clone()],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some(OWNER.into()),
            subsonic_user: "pubky".into(),
            pubky: pm_pubky::PmPubkyConfig {
                network: pm_pubky::config::Network::Testnet,
                ..Default::default()
            },
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
async fn sharing_is_off_by_default_and_needs_a_pubky_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, app, _) = node(dir.path()).await;
    let (st, v) = call(&app, "GET", "/v1/me/listening", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["share"], false);
    let (st, e) = call(
        &app,
        "PUT",
        "/v1/me/listening",
        Some(serde_json::json!({"share": true})),
    )
    .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED, "{e}");
    let (st, v) = call(
        &app,
        "PUT",
        "/v1/me/listening",
        Some(serde_json::json!({"name": "  Friday vinyl night  "})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        (v["share"].as_bool(), v["name"].as_str()),
        (Some(false), Some("Friday vinyl night"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owners_listening_is_one_post_versioned_per_change() {
    if std::env::var("PM_TESTNET").as_deref() != Ok("1") {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    signer
        .signup(&PublicKey::try_from(TESTNET_HOMESERVER).unwrap(), None)
        .await
        .unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-listening").unwrap())
        .await
        .unwrap();
    let z32 = session.public_key().to_string();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let dir = tempfile::tempdir().unwrap();
    let (s, app, track) = node(dir.path()).await;
    s.set_sync(Some(Arc::new(PubkySync::from_session(
        homeserver.clone(),
        session,
    ))));
    let (st, v) = call(
        &app,
        "PUT",
        "/v1/me/listening",
        Some(serde_json::json!({"share": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["post"].is_null(), "nothing plays yet: {v}");
    // Every version of the post, oldest first, once there are `n`
    let versions = |n: usize| {
        let (hs, z32, s) = (homeserver.clone(), z32.clone(), s.clone());
        async move {
            for _ in 0..200 {
                let id = {
                    let r = router(s.clone());
                    let (_, v) = call(&r, "GET", "/v1/me/listening", None).await;
                    v["post"]
                        .as_str()
                        .map(|p| p.rsplit('/').next().unwrap().to_string())
                };
                if let Some(id) = id {
                    let mut paths = hs
                        .public_paths(&z32, &format!("/pub/music/v1/posts/{id}/"))
                        .await
                        .unwrap();
                    if paths.len() >= n {
                        paths.sort();
                        let mut out = Vec::new();
                        for p in paths {
                            let p = if p.starts_with('/') {
                                p
                            } else {
                                format!("/pub/music/v1/posts/{id}/{p}")
                            };
                            let body: serde_json::Value = serde_json::from_slice(
                                &hs.get_bytes(&z32, &p, Visibility::Public, None).await.unwrap(),
                            )
                            .unwrap();
                            let content: serde_json::Value =
                                serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
                            out.push((body, content));
                        }
                        return (id, out);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            panic!("no {n} versions came");
        }
    };
    let (st, r) = call(
        &app,
        "POST",
        "/v1/zones/null-output/queue",
        Some(serde_json::json!({"track_ids": [track], "replace": true, "play": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let (id, v1) = versions(1).await;
    let (body, content) = &v1[0];
    assert_eq!(body["kind"], "listening");
    assert_eq!(body["embed"], format!("https://musicbrainz.org/recording/{SONG}"));
    assert!(
        content.get("paused_at").is_none() && content.get("ended_at").is_none(),
        "{content}"
    );
    let started = content["started_at"].as_i64().unwrap();
    assert!(content["expires_at"].as_i64().unwrap() > started);
    // Pause: a new version of the same post, paused
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    call(
        &app,
        "POST",
        "/v1/zones/null-output/transport",
        Some(serde_json::json!({"action": "pause"})),
    )
    .await;
    let (id2, v2) = versions(2).await;
    assert_eq!(id2, id, "the same post");
    assert!(v2[1].1["paused_at"].as_i64().unwrap() >= started, "{}", v2[1].1);
    // Stop: the last version ends the session, and the next play starts a new post
    call(
        &app,
        "POST",
        "/v1/zones/null-output/transport",
        Some(serde_json::json!({"action": "stop"})),
    )
    .await;
    let mut ended = None;
    for _ in 0..200 {
        let paths = homeserver
            .public_paths(&z32, &format!("/pub/music/v1/posts/{id}/"))
            .await
            .unwrap();
        if paths.len() >= 3 {
            let mut paths = paths;
            paths.sort();
            let p = paths.last().unwrap().clone();
            let p = if p.starts_with('/') {
                p
            } else {
                format!("/pub/music/v1/posts/{id}/{p}")
            };
            let body: serde_json::Value = serde_json::from_slice(
                &homeserver
                    .get_bytes(&z32, &p, Visibility::Public, None)
                    .await
                    .unwrap(),
            )
            .unwrap();
            ended =
                Some(serde_json::from_str::<serde_json::Value>(body["content"].as_str().unwrap()).unwrap());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let ended = ended.expect("an ended version");
    assert!(ended["ended_at"].as_i64().is_some(), "{ended}");
    let (_, v) = call(&app, "GET", "/v1/me/listening", None).await;
    assert!(v["post"].is_null(), "the session ended: {v}");
}
