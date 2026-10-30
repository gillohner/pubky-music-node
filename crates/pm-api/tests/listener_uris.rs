//! What a listener may put in a queue (security review 2026-10-30, #1): library songs, the
//! services' tracks and links to public web addresses; never a file path, a `file:` URL or an
//! address on the node's machine or network, in a queue, a moved queue or an announcement. The
//! owner still may. And a listener who loses access loses the event socket too.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use std::sync::Arc;
use tower::ServiceExt;

const OWNER: &str = "owner-token";
const KEY: &str = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";

async fn call(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, String) {
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
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn a_listener_queues_library_songs_service_tracks_and_public_links_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
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
        OutputRegistry::new(),
    );
    let out: Arc<dyn pm_zones::Output> = Arc::new(NullOutput::new());
    let zone = s.create_zone("Kitchen", out).id().0.clone();
    let roster = serde_json::json!({"people": [
        {"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster.to_string())
        .unwrap();
    let listener = pm_api::accounts::mint(&s, KEY).unwrap();
    let app = router(s.clone());
    let (st, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/people/zones",
        Some(serde_json::json!({"zones": [zone]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let enqueue = |uri: &str| serde_json::json!({"items": [{"uri": uri}], "play": false});
    for bad in [
        "/etc/passwd",
        "file:///etc/passwd",
        "C:\\Users\\owner\\a.flac",
        "smb://nas/music/a.flac",
        "http://127.0.0.1:7788/v1/zones",
        "http://localhost:8080/",
        "http://169.254.169.254/latest/meta-data/",
        "http://192.168.1.10/a.mp3",
        "http://10.0.0.5:8000/stream",
        "http://[::1]/x",
        "http://[fd00::4]/x",
    ] {
        let (st, body) = call(
            &app,
            &listener,
            "POST",
            &format!("/v1/zones/{zone}/queue"),
            Some(enqueue(bad)),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{bad}: {body}");
        let (st, body) = call(
            &app,
            &listener,
            "POST",
            &format!("/v1/zones/{zone}/load"),
            Some(serde_json::json!({"items": [{"uri": bad}]})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "load {bad}: {body}");
    }
    let (st, body) = call(
        &app,
        &listener,
        "POST",
        &format!("/v1/zones/{zone}/announce"),
        Some(serde_json::json!({"uri": "/etc/passwd"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "announce a file: {body}");
    for ok in [
        "track:1",
        "tidal:track:123",
        "https://radio.example.org/stream.mp3",
    ] {
        let (st, body) = call(
            &app,
            &listener,
            "POST",
            &format!("/v1/zones/{zone}/queue"),
            Some(enqueue(ok)),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{ok}: {body}");
    }
    // The URI checked is the one queued (spaces around it), and a listener's item is fetched
    // only from public addresses, whatever limit it claims (review of #177)
    let (st, body) = call(
        &app,
        &listener,
        "POST",
        &format!("/v1/zones/{zone}/queue"),
        Some(serde_json::json!({"items": [{"uri": " http://169.254.169.254.nip.io/latest/ ", "limit": {"peer": "127.0.0.1", "lan": true}}], "play": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (_, q) = call(&app, OWNER, "GET", &format!("/v1/zones/{zone}/queue"), None).await;
    let q: serde_json::Value = serde_json::from_str(&q).unwrap();
    let items = q["items"]
        .as_array()
        .or(q.as_array())
        .cloned()
        .unwrap_or_default();
    let it = items
        .iter()
        .find(|i| i["uri"].as_str().is_some_and(|u| u.contains("nip.io")))
        .unwrap_or_else(|| panic!("{q}"));
    assert_eq!(it["uri"], "http://169.254.169.254.nip.io/latest/");
    assert_eq!(
        it["limit"],
        serde_json::json!({"peer": null, "lan": false}),
        "{it}"
    );
    // The owner plays their own files and LAN streams
    for ok in ["/music/a.flac", "http://192.168.1.10/a.mp3"] {
        let (st, body) = call(
            &app,
            OWNER,
            "POST",
            &format!("/v1/zones/{zone}/queue"),
            Some(enqueue(ok)),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "owner {ok}: {body}");
    }
}

/// A listener whose music access is taken away loses the event socket too, not only new
/// requests (security review 2026-10-30, low).
#[tokio::test]
async fn a_revoked_listener_loses_its_event_socket() {
    use futures_util::StreamExt;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
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
        OutputRegistry::new(),
    );
    let out: Arc<dyn pm_zones::Output> = Arc::new(NullOutput::new());
    let zone = s.create_zone("Kitchen", out).id().0.clone();
    let roster = |on: bool| {
        serde_json::json!({"people": [{"key": KEY, "access": "music", "music_enabled": on, "added_at": 0}]})
            .to_string()
    };
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster(true))
        .unwrap();
    let listener = pm_api::accounts::mint(&s, KEY).unwrap();
    let app = router(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(pm_api::serve_listener(s.clone(), l));
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/events?token={listener}"))
        .await
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .unwrap();
    assert!(first.is_some(), "the snapshot");
    // Access goes; past the recheck interval the next event closes the socket
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster(false))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5200)).await;
    let (st, _) = call(
        &app,
        OWNER,
        "POST",
        &format!("/v1/zones/{zone}/queue"),
        Some(serde_json::json!({"items": [{"uri": "/music/a.flac"}], "play": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) => return true,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return true,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert_eq!(closed, Ok(true), "the socket closed");
}
