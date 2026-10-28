//! A listener's `/v1/info` has no scan error paths (where the owner keeps music), and a listener
//! signed in with `?token=` signs out with it too (security review 2026-10-27, lows).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use tower::ServiceExt;

const OWNER: &str = "owner-token";
const KEY: &str = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";

async fn call(app: &axum::Router, auth: Option<&str>, method: &str, uri: &str) -> (StatusCode, String) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = auth {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let resp = app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn listeners_get_no_owner_paths_and_sign_out_by_query_token() {
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
    let roster = serde_json::json!({"people": [
        {"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster.to_string())
        .unwrap();
    *s.last_scan.lock().unwrap() = Some(pm_api::types::LastScan {
        finished_at: 1,
        added: 0,
        updated: 0,
        removed: 0,
        error_count: 1,
        errors: vec![("/home/owner/Private Music/x.flac".into(), "bad header".into())],
    });
    let app = router(s.clone());

    // /v1/info: the count for a listener, the paths for the owner
    let listener = pm_api::accounts::mint(&s, KEY).unwrap();
    let (st, body) = call(&app, Some(&listener), "GET", "/v1/info").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(!body.contains("Private Music"), "{body}");
    assert!(body.contains("\"error_count\":1"), "{body}");
    let (_, body) = call(&app, Some(OWNER), "GET", "/v1/info").await;
    assert!(body.contains("Private Music"), "{body}");

    // Signed in with ?token= (the WebSocket way), signed out the same way
    let (st, body) = call(&app, None, "DELETE", &format!("/v1/me/token?token={listener}")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, _) = call(&app, Some(&listener), "GET", "/v1/info").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "the token no longer works");
}
