//! A person's devices and what they keep offline (STEWARD-DOWNLOADS.md): registering, the
//! manifest's `wanted` and `unkept`, kept albums, playlists and favourites getting their new
//! songs, and each person seeing only their own devices.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OWNER: &str = "owner-token";
const KEY: &str = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";

fn node(dir: &std::path::Path) -> Arc<NodeState> {
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

/// A track on album `album` (by title), `bytes` big.
fn add(s: &NodeState, title: &str, album: &str, n: u32, bytes: u64) -> i64 {
    let t: pm_model::Track = serde_json::from_value(json!({
        "id": null, "title": title, "artists": [{"id": null, "name": "Artist", "mbid": null}],
        "album": {"id": null, "title": album, "album_artist": "Artist", "release_mbid": null,
                  "release_group_mbid": null, "year": null, "upc": null, "total_discs": null},
        "track_no": n, "disc_no": null, "duration_ms": 1000,
        "file": {"path": format!("/music/{album}/{n:02} {title}.flac"), "size_bytes": bytes, "mtime_unix": 1,
                 "container": "flac", "format": serde_json::to_value(pm_model::AudioFormat::pcm(44_100, 16, 2)).unwrap(),
                 "duration_ms": 1000},
        "recording_mbid": null, "isrc": null, "acoustid": null, "canonical": null,
        "replaygain_track_gain_db": null, "replaygain_album_gain_db": null}))
    .unwrap();
    s.library.lock().unwrap().upsert_track(&t).unwrap().0
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
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn ids(v: &Value) -> Vec<i64> {
    let mut out: Vec<i64> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["track_id"].as_i64().or(x.as_i64()).unwrap())
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn what_a_device_keeps_and_what_it_still_wants() {
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let app = router(s.clone());
    let a1 = add(&s, "One", "Album A", 1, 1000);
    let a2 = add(&s, "Two", "Album A", 2, 2000);
    let single = add(&s, "Single", "Single", 1, 500);
    let album_a = s
        .library
        .lock()
        .unwrap()
        .get_track(pm_model::TrackId(a1))
        .unwrap()
        .unwrap()
        .album
        .unwrap()
        .id
        .unwrap();

    // Register the phone
    let phone = "2f6c1a9e-7d4b-4c55-9a1e-8b2c3d4e5f60";
    let (st, d) = call(
        &app,
        OWNER,
        "PUT",
        &format!("/v1/devices/{phone}"),
        Some(json!({"name": "Pixel 8", "kind": "android"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{d}");
    assert_eq!(
        (d["name"].as_str(), d["files"].as_u64()),
        (Some("Pixel 8"), Some(0))
    );

    // It keeps Album A and the single: it wants all three
    let uri = format!("/v1/devices/{phone}/downloads");
    let (st, m) = call(
        &app,
        OWNER,
        "PUT",
        &uri,
        Some(json!({"keep": {"albums": [album_a], "tracks": [single]}})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    assert_eq!(ids(&m["wanted"]), [a1, a2, single]);
    assert_eq!(
        m["wanted"][0]["bytes"].as_u64().unwrap()
            + m["wanted"][1]["bytes"].as_u64().unwrap()
            + m["wanted"][2]["bytes"].as_u64().unwrap(),
        3500
    );

    // It downloaded two (one as AAC), and still has an old song nothing keeps
    let old = add(&s, "Old", "Gone", 1, 10);
    let (_, m) = call(&app, OWNER, "PUT", &uri, Some(json!({"have": [
        {"track_id": a1, "bytes": 1000}, {"track_id": single, "bytes": 300, "format": "aac"}, {"track_id": old, "bytes": 10}]}))).await;
    assert_eq!(ids(&m["wanted"]), [a2]);
    assert_eq!(m["unkept"], json!([old]));
    assert_eq!(m["have"][1]["format"], "aac");
    assert_eq!(m["have"][0]["format"], "original");

    // A new song on the kept album: wanted next time, without the device asking for it
    let a3 = add(&s, "Three", "Album A", 3, 3000);
    let (_, m) = call(&app, OWNER, "GET", &uri, None).await;
    assert_eq!(ids(&m["wanted"]), [a2, a3]);

    // Keep a playlist and the favourites: their songs come along, and new ones too
    let p1 = add(&s, "Listed", "Mix", 1, 1);
    let (st, pl) = call(
        &app,
        OWNER,
        "POST",
        "/v1/playlists",
        Some(json!({"name": "Road trip", "track_ids": [p1]})),
    )
    .await;
    assert!(st.is_success(), "{pl}");
    let pid = pl["summary"]["id"].as_str().unwrap().to_string();
    let fav = add(&s, "Loved", "Hearts", 1, 1);
    let (st, f) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/favourites",
        Some(json!({"track_id": fav})),
    )
    .await;
    assert!(st.is_success(), "{f}");
    let (_, m) = call(&app, OWNER, "PUT", &uri, Some(json!({"keep": {"albums": [album_a], "tracks": [single], "playlists": [pid], "favourites": true}}))).await;
    assert_eq!(ids(&m["wanted"]), [a2, a3, p1, fav]);
    // A song removed from the library drops out of what it wants
    assert!(s
        .library
        .lock()
        .unwrap()
        .remove_track(pm_model::TrackId(a3))
        .unwrap());
    let (_, m) = call(&app, OWNER, "GET", &uri, None).await;
    assert_eq!(ids(&m["wanted"]), [a2, p1, fav]);

    // The list shows its storage
    let (_, list) = call(&app, OWNER, "GET", "/v1/devices", None).await;
    assert_eq!(list[0]["bytes"], 1310);
    assert_eq!(list[0]["files"], 3);

    // A listener: their own devices only, songs and albums only
    let roster = json!({"people": [{"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster.to_string())
        .unwrap();
    let listener = pm_api::accounts::mint(&s, KEY).unwrap();
    assert_eq!(
        call(&app, &listener, "GET", "/v1/devices", None).await.1,
        json!([])
    );
    assert_eq!(
        call(&app, &listener, "GET", &uri, None).await.0,
        StatusCode::NOT_FOUND,
        "someone else's device"
    );
    assert_eq!(
        call(
            &app,
            &listener,
            "PUT",
            &format!("/v1/devices/{phone}"),
            Some(json!({"name": "Mine now"}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let theirs = "listener-laptop-01";
    assert_eq!(
        call(
            &app,
            &listener,
            "PUT",
            &format!("/v1/devices/{theirs}"),
            Some(json!({"name": "Laptop", "kind": "desktop"}))
        )
        .await
        .0,
        StatusCode::OK
    );
    let (st, m) = call(
        &app,
        &listener,
        "PUT",
        &format!("/v1/devices/{theirs}/downloads"),
        Some(json!({"keep": {"albums": [album_a]}})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    assert_eq!(ids(&m["wanted"]), [a1, a2]);
    let (st, _) = call(
        &app,
        &listener,
        "PUT",
        &format!("/v1/devices/{theirs}/downloads"),
        Some(json!({"keep": {"favourites": true}})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // The owner doesn't see the listener's laptop, and the listener's list has only it
    let (_, owner_list) = call(&app, OWNER, "GET", "/v1/devices", None).await;
    assert_eq!(owner_list.as_array().unwrap().len(), 1);
    let (_, their_list) = call(&app, &listener, "GET", "/v1/devices", None).await;
    assert_eq!(their_list[0]["id"], theirs);

    // A listener can't change or forget the owner's device
    assert_eq!(
        call(&app, &listener, "DELETE", &format!("/v1/devices/{phone}"), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            &listener,
            "PUT",
            &uri,
            Some(json!({"keep": {"tracks": [a1]}}))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Duplicates count once, and huge or odd reports are refused (the Claude review of #99)
    let dup_albums: Vec<i64> = std::iter::repeat_n(album_a, 5_000).collect();
    let (st, m) = call(
        &app,
        &listener,
        "PUT",
        &format!("/v1/devices/{theirs}/downloads"),
        Some(json!({"keep": {"albums": dup_albums}})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    assert_eq!(m["keep"]["albums"], json!([album_a]));
    let too_many: Vec<i64> = (1..=2_001).collect();
    assert_eq!(
        call(
            &app,
            &listener,
            "PUT",
            &format!("/v1/devices/{theirs}/downloads"),
            Some(json!({"keep": {"albums": too_many}}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            &listener,
            "PUT",
            &format!("/v1/devices/{theirs}/downloads"),
            Some(json!({"have": [{"track_id": a1, "bytes": u64::MAX}]}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            &listener,
            "PUT",
            &format!("/v1/devices/{theirs}/downloads"),
            Some(json!({"have": [{"track_id": a1, "bytes": 1, "format": "wav"}]}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // A deleted playlist keeps nothing
    let (st, _) = call(&app, OWNER, "DELETE", &format!("/v1/playlists/{pid}"), None).await;
    assert!(st.is_success());
    let (_, m) = call(&app, OWNER, "GET", &uri, None).await;
    assert!(!ids(&m["wanted"]).contains(&p1), "{m}");
    // The listener's music access goes: so do their devices
    let (st, _) = call(
        &app,
        OWNER,
        "PATCH",
        &format!("/v1/people/{KEY}"),
        Some(json!({"music": false})),
    )
    .await;
    assert!(st.is_success());
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster.to_string())
        .unwrap();
    assert_eq!(
        call(&app, &listener, "GET", "/v1/devices", None).await.1,
        json!([])
    );

    // Bad input, and forgetting
    assert_eq!(
        call(
            &app,
            OWNER,
            "PUT",
            "/v1/devices/short",
            Some(json!({"name": "x"}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            OWNER,
            "PUT",
            "/v1/devices/valid-id-but-bad-kind",
            Some(json!({"name": "x", "kind": "toaster"}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, OWNER, "DELETE", &format!("/v1/devices/{phone}"), None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, OWNER, "GET", &uri, None).await.0,
        StatusCode::NOT_FOUND
    );
}
