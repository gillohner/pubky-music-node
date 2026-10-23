//! `PUT /v1/zones/{id}/crossfade`: the zone's crossfade, kept across restarts; owner only.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OWNER: &str = "owner";
const KEY: &str = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";

fn node(dir: &std::path::Path) -> Arc<NodeState> {
    let db = dir.join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
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
    s
}

async fn call(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_zones_crossfade_is_set_kept_and_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let zone = {
        let s = node(dir.path());
        let app = router(s.clone());
        let (_, zones) = call(&app, OWNER, "GET", "/v1/zones", None).await;
        let zone = zones[0]["id"].as_str().unwrap().to_string();
        assert_eq!(zones[0]["crossfade_ms"], 0, "off by default");
        let path = format!("/v1/zones/{zone}/crossfade");
        let (st, _) = call(&app, OWNER, "PUT", &path, Some(json!({"seconds": 4.5}))).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, z) = call(&app, OWNER, "GET", &format!("/v1/zones/{zone}"), None).await;
        assert_eq!(z["crossfade_ms"], 4500, "{z}");
        let (st, _) = call(&app, OWNER, "PUT", &path, Some(json!({"seconds": 30}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // A listener plays on the zone but doesn't tune it
        let roster =
            json!({"people": [{"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
        s.library
            .lock()
            .unwrap()
            .set_setting("people", &roster.to_string())
            .unwrap();
        let listener = pm_api::accounts::mint(&s, KEY).unwrap();
        let (st, _) = call(&app, &listener, "PUT", &path, Some(json!({"seconds": 0}))).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        zone
    };
    // Kept across a restart
    let s = node(dir.path());
    assert_eq!(
        s.zones.get(&pm_zones::ZoneId::new(&zone)).unwrap().crossfade_ms(),
        4500
    );
}
