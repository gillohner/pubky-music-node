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
    t.set_genre("Disco; Funk");
    if let Some(m) = mbid {
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Track Id".into(),
            value: m.into(),
        });
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Album Id".into(),
            value: MB2.into(),
        });
        t.add_frame(id3::frame::Picture {
            mime_type: "image/png".into(),
            picture_type: id3::frame::PictureType::CoverFront,
            description: String::new(),
            data: b"\x89PNG-not-really".to_vec(),
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

    // The album got its cover from the tagged file's embedded picture. (The
    // untagged file has no release id, so it forms a second "Lists" album.)
    let (_, albums) = call(&app, "GET", "/v1/library/albums", None).await;
    let albums = albums.as_array().unwrap().clone();
    let album_id = albums.iter().find(|a| a["release_mbid"] == MB2).unwrap()["id"]
        .as_i64()
        .unwrap();
    let bare_album_id = albums.iter().find(|a| a["release_mbid"].is_null()).unwrap()["id"]
        .as_i64()
        .unwrap();
    let req = Request::builder()
        .uri(format!("/v1/library/albums/{album_id}/cover"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/png");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"\x89PNG-not-really");
    let (st, _) = call(&app, "GET", "/v1/library/albums/999/cover", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

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

    // Favourites: a tagged track hearts by id; an untagged one and the
    // album (no release id) are refused with a reason; the list joins back.
    let (st, tracks) = call(&app, "GET", "/v1/library/tracks?limit=50", None).await;
    assert_eq!(st, StatusCode::OK);
    let tracks = tracks["tracks"].as_array().unwrap().clone();
    let tagged = tracks
        .iter()
        .find(|t| t["canonical"].is_object() || t["canonical"].is_array())
        .unwrap_or(&tracks[0]);
    let tagged_id = tagged["id"].as_i64().unwrap();
    let untagged = tracks.iter().find(|t| t["canonical"].is_null());
    let (st, f) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"track_id": tagged_id})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{f}");
    assert_eq!(f["favourite"], true);
    let uri = f["uri"].as_str().unwrap().to_string();
    assert!(uri.starts_with("https://musicbrainz.org/"), "{uri}");
    if let Some(u) = untagged {
        let (st, e) = call(
            &app,
            "PUT",
            "/v1/favourites",
            Some(serde_json::json!({"track_id": u["id"]})),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    }
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"album_id": bare_album_id})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an album without a release id cannot be hearted"
    );
    let (st, f) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"album_id": album_id})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{f}");
    assert_eq!(f["uri"], format!("https://musicbrainz.org/release/{MB2}"));
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"track_id": 999_999})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(&app, "PUT", "/v1/favourites", Some(serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // By canonical URL too, idempotent; an unknown URL lands in `other`.
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"uri": uri})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(
            serde_json::json!({"uri": "https://musicbrainz.org/artist/833f00e1-781f-4edd-90e4-e52712618862"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, favs) = call(&app, "GET", "/v1/favourites", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(favs["tracks"].as_array().unwrap().len(), 1, "{favs}");
    assert_eq!(favs["tracks"][0]["id"], tagged_id);
    assert_eq!(favs["other"].as_array().unwrap().len(), 1);
    assert_eq!(favs["albums"].as_array().unwrap().len(), 1);
    assert_eq!(favs["uris"].as_array().unwrap().len(), 3);
    let (st, home) = call(&app, "GET", "/v1/library/home", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(home["favourite_albums"].is_array());
    // Un-heart by id through the query form.
    let (st, f) = call(
        &app,
        "DELETE",
        &format!("/v1/favourites?track_id={tagged_id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{f}");
    assert_eq!(f["favourite"], false);
    let (_, favs) = call(&app, "GET", "/v1/favourites", None).await;
    assert!(favs["tracks"].as_array().unwrap().is_empty());
    // Subsonic sees the same hearts.
    let (st, _) = call(
        &app,
        "GET",
        &format!("/rest/star?id={tagged_id}&f=json&u=pubky&p=x&v=1.16.1&c=t"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, sub) = call(
        &app,
        "GET",
        "/rest/getStarred2?f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        sub["subsonic-response"]["starred2"]["song"][0]["id"],
        tagged_id.to_string(),
        "{sub}"
    );
    let (st, _) = call(
        &app,
        "GET",
        &format!("/rest/unstar?id={tagged_id}&f=json&u=pubky&p=x&v=1.16.1&c=t"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getStarred2?f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert!(sub["subsonic-response"]["starred2"]["song"]
        .as_array()
        .unwrap()
        .is_empty());

    // Tags: labels are normalized like pubky.app, idempotent per (uri, label),
    // refused for objects without a canonical identity or over-long labels.
    let (st, t) = call(
        &app,
        "PUT",
        "/v1/tags",
        Some(serde_json::json!({"track_id": tagged_id, "label": " Disco "})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{t}");
    assert_eq!(t["label"], "disco");
    assert_eq!(t["tagged"], true);
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/tags",
        Some(serde_json::json!({"track_id": tagged_id, "label": "disco"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/tags",
        Some(serde_json::json!({"track_id": tagged_id, "label": "funk"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, e) = call(
        &app,
        "PUT",
        "/v1/tags",
        Some(serde_json::json!({"track_id": tagged_id, "label": "x".repeat(40)})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/tags",
        Some(serde_json::json!({"track_id": tagged_id, "label": ""})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    if let Some(u) = untagged {
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/tags",
            Some(serde_json::json!({"track_id": u["id"], "label": "disco"})),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let (st, mine) = call(&app, "GET", &format!("/v1/tags?track_id={tagged_id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{mine}");
    assert_eq!(mine["uri"], uri);
    let labels: Vec<&str> = mine["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels.len(), 2);
    assert!(labels.contains(&"disco") && labels.contains(&"funk"));
    assert_eq!(mine["tags"][0]["synced"], false, "no homeserver in this test");
    let (st, all) = call(&app, "GET", "/v1/tags", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(all["tags"].as_array().unwrap().len(), 2);
    let (st, gone) = call(
        &app,
        "DELETE",
        &format!("/v1/tags?track_id={tagged_id}&label=Disco"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{gone}");
    assert_eq!(gone["tagged"], false);
    let (_, mine) = call(&app, "GET", &format!("/v1/tags?uri={}", urlencoding(&uri)), None).await;
    assert_eq!(mine["tags"].as_array().unwrap().len(), 1);
    assert_eq!(mine["tags"][0]["label"], "funk");

    // Search spans tracks, albums, artists and genres.
    let (st, sr) = call(&app, "GET", "/v1/library/search?q=lists", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!sr["albums"].as_array().unwrap().is_empty(), "{sr}");
    let (_, sr) = call(&app, "GET", "/v1/library/search?q=tester", None).await;
    assert_eq!(sr["artists"][0]["name"], "Tester");
    let (_, sr) = call(&app, "GET", "/v1/library/search?q=disco", None).await;
    assert_eq!(sr["genres"][0]["name"], "Disco");
    assert!(
        !sr["tracks"].as_array().unwrap().is_empty(),
        "genre words reach the track search too"
    );

    // Genres come from the files' tags, split on separators.
    let (st, genres) = call(&app, "GET", "/v1/library/genres", None).await;
    assert_eq!(st, StatusCode::OK);
    let names: Vec<&str> = genres
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["value"].as_str().or(g["name"].as_str()).unwrap())
        .collect();
    assert_eq!(names, vec!["Disco", "Funk"], "{genres}");
    assert_eq!(genres[0]["tracks"], 2);
    let gid = genres[0]["id"].as_i64().unwrap();
    let (st, d) = call(&app, "GET", &format!("/v1/library/genres/{gid}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(d["genre"]["name"], "Disco");
    assert_eq!(d["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(d["tracks"][0]["genres"], serde_json::json!(["Disco", "Funk"]));
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getGenres?f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert_eq!(sub["subsonic-response"]["genres"]["genre"][0]["value"], "Disco");
    assert_eq!(sub["subsonic-response"]["genres"]["genre"][0]["songCount"], 2);
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getSongsByGenre?genre=funk&count=5&f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert_eq!(
        sub["subsonic-response"]["songsByGenre"]["song"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        sub["subsonic-response"]["songsByGenre"]["song"][0]["genre"],
        "Disco"
    );
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getAlbumList2?type=byGenre&genre=Disco&f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert!(!sub["subsonic-response"]["albumList2"]["album"]
        .as_array()
        .unwrap()
        .is_empty());
}

fn urlencoding(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

/// Albums without embedded art get their cover from the Cover Art Archive
/// (pointed at a local server here), once; the file is then served from disk.
#[tokio::test]
async fn covers_fall_back_to_the_cover_art_archive() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = hits.clone();
    let caa = axum::Router::new().route(
        "/release/{mbid}/front-500",
        axum::routing::get(move |axum::extract::Path(mbid): axum::extract::Path<String>| {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                if mbid == MB2 {
                    (
                        StatusCode::OK,
                        [("content-type", "image/png")],
                        b"\x89PNG-fake".to_vec(),
                    )
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        [("content-type", "text/plain")],
                        Vec::new(),
                    )
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, caa).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    // Tagged with a release id but no picture frame: the archive is asked.
    let path = music.join("bare.wav");
    {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..2205 {
            w.write_sample((i % 50) as i16).unwrap();
            w.write_sample(-((i % 50) as i16)).unwrap();
        }
        w.finalize().unwrap();
        let mut t = id3::Tag::new();
        t.set_title("Bare");
        t.set_artist("Tester");
        t.set_album("No Art");
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Album Id".into(),
            value: MB2.into(),
        });
        t.write_to_path(&path, id3::Version::Id3v24).unwrap();
    }
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
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    *s.caa_base.write().unwrap() = format!("http://127.0.0.1:{port}");
    let app = router(s.clone());
    call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    for _ in 0..500 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, albums) = call(&app, "GET", "/v1/library/albums", None).await;
    let album = albums
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "No Art")
        .unwrap();
    assert_eq!(album["release_mbid"], MB2);
    let id = album["id"].as_i64().unwrap();
    for round in 0..2 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/library/albums/{id}/cover"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "round {round}");
        assert_eq!(resp.headers()["content-type"], "image/png");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"\x89PNG-fake");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "fetched once, then served from the art dir"
    );
    let art = s.library.lock().unwrap().album_art(id).unwrap().unwrap();
    assert_eq!(art.source, "coverartarchive");
    assert!(art.file.starts_with(dir.path().join("art")));
}
