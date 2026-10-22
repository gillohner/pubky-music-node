//! Artist credits split, and the writer that puts what the node knows into the files
//! (owner, 2026-10-22: "all artists are wrong and not separated … a writer that fixes this").

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use id3::TagLike;
use pm_api::{router, NodeSettings, NodeState};
use pm_library::Library;
use pm_zones::OutputRegistry;
use std::sync::Arc;
use tower::ServiceExt;

fn wav(path: &std::path::Path, title: &str, artist: &str) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..4410 {
        w.write_sample((i % 50) as i16).unwrap();
        w.write_sample(-((i % 50) as i16)).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    t.set_title(title);
    t.set_artist(artist);
    t.set_album("Frenchcore Worldwide 02");
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
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        st,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn credits_split_and_the_writer_fixes_the_files() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    wav(
        &music.join("01 Flowing River.wav"),
        "Flowing River",
        "Sefa, Dr. Peacock",
    );
    wav(&music.join("02 Solo.wav"), "Solo", "Dr. Peacock");
    wav(&music.join("03 Duo.wav"), "Duo", "Sefa & Cyclon");
    let db = dir.path().join("lib.db");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![music.clone()],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("hs"),
            pipeline: Default::default(),
            token: None,
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
    let app = router(s.clone());
    let scan = |s: Arc<NodeState>| async move {
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if !s.is_scanning() {
                break;
            }
        }
    };
    call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    scan(s.clone()).await;
    let (_, artists) = call(&app, "GET", "/v1/library/artists", None).await;
    let mut names: Vec<String> = artists
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Cyclon", "Dr. Peacock", "Sefa"], "each artist once");

    let (st, fixes) = call(&app, "GET", "/v1/library/tag-fixes", None).await;
    assert_eq!(st, StatusCode::OK, "{fixes}");
    assert_eq!(fixes["total"], 2, "{fixes}");
    assert!(fixes.to_string().contains("ARTISTS"));
    let (st, w) = call(&app, "POST", "/v1/library/tag-fixes", Some(serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{w}");
    assert_eq!(w["written"], 2, "{w}");
    let t = id3::Tag::read_from_path(music.join("01 Flowing River.wav")).unwrap();
    assert_eq!(t.artist(), Some("Sefa, Dr. Peacock"), "the display credit stays");
    let v = t
        .extended_texts()
        .find(|x| x.description == "ARTISTS")
        .unwrap()
        .value
        .clone();
    assert_eq!(v, "Sefa\0Dr. Peacock");
    let (_, again) = call(&app, "GET", "/v1/library/tag-fixes", None).await;
    assert_eq!(again["total"], 0, "nothing left: {again}");
}
