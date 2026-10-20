//! Playlist CRUD over the router: items by canonical URL and by tagged
//! library track, resolution back to tracks, play into a zone, local sync
//! bookkeeping (no homeserver here).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use id3::TagLike;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::{OutputRegistry, ZoneId};
use std::sync::Arc;
use tower::ServiceExt;

const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
const MB2: &str = "833f00e1-781f-4edd-90e4-e52712618862";

fn write_wav(path: &std::path::Path, title: &str, mbid: Option<&str>) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..2205 {
        w.write_sample((i % 50) as i16).unwrap();
        w.write_sample(-((i % 50) as i16)).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    t.set_title(title);
    t.set_artist("Tester");
    t.set_album("Lists");
    if let Some(m) = mbid {
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Track Id".into(),
            value: m.into(),
        });
    }
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
        .header("content-type", "application/json")
        .body(match body {
            Some(b) => Body::from(b.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn playlist_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("tagged.wav"), "Tagged", Some(MB));
    write_wav(&music.join("untagged.wav"), "Untagged", None);
    let db = dir.path().join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![music.clone()],
            pipeline: PipelineConfig::default(),
            token: None,
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
    let tracks = found["tracks"].as_array().unwrap();
    let tagged = tracks.iter().find(|t| t["title"] == "Tagged").unwrap()["id"]
        .as_i64()
        .unwrap();
    let untagged = tracks.iter().find(|t| t["title"] == "Untagged").unwrap()["id"]
        .as_i64()
        .unwrap();

    // Create from a raw canonical URL plus a tagged track.
    let (st, pl) = call(
        &app,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({
            "name": "  Late night ",
            "description": "quiet",
            "uris": [format!("https://musicbrainz.org/recording/{MB2}")],
            "track_ids": [tagged]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{pl}");
    let id = pl["summary"]["id"].as_str().unwrap().to_string();
    assert_eq!(pl["summary"]["name"], "Late night");
    assert_eq!(pl["summary"]["visibility"], "private");
    assert_eq!(pl["summary"]["synced"], false);
    assert_eq!(pl["playlist"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        pl["playlist"]["items"][1]["uri"],
        format!("https://musicbrainz.org/recording/{MB}")
    );
    // Item 0 is not in the library, item 1 resolves to the tagged track.
    assert!(pl["tracks"][0].is_null());
    assert_eq!(pl["tracks"][1]["id"], tagged);

    // An untagged track cannot enter social data.
    let (st, err) = call(
        &app,
        "POST",
        &format!("/v1/playlists/{id}/items"),
        Some(serde_json::json!({"track_ids": [untagged]})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"].as_str().unwrap().contains("canonical identity"));
    // Bad URIs are rejected; `music:` especially.
    let (st, _) = call(
        &app,
        "POST",
        &format!("/v1/playlists/{id}/items"),
        Some(serde_json::json!({"uris": ["music:recording:x"]})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Insert at the front, then remove it again.
    let (st, pl) = call(
        &app,
        "POST",
        &format!("/v1/playlists/{id}/items"),
        Some(serde_json::json!({"uris": ["https://open.spotify.com/track/2Foc5Q5nqNiosCNqttzHof"], "at": 0})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{pl}");
    assert_eq!(pl["playlist"]["items"].as_array().unwrap().len(), 3);
    assert!(pl["playlist"]["items"][0]["uri"]
        .as_str()
        .unwrap()
        .contains("spotify"));
    let (st, pl) = call(&app, "DELETE", &format!("/v1/playlists/{id}/items/0"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(pl["playlist"]["items"].as_array().unwrap().len(), 2);

    // Rename keeps the id; the stored post is a valid v1 post envelope.
    let (st, pl) = call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}"),
        Some(serde_json::json!({"name": "Night"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(pl["summary"]["id"], id);
    assert_eq!(pl["summary"]["name"], "Night");
    {
        let lib = s.library.lock().unwrap();
        let row = lib.get_music_post(&id).unwrap().unwrap();
        assert_eq!(row.kind, "pubky.music/playlist");
        let post: pubky_app_specs::PubkyAppPost = serde_json::from_str(&row.json).unwrap();
        let music = pm_pubky::MusicPost::from_post(post, Some(&id)).unwrap().unwrap();
        assert_eq!(music.as_playlist().unwrap().name, "Night");
        assert_eq!(lib.unsynced_music_posts().unwrap().len(), 1);
    }

    // Play into the zone: the tagged track queues, the other item is reported missing.
    let (st, played) = call(
        &app,
        "POST",
        &format!("/v1/playlists/{id}/play"),
        Some(serde_json::json!({"zone": "null-output"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{played}");
    assert_eq!(played["queued"], 1);
    assert_eq!(played["missing"].as_array().unwrap().len(), 1);
    let zone = s.zones.get(&ZoneId::new("null-output")).unwrap();
    let st = zone.wait_stopped().await;
    assert_eq!(st.error, None);

    // Listing, status, delete.
    let (_, list) = call(&app, "GET", "/v1/playlists", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (st, status) = call(&app, "GET", "/v1/pubky", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(status["user"].is_null());
    assert_eq!(status["unsynced"], 1);
    let (st, _) = call(&app, "POST", "/v1/pubky/sync", None).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    let (st, _) = call(&app, "DELETE", &format!("/v1/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(&app, "GET", &format!("/v1/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (_, list) = call(&app, "GET", "/v1/playlists", None).await;
    assert!(list.as_array().unwrap().is_empty());
}
