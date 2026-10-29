//! YouTube search and video pages without an API key (TASK-service-apps-builtin.md): through
//! yt-dlp once the owner switched it on. A stand-in yt-dlp answers here; its own process, so the
//! global yt-dlp path can't reach other tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use serde_json::{json, Value};
use tower::ServiceExt;

const OWNER: &str = "owner-token";

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
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn node(dir: &std::path::Path) -> std::sync::Arc<NodeState> {
    let db = dir.join("lib.sqlite");
    NodeState::new(
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
        OutputRegistry::new(),
    )
}

#[tokio::test]
async fn youtube_searches_through_yt_dlp_without_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let app = router(s.clone());
    // No key and yt-dlp off: said in plain words
    let (st, e) = call(
        &app,
        OWNER,
        "GET",
        "/v1/sources/youtube/search?q=get%20lucky",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED, "{e}");
    assert!(e["error"].as_str().unwrap().contains("yt-dlp"), "{e}");
    // A stand-in yt-dlp: its version, a search answer, a video's facts; it logs its arguments
    let bin = dir.path().join("yt-dlp");
    let log = dir.path().join("args");
    std::fs::write(
        &bin,
        format!(
            r#"#!/bin/sh
echo "$@" >> '{}'
if [ "$1" = "--version" ]; then echo 2026.10.01; exit 0; fi
for a; do last="$a"; done
case "$last" in
  ytsearch*) echo '{{"entries": [{{"id": "5NV6Rdv1a3I", "title": "Get Lucky", "channel": "Daft Punk - Topic", "duration": 248.0, "thumbnails": [{{"url": "https://i.ytimg.com/vi/5NV6Rdv1a3I/hq.jpg"}}]}}, {{"id": "bad id!", "title": "Junk"}}]}}' ;;
  *watch?v=5NV6Rdv1a3I) echo '{{"title": "Get Lucky", "channel": "Daft Punk - Topic", "upload_date": "20130517", "duration": 248, "thumbnail": "https://i.ytimg.com/vi/5NV6Rdv1a3I/maxres.jpg"}}' ;;
  *) echo 'ERROR: Video unavailable' >&2; exit 1 ;;
esac
"#,
            log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    pm_api::ytdlp::set_config_path(bin.to_str().unwrap());
    let (st, m) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/sources/youtube/unofficial",
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    let (st, hits) = call(
        &app,
        OWNER,
        "GET",
        "/v1/sources/youtube/search?q=get%20lucky&limit=5",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{hits}");
    let list = hits["items"].as_array().unwrap();
    assert_eq!(list.len(), 1, "a bad id is left out: {hits}");
    assert_eq!(list[0]["id"], "video:5NV6Rdv1a3I");
    assert_eq!(list[0]["creator"], "Daft Punk");
    assert_eq!(list[0]["duration_ms"], 248_000);
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(
        args.contains("--ignore-config") && args.contains("ytsearch5:get lucky"),
        "{args}"
    );
    // A video's page
    let (st, page) = call(
        &app,
        OWNER,
        "GET",
        "/v1/sources/youtube/items/video:5NV6Rdv1a3I",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(
        (page["title"].as_str(), page["year"].as_i64()),
        (Some("Get Lucky"), Some(2013))
    );
    assert_eq!(page["tracks"][0]["duration_ms"], 248_000);
    let (st, _) = call(
        &app,
        OWNER,
        "GET",
        "/v1/sources/youtube/items/video:aaaaaaaaaaa",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
}
