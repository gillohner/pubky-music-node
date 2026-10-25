//! What a listener's personal token may read and do (accounts.rs): the shared library and
//! their zones, never the owner's favourites, playlists, history or accounts.

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
async fn a_listener_reads_the_library_but_not_the_owners_things() {
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
    // A person with music access, signed in on one device
    let roster = serde_json::json!({"people": [
        {"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster.to_string())
        .unwrap();
    assert!(
        pm_api::people::may_use_music(&s, KEY),
        "the roster's shape changed"
    );
    let listener = pm_api::accounts::mint(&s, KEY).unwrap();
    let app = router(s.clone());

    // A track in the owner's folder: listeners get its file name, not where the owner keeps it
    let file = dir.path().join("Owner's music").join("A.flac");
    let track: pm_model::Track = serde_json::from_value(serde_json::json!({
        "id": null, "title": "A", "artists": [{"id": null, "name": "X", "mbid": null}], "album": null,
        "track_no": 1, "disc_no": null, "duration_ms": 1000,
        "file": {"path": file, "size_bytes": 1, "mtime_unix": 0, "container": "flac",
                 "format": serde_json::to_value(pm_model::AudioFormat::pcm(44_100, 16, 2)).unwrap(), "duration_ms": 1000},
        "recording_mbid": null, "isrc": null, "acoustid": null, "canonical": null,
        "replaygain_track_gain_db": null, "replaygain_album_gain_db": null}))
    .unwrap();
    let id = s.library.lock().unwrap().upsert_track(&track).unwrap().0;
    for uri in [
        "/v1/library/tracks".to_string(),
        format!("/v1/library/tracks/{id}"),
        // The song page: its About section has a "File" fact
        format!("/v1/item/song/lib-{id}"),
    ] {
        let (st, body) = call(&app, &listener, "GET", &uri, None).await;
        assert_eq!(st, StatusCode::OK, "{uri}: {body}");
        if uri.starts_with("/v1/item/") {
            // The About section's File fact shows the file name only
            let page: serde_json::Value = serde_json::from_str(&body).unwrap();
            let file = page["about"]
                .as_array()
                .and_then(|a| a.iter().find(|f| f["label"] == "File"))
                .unwrap_or_else(|| panic!("no File fact: {body}"));
            assert_eq!(file["value"], "A.flac", "{body}");
        } else {
            assert!(body.contains("\"path\":\"A.flac\""), "{uri}: {body}");
        }
        assert!(!body.contains("Owner's music"), "{uri}: {body}");
        let (_, owner) = call(&app, OWNER, "GET", &uri, None).await;
        assert!(
            owner.contains("Owner's music"),
            "the owner still sees the folder: {owner}"
        );
    }

    let (st, me) = call(&app, &listener, "GET", "/v1/me", None).await;
    assert_eq!(st, StatusCode::OK, "{me}");
    assert!(me.contains("\"listener\""), "{me}");
    for open in [
        "/v1/library/albums",
        "/v1/library/artists",
        "/v1/library/tracks",
        "/v1/zones",
        "/v1/radio/tags",
    ] {
        let (st, body) = call(&app, &listener, "GET", open, None).await;
        assert_ne!(st, StatusCode::FORBIDDEN, "{open}: {body}");
        assert_ne!(st, StatusCode::UNAUTHORIZED, "{open}: {body}");
    }
    for owners in [
        "/v1/favourites",
        "/v1/activity",
        "/v1/playlists",
        "/v1/tags",
        "/v1/reviews",
        "/v1/library/history",
        "/v1/library/home",
        "/v1/sources",
        "/v1/sources/tidal/collection",
        "/v1/podcasts/subscriptions",
        "/v1/people",
        "/v1/library/tracks/1/candidates",
    ] {
        let (st, body) = call(&app, &listener, "GET", owners, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{owners}: {body}");
        assert!(body.contains("Not for listeners"), "{owners}: {body}");
        // The owner still reads them (or gets the route's own answer, never a refusal)
        let (st, _) = call(&app, OWNER, "GET", owners, None).await;
        assert_ne!(st, StatusCode::FORBIDDEN, "owner {owners}");
    }

    // Zones: with a list set, only those zones, for reads too
    let (st, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/people/zones",
        Some(serde_json::json!({"zones": ["kitchen"]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = call(&app, &listener, "GET", "/v1/zones/office", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("not opened that zone"), "{body}");
    let (st, _) = call(&app, &listener, "GET", "/v1/zones/kitchen", None).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "allowed, and there is no such zone here"
    );
    let (st, _) = call(
        &app,
        &listener,
        "POST",
        "/v1/zones/office/transport",
        Some(serde_json::json!({"action": "play"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Writes outside their zones are refused
    let (st, _) = call(
        &app,
        &listener,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({"name": "x"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}
