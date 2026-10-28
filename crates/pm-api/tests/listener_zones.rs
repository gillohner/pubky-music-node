//! A listener sees and plays only the zones the owner opened to listeners: the zone list, the
//! event socket's snapshot and events, and a transfer's target (security review 2026-10-27, #5).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
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
async fn a_listener_sees_and_plays_only_the_zones_open_to_listeners() {
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
    let open = s.create_zone("Kitchen", out.clone()).id().0.clone();
    let closed = s.create_zone("Office", out).id().0.clone();
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
        Some(serde_json::json!({"zones": [open]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // The zone list
    let (_, body) = call(&app, &listener, "GET", "/v1/zones", None).await;
    let ids: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&body)
        .unwrap()
        .iter()
        .map(|z| z["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![open.clone()], "{body}");
    let (_, body) = call(&app, OWNER, "GET", "/v1/zones", None).await;
    assert!(body.contains(&closed), "the owner sees every zone");

    // A transfer from an open zone to a closed one
    let (st, body) = call(
        &app,
        &listener,
        "POST",
        &format!("/v1/zones/{open}/transfer"),
        Some(serde_json::json!({"to": closed})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");

    // The event socket: the snapshot has the open zone only; an event about the closed zone
    // arrives as `hidden`, keeping `seq` without gaps
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(pm_api::serve_listener(s.clone(), l));
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/events?token={listener}"))
        .await
        .unwrap();
    let mut next = async || -> serde_json::Value {
        loop {
            let m = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
                .await
                .expect("an event")
                .unwrap()
                .unwrap();
            if let tokio_tungstenite::tungstenite::Message::Text(t) = m {
                return serde_json::from_str(&t).unwrap();
            }
        }
    };
    let snap = next().await;
    assert_eq!(snap["type"], "snapshot");
    let zones = snap["zones"].as_array().unwrap();
    assert_eq!(zones.len(), 1, "{snap}");
    assert_eq!(zones[0]["id"], open.as_str());
    let seq = snap["seq"].as_u64().unwrap();
    s.events.send(pm_api::types::ApiEvent::ZoneRemoved {
        zone: pm_zones::ZoneId(closed.clone()),
    });
    s.events.send(pm_api::types::ApiEvent::ZoneRemoved {
        zone: pm_zones::ZoneId(open.clone()),
    });
    let mut got = Vec::new();
    loop {
        let ev = next().await;
        got.push(ev.clone());
        if ev["type"] == "zone_removed" {
            break;
        }
    }
    let text = serde_json::to_string(&got).unwrap();
    assert!(!text.contains(&closed), "nothing about the closed zone: {text}");
    assert!(got.iter().any(|e| e["type"] == "hidden"), "{text}");
    assert_eq!(got.last().unwrap()["zone"], open.as_str());
    for (i, e) in got.iter().enumerate() {
        assert_eq!(e["seq"].as_u64().unwrap(), seq + 1 + i as u64, "no gaps: {text}");
    }
}
