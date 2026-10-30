//! Activity recording and playlist import resolve described tracks through
//! the library and refuse anything without a canonical identity.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use id3::TagLike;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use tower::ServiceExt;

const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";

fn write_wav(path: &std::path::Path, title: &str, artist: &str, mbid: Option<&str>) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..441 {
        w.write_sample((i % 50) as i16).unwrap();
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    t.set_title(title);
    t.set_artist(artist);
    t.set_album("Imports");
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
async fn record_and_import_resolve_through_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("a.wav"), "Get Lucky", "Daft Punk", Some(MB));
    write_wav(&music.join("b.wav"), "Mystery Song", "Nobody", None);
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![music.clone()],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
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
    call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    for _ in 0..500 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // A Roon-style scrobble with only title and artist resolves to the tagged track.
    let (st, rec) = call(
        &app,
        "POST",
        "/v1/activity",
        Some(serde_json::json!({"track": {"title": "get lucky", "artist": "Daft Punk"}, "source": "roon", "duration_ms": 369000})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rec}");
    assert_eq!(rec["uri"], format!("https://musicbrainz.org/recording/{MB}"));
    assert!(rec["track_id"].is_i64());
    // An untagged track cannot be recorded; a canonical URL alone can.
    let (st, err) = call(
        &app,
        "POST",
        "/v1/activity",
        Some(serde_json::json!({"track": {"title": "Mystery Song", "artist": "Nobody"}, "source": "roon"})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    let (st, rec2) = call(
        &app,
        "POST",
        "/v1/activity",
        Some(serde_json::json!({"track": {"title": "Elsewhere", "uri": "https://open.spotify.com/track/2Foc5Q5nqNiosCNqttzHof"}, "source": "spotify"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rec2}");
    assert!(rec2["track_id"].is_null());
    let (st, list) = call(&app, "GET", "/v1/activity?limit=10", None).await;
    assert_eq!(st, StatusCode::OK);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert!(list
        .iter()
        .any(|e| e["source"]["kind"] == "roon" && e["track"]["title"] == "Get Lucky"));
    assert!(list
        .iter()
        .any(|e| e["source"]["kind"] == "service" && e["source"]["name"] == "spotify"));

    // Import (TASK-pinakes-social-ux.md B4): one resolves in the library, one has a URL, one an
    // ISRC, one a MusicBrainz id, one a service link; the lost one stays, as a MusicBrainz search.
    let (st, imp) = call(
        &app,
        "POST",
        "/v1/playlists/import",
        Some(serde_json::json!({
            "name": "From Roon",
            "tracks": [
                {"title": "Get Lucky", "artist": "Daft Punk", "album": "Random Access Memories"},
                {"title": "Elsewhere", "uri": "https://musicbrainz.org/isrc/USQX91300108"},
                {"title": "Mystery Song", "artist": "Nobody"},
                {"title": "By ISRC", "isrc": "gbayE9701376"},
                {"title": "By MBID", "mbid": "833f00e1-781f-4edd-90e4-e52712618862"},
                {"title": "On TIDAL", "uri": "https://tidal.com/browse/track/50850120"}
            ]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{imp}");
    assert_eq!(
        (imp["imported"].as_u64(), imp["kept"].as_u64()),
        (Some(6), Some(1)),
        "{imp}"
    );
    assert_eq!(imp["unresolved"].as_array().unwrap().len(), 1);
    assert_eq!(imp["unresolved"][0]["title"], "Mystery Song");
    let (_, mixed) = call(
        &app,
        "GET",
        &format!("/v1/collections/{}", imp["id"].as_str().unwrap()),
        None,
    )
    .await;
    let uris: Vec<&str> = mixed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["uri"].as_str().unwrap())
        .collect();
    assert_eq!(
        &uris[1..],
        [
            "https://musicbrainz.org/isrc/USQX91300108",
            "https://musicbrainz.org/search?type=recording&query=Nobody+Mystery+Song",
            "https://musicbrainz.org/isrc/GBAYE9701376",
            "https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862",
            "https://tidal.com/track/50850120",
        ],
        "{mixed}"
    );
    assert_eq!(
        mixed["entries"][2]["kind"], "link",
        "the lost line is a link, in its place"
    );
    let id = imp["id"].as_str().unwrap();
    let (st, pl) = call(&app, "GET", &format!("/v1/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(pl["summary"]["name"], "From Roon");
    assert_eq!(pl["tracks"][0]["title"], "Get Lucky");
    assert!(pl["tracks"][1].is_null());
    // A line with nothing to keep (no words, no address) is left out; only such lines is a 422
    let (st, _) = call(
        &app,
        "POST",
        "/v1/playlists/import",
        Some(serde_json::json!({"name": "x", "tracks": [{"title": "  "}]})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // A playlist file: the exact path, the same file under another mount (matched by file name),
    // a title-only entry, a miss (kept as a search), a stream on Spotify, and a web page (kept)
    let a = music.join("a.wav");
    let m3u = format!(
        "#EXTM3U\n#PLAYLIST:From elsewhere\n{}\n/mnt/old-nas/Daft Punk/a.wav\n#EXTINF:369,Daft Punk - Get Lucky\n/nowhere/x.flac\n#EXTINF:1,Nobody - Missing\n/nowhere/missing.flac\nhttps://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC\nhttps://example.com/mixtape-notes\n",
        a.display()
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/playlists/import/file?file=mix.m3u8")
        .header("content-type", "application/octet-stream")
        .body(Body::from(m3u))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let imp: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(st, StatusCode::OK, "{imp}");
    assert_eq!(imp["name"], "From elsewhere");
    assert_eq!(imp["parsed"], 6);
    assert_eq!(
        (imp["imported"].as_u64(), imp["kept"].as_u64()),
        (Some(6), Some(2)),
        "{imp}"
    );
    assert_eq!(imp["unresolved"][0]["title"], "Missing");
    assert_eq!(imp["unresolved"][0]["path"], "/nowhere/missing.flac");
    let (_, file) = call(
        &app,
        "GET",
        &format!("/v1/collections/{}", imp["id"].as_str().unwrap()),
        None,
    )
    .await;
    let kinds: Vec<&str> = file["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["track", "track", "track", "link", "track", "link"],
        "{file}"
    );
    assert_eq!(
        file["entries"][4]["uri"],
        "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"
    );
    assert_eq!(file["entries"][5]["uri"], "https://example.com/mixtape-notes");
    // Explicit name wins; an unknown format is a 400; garbage CSV is a 422.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/playlists/import/file?name=Named&format=csv")
        .body(Body::from("Title,Artist\nGet Lucky,Daft Punk\n"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let imp: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(imp["name"], "Named");
    assert_eq!(imp["imported"], 1);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/playlists/import/file?format=doc")
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/playlists/import/file?file=x.csv")
        .body(Body::from("a,b\n1,2\n"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // A media server's playlist (review of #156): streams with their sign-in in them, on the
    // LAN, or longer than a v1 reference are never kept; the lines fall back to their words,
    // and the import still works
    let m3u = format!(
        "#EXTM3U\n#EXTINF:1,Nobody - Token Song\nhttp://media.example.com/Audio/x/stream.mp3?api_key=hunter2\n#EXTINF:1,Nobody - Password Song\nhttps://admin:hunter2@stream.example.com/live.mp3\n#EXTINF:1,Nobody - Nas Song\nhttp://nas:8096/a.mp3\n#EXTINF:1,Nobody - Long Song\nhttps://cdn.example.com/{}.mp3\n",
        "a".repeat(3000)
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/playlists/import/file?file=server.m3u")
        .body(Body::from(m3u))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let imp: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(st, StatusCode::OK, "{imp}");
    assert_eq!(imp["kept"], 4, "{imp}");
    let (_, server) = call(
        &app,
        "GET",
        &format!("/v1/collections/{}", imp["id"].as_str().unwrap()),
        None,
    )
    .await;
    let uris: Vec<&str> = server["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["uri"].as_str().unwrap())
        .collect();
    assert_eq!(
        uris,
        [
            "https://musicbrainz.org/search?type=recording&query=Nobody+Token+Song",
            "https://musicbrainz.org/search?type=recording&query=Nobody+Password+Song",
            "https://musicbrainz.org/search?type=recording&query=Nobody+Nas+Song",
            "https://musicbrainz.org/search?type=recording&query=Nobody+Long+Song",
        ],
        "{server}"
    );
    let (_, lists) = call(&app, "GET", "/v1/playlists", None).await;
    assert!(lists
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "From elsewhere" && p["items"] == 6));
}
