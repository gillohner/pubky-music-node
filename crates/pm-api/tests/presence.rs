//! Who on this node is listening to what, and listening along (presence.rs, listening together
//! L3): hidden by default, shown as each person chooses; listening along plays the host's music
//! on both zones' speakers as one group, and leaving moves it back.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_model::{AudioFormat, OutputCaps};
use pm_zones::outputs::NullOutput;
use pm_zones::{AudioSink, Output, OutputId, OutputRegistry, SinkError};
use std::sync::Arc;
use tower::ServiceExt;

const OWNER: &str = "owner-token";
const KEY: &str = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";

/// A speaker of its own: a real-time null output under another id and name.
struct Room(&'static str, NullOutput);

#[async_trait]
impl Output for Room {
    fn id(&self) -> OutputId {
        OutputId::new("room", &self.0.to_lowercase())
    }
    fn name(&self) -> String {
        self.0.into()
    }
    fn caps(&self) -> &OutputCaps {
        self.1.caps()
    }
    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        self.1.open(fmt).await
    }
}

/// A 20-second song.
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
    t.write_to_path(path, id3::Version::Id3v24).unwrap();
}

async fn call(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_is_chosen_per_person_and_listening_along_groups_the_speakers() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("song.wav"));
    let db = dir.path().join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(Room("Kitchen", NullOutput::realtime())));
    outputs.register(Arc::new(Room("Den", NullOutput::realtime())));
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
    // A listener with music access, on both zones
    let roster = serde_json::json!({"people": [
        {"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
    {
        let lib = s.library.lock().unwrap();
        lib.set_setting("people", &roster.to_string()).unwrap();
        lib.set_setting("accounts.listener_zones", r#"["kitchen","den"]"#)
            .unwrap();
    }
    let listener = pm_api::accounts::mint(&s, KEY).unwrap();
    let app = router(s.clone());
    call(
        &app,
        OWNER,
        "POST",
        "/v1/library/scan",
        Some(serde_json::json!({})),
    )
    .await;
    for _ in 0..500 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, found) = call(&app, OWNER, "GET", "/v1/library/tracks?limit=10", None).await;
    let track = found["tracks"][0]["id"].as_i64().unwrap();

    // The owner (not signed in to Pubky: "owner") plays in the kitchen
    let (st, r) = call(
        &app,
        OWNER,
        "POST",
        "/v1/zones/kitchen/queue",
        Some(serde_json::json!({"track_ids": [track], "replace": true, "play": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let mut playing = false;
    for _ in 0..100 {
        let (_, mine) = call(&app, OWNER, "GET", "/v1/presence", None).await;
        if mine[0]["playing"] == true {
            assert_eq!(
                (mine[0]["key"].as_str(), mine[0]["you"].as_bool()),
                (Some("owner"), Some(true))
            );
            assert_eq!(mine[0]["title"], "Song");
            playing = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(playing, "the owner sees themself listening");

    // Hidden by default: the listener sees nobody, and can't listen along
    let (_, v) = call(&app, &listener, "GET", "/v1/me/presence", None).await;
    assert_eq!(v["visible"], "hidden", "{v}");
    let (st, seen) = call(&app, &listener, "GET", "/v1/presence", None).await;
    assert_eq!(st, StatusCode::OK, "{seen}");
    assert_eq!(seen, serde_json::json!([]));
    let join = serde_json::json!({"key": "owner", "zone": "den"});
    let (st, _) = call(&app, &listener, "POST", "/v1/presence/along", Some(join.clone())).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a hidden person isn't there to join");
    // "Followed" needs Nexus to say so: without it, still nobody
    let (st, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/me/presence",
        Some(serde_json::json!({"visible": "followed"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, seen) = call(&app, &listener, "GET", "/v1/presence", None).await;
    assert_eq!(seen, serde_json::json!([]));
    // Shown to everyone: the listener sees the owner in the kitchen
    call(
        &app,
        OWNER,
        "PUT",
        "/v1/me/presence",
        Some(serde_json::json!({"visible": "everyone"})),
    )
    .await;
    let (_, seen) = call(&app, &listener, "GET", "/v1/presence", None).await;
    assert_eq!(seen[0]["key"], "owner", "{seen}");
    assert_eq!(seen[0]["you"], false);
    assert_eq!(seen[0]["zone"]["id"], "kitchen");
    assert_eq!(seen[0]["can_join"], true);

    // Listen along from the den: one group on both speakers, the owner's song where it was
    let (st, group) = call(&app, &listener, "POST", "/v1/presence/along", Some(join.clone())).await;
    assert_eq!(st, StatusCode::OK, "{group}");
    assert_eq!(group["name"], "Kitchen + Den", "{group}");
    assert_eq!(group["now_playing"]["item"]["title"], "Song", "{group}");
    let gid = group["id"].as_str().unwrap().to_string();
    let (_, kitchen) = call(&app, OWNER, "GET", "/v1/zones/kitchen", None).await;
    assert_eq!(kitchen["transport"], "stopped", "the music moved: {kitchen}");
    let (st, _) = call(&app, &listener, "POST", "/v1/presence/along", Some(join)).await;
    assert_eq!(st, StatusCode::CONFLICT, "once is enough");
    // The owner still listens, now on the group; the listener is along only once they show it
    let (_, seen) = call(&app, OWNER, "GET", "/v1/presence", None).await;
    assert_eq!(seen[0]["zone"]["id"], gid.as_str(), "{seen}");
    assert_eq!(seen[0]["along"], serde_json::json!([]));
    assert_eq!(seen[0]["mine"], false, "{seen}");
    let (_, theirs) = call(&app, &listener, "GET", "/v1/presence", None).await;
    assert_eq!(
        theirs[0]["mine"], true,
        "the listener is along with them: {theirs}"
    );
    assert_eq!(theirs[0]["can_join"], false, "{theirs}");
    assert!(theirs[0]["name"].is_null(), "no profile for the owner: {theirs}");
    call(
        &app,
        &listener,
        "PUT",
        "/v1/me/presence",
        Some(serde_json::json!({"visible": "everyone"})),
    )
    .await;
    let (_, seen) = call(&app, OWNER, "GET", "/v1/presence", None).await;
    assert_eq!(seen[0]["along"], serde_json::json!([KEY]), "{seen}");
    // The listener may pause the group: both of its zones are open to listeners
    let (st, r) = call(
        &app,
        &listener,
        "POST",
        &format!("/v1/zones/{gid}/transport"),
        Some(serde_json::json!({"action": "pause"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    call(
        &app,
        &listener,
        "POST",
        &format!("/v1/zones/{gid}/transport"),
        Some(serde_json::json!({"action": "play"})),
    )
    .await;

    // Leaving: the group goes, the music goes back to the kitchen
    let (st, back) = call(&app, &listener, "DELETE", "/v1/presence/along", None).await;
    assert_eq!(st, StatusCode::OK, "{back}");
    assert_eq!(back["id"], "kitchen", "{back}");
    assert_eq!(back["now_playing"]["item"]["title"], "Song", "{back}");
    let (st, _) = call(&app, OWNER, "GET", &format!("/v1/zones/{gid}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "the group is gone");
    let (st, _) = call(&app, &listener, "DELETE", "/v1/presence/along", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "not along any more");
    // Pausing the group didn't make it the listener's: the owner listens in the kitchen again
    let (_, seen) = call(&app, OWNER, "GET", "/v1/presence", None).await;
    assert_eq!(
        (seen[0]["key"].as_str(), seen[0]["zone"]["id"].as_str()),
        (Some("owner"), Some("kitchen")),
        "{seen}"
    );
}
