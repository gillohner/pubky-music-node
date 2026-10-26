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
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Artist Id".into(),
            value: MB.into(),
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
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
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

    // An untagged track never enters social data: it stays in the playlist on this node only
    let (st, pl) = call(
        &app,
        "POST",
        &format!("/v1/playlists/{id}/items"),
        Some(serde_json::json!({"track_ids": [untagged]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{pl}");
    assert_eq!(
        pl["playlist"]["items"].as_array().unwrap().len(),
        2,
        "not in the synced items"
    );
    assert_eq!(pl["local_tracks"][0]["id"], untagged);
    assert_eq!(pl["summary"]["items"], 3);
    let (st, pl) = call(&app, "DELETE", &format!("/v1/playlists/{id}/local/0"), None).await;
    assert_eq!(st, StatusCode::OK, "{pl}");
    assert!(pl.get("local_tracks").is_none(), "{pl}");
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

    // Favourites: a tagged track hearts by id; an untagged one and an album
    // without a release id are kept on this node; the list joins back.
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
    // A clear made offline before the heart was set doesn't undo it; a later one does
    let (st, f) = call(
        &app,
        "DELETE",
        &format!("/v1/favourites?track_id={tagged_id}&at=1000"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{f}");
    assert_eq!(f["favourite"], true, "an older clear is skipped");
    let (_, all) = call(&app, "GET", "/v1/favourites", None).await;
    assert!(all["uris"].as_array().unwrap().iter().any(|x| *x == uri.as_str()));
    let later = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 5;
    let (st, f) = call(
        &app,
        "DELETE",
        &format!("/v1/favourites?track_id={tagged_id}&at={later}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{f}");
    assert_eq!(f["favourite"], false);
    // A heart made offline keeps its time, so a clear from before it is skipped too
    let (st, f) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"track_id": tagged_id, "at": 2000})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{f}");
    assert_eq!(f["favourite"], true);
    let (_, f) = call(
        &app,
        "DELETE",
        &format!("/v1/favourites?track_id={tagged_id}&at=1500"),
        None,
    )
    .await;
    assert_eq!(f["favourite"], true);
    // Hearted at T0 (2000), cleared at T1.5 and hearted again at T2 (both now): a clear made
    // offline at T1 (3000) doesn't undo the T2 heart
    let (_, f) = call(
        &app,
        "DELETE",
        &format!("/v1/favourites?track_id={tagged_id}"),
        None,
    )
    .await;
    assert_eq!(f["favourite"], false);
    let (_, f) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"track_id": tagged_id})),
    )
    .await;
    assert_eq!(f["favourite"], true);
    let (st, f) = call(
        &app,
        "DELETE",
        &format!("/v1/favourites?track_id={tagged_id}&at=3000"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{f}");
    assert_eq!(f["favourite"], true, "the heart set again after the clear stays");
    if let Some(u) = untagged {
        let (st, e) = call(
            &app,
            "PUT",
            "/v1/favourites",
            Some(serde_json::json!({"track_id": u["id"]})),
        )
        .await;
        // Kept as a favourite on this node only, never synced
        assert_eq!(st, StatusCode::OK, "{e}");
        assert_eq!(e["uri"], format!("track:{}", u["id"]));
        let (_, all) = call(&app, "GET", "/v1/favourites", None).await;
        assert!(
            all["uris"].as_array().unwrap().iter().any(|x| *x == e["uri"]),
            "{all}"
        );
        let (st, _) = call(
            &app,
            "DELETE",
            &format!("/v1/favourites?track_id={}", u["id"]),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }
    // An album without a release id is kept on this node, listed, and can be unhearted
    let (st, b) = call(
        &app,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"album_id": bare_album_id})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["uri"], format!("album:{bare_album_id}"));
    let (_, all) = call(&app, "GET", "/v1/favourites", None).await;
    assert!(
        all["albums"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == bare_album_id),
        "{all}"
    );
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/v1/favourites?album_id={bare_album_id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
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
    // A Subsonic scrobble is a play in the history, under the client's name; the
    // recent/frequent album lists and now-playing answer from the same data.
    let (st, _) = call(
        &app,
        "GET",
        &format!("/rest/scrobble?id={tagged_id}&f=json&u=pubky&p=x&v=1.16.1&c=symfonium"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, hist) = call(&app, "GET", "/v1/library/history", None).await;
    assert_eq!(hist["plays"][0]["zone"], "subsonic:symfonium", "{hist}");
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getAlbumList2?type=frequent&f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert_eq!(
        sub["subsonic-response"]["albumList2"]["album"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "{sub}"
    );
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getAlbumList2?type=recent&f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert_eq!(
        sub["subsonic-response"]["albumList2"]["album"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getNowPlaying?f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert!(sub["subsonic-response"]["nowPlaying"]["entry"]
        .as_array()
        .unwrap()
        .is_empty());

    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getAlbumList2?type=starred&f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert_eq!(
        sub["subsonic-response"]["albumList2"]["album"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "starred albums list the hearted album"
    );

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
        // It waits for a match (STEWARD-UX-INSTANCES item 6)
        assert_eq!(st, StatusCode::OK);
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

    // Subsonic internet radio maps onto the node's saved stations.
    let (st, _) = call(&app, "GET", "/rest/createInternetRadioStation?streamUrl=https%3A%2F%2Fice1.somafm.com%2Fgroovesalad-128-mp3&name=Groove%20Salad&homepageUrl=https%3A%2F%2Fsomafm.com&f=json&u=pubky&p=x&v=1.16.1&c=t", None).await;
    assert_eq!(st, StatusCode::OK);
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getInternetRadioStations?f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    let radio = &sub["subsonic-response"]["internetRadioStations"]["internetRadioStation"];
    assert_eq!(radio.as_array().unwrap().len(), 1, "{sub}");
    assert_eq!(radio[0]["name"], "Groove Salad");
    assert_eq!(radio[0]["homePageUrl"], "https://somafm.com");
    let sid = radio[0]["id"].as_str().unwrap().to_string();
    let (_, saved) = call(&app, "GET", "/v1/radio/stations", None).await;
    assert_eq!(
        saved["stations"][0]["id"], sid,
        "the same list as the node's Radio screen"
    );
    let (st, _) = call(&app, "GET", &format!("/rest/updateInternetRadioStation?id={sid}&streamUrl=https%3A%2F%2Fice1.somafm.com%2Fgroovesalad-256-mp3&name=Groove%20Salad%20256&f=json&u=pubky&p=x&v=1.16.1&c=t"), None).await;
    assert_eq!(st, StatusCode::OK);
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getInternetRadioStations?f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    let radio = &sub["subsonic-response"]["internetRadioStations"]["internetRadioStation"];
    assert_eq!(radio.as_array().unwrap().len(), 1);
    assert_eq!(radio[0]["name"], "Groove Salad 256");
    let sid2 = radio[0]["id"].as_str().unwrap().to_string();
    let (_, sub) = call(
        &app,
        "GET",
        &format!("/rest/deleteInternetRadioStation?id={sid2}&f=json&u=pubky&p=x&v=1.16.1&c=t"),
        None,
    )
    .await;
    assert_eq!(sub["subsonic-response"]["status"], "ok", "{sub}");
    let (_, sub) = call(
        &app,
        "GET",
        "/rest/getInternetRadioStations?f=json&u=pubky&p=x&v=1.16.1&c=t",
        None,
    )
    .await;
    assert!(
        sub["subsonic-response"]["internetRadioStations"]["internetRadioStation"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Reviews: one per target, edits keep the id, a rating alone is fine.
    let (st, r) = call(
        &app,
        "PUT",
        "/v1/reviews",
        Some(serde_json::json!({"track_id": tagged_id, "rating": 4})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let rid = r["id"].as_str().unwrap().to_string();
    assert_eq!(r["rating"], 4);
    let (st, r2) = call(
        &app,
        "PUT",
        "/v1/reviews",
        Some(serde_json::json!({"track_id": tagged_id, "rating": 5, "text": "  Desert island.  "})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r2}");
    assert_eq!(r2["id"], rid, "editing keeps the post id");
    assert_eq!(r2["text"], "Desert island.");
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/reviews",
        Some(serde_json::json!({"track_id": tagged_id, "rating": 11})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/reviews",
        Some(serde_json::json!({"track_id": tagged_id})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    if let Some(u) = untagged {
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/reviews",
            Some(serde_json::json!({"track_id": u["id"], "rating": 3})),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let (_, mine) = call(&app, "GET", &format!("/v1/reviews?track_id={tagged_id}"), None).await;
    assert_eq!(mine["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(mine["reviews"][0]["rating"], 5);
    let (_, all) = call(&app, "GET", "/v1/reviews", None).await;
    assert_eq!(all["reviews"].as_array().unwrap().len(), 1);
    let (st, _) = call(&app, "DELETE", &format!("/v1/reviews?track_id={tagged_id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    let (_, mine) = call(&app, "GET", &format!("/v1/reviews?track_id={tagged_id}"), None).await;
    assert!(mine["reviews"].as_array().unwrap().is_empty());

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
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
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

/// Artist pages pull a portrait and a biography through MusicBrainz →
/// Wikidata → Wikipedia (all mocked here), once, then from the cache.
#[tokio::test]
async fn artist_info_comes_from_open_sources_and_is_cached() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = hits.clone();
    let count = move || {
        h.fetch_add(1, Ordering::SeqCst);
    };
    let (c1, c2, c3, c4) = (count.clone(), count.clone(), count.clone(), count.clone());
    let mock = axum::Router::new()
        .route(
            "/ws/2/artist/{mbid}",
            axum::routing::get(move || {
                c1();
                async {
                    axum::Json(serde_json::json!({"relations": [
                        {"type": "wikidata", "url": {"resource": "https://www.wikidata.org/wiki/Q1"}},
                        {"type": "official homepage", "url": {"resource": "https://daftpunk.example"}}]}))
                }
            }),
        )
        .route(
            "/wiki/Special:EntityData/{q}",
            axum::routing::get(move || {
                c2();
                async {
                    axum::Json(serde_json::json!({"entities": {"Q1": {
                        "claims": {"P18": [{"mainsnak": {"datavalue": {"value": "Daft Punk 2013.jpg"}}}]},
                        "sitelinks": {"enwiki": {"title": "Daft Punk"}}}}}))
                }
            }),
        )
        .route(
            "/api/rest_v1/page/summary/{title}",
            axum::routing::get(move || {
                c3();
                async {
                    axum::Json(
                        serde_json::json!({"extract": "Daft Punk were a French electronic music duo.",
                        "content_urls": {"desktop": {"page": "https://en.wikipedia.org/wiki/Daft_Punk"}}}),
                    )
                }
            }),
        )
        .route(
            "/wiki/Special:FilePath/{file}",
            axum::routing::get(move || {
                c4();
                async { ([("content-type", "image/jpeg")], b"\xFF\xD8fake-jpeg".to_vec()) }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("a.wav"), "Get Lucky", Some(MB));
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
    {
        let base = format!("http://127.0.0.1:{port}");
        let mut b = s.info_bases.write().unwrap();
        b.musicbrainz = base.clone();
        b.wikidata = base.clone();
        b.wikipedia = base.clone();
        b.commons = base;
    }
    let app = router(s.clone());
    call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    for _ in 0..500 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, artists) = call(&app, "GET", "/v1/library/artists", None).await;
    let artist = artists
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "Tester")
        .unwrap();
    assert_eq!(artist["mbid"], MB);
    let id = artist["id"].as_i64().unwrap();
    for _ in 0..2 {
        let (st, info) = call(&app, "GET", &format!("/v1/library/artists/{id}/info"), None).await;
        assert_eq!(st, StatusCode::OK, "{info}");
        assert_eq!(info["bio"], "Daft Punk were a French electronic music duo.");
        assert_eq!(info["image"], true);
        assert_eq!(info["homepage"], "https://daftpunk.example");
        assert_eq!(info["wikipedia"], "https://en.wikipedia.org/wiki/Daft_Punk");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        4,
        "four upstream calls, then the cache"
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/library/artists/{id}/image"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/jpeg");
    let (st, _) = call(&app, "GET", "/v1/library/artists/999/info", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Where each item plays from (play_from.rs): the library first by default, a direct file link,
/// a service that is not connected with its reason, pins that survive reordering, and the
/// playlist's own order next to the node's global one.
#[tokio::test]
async fn items_resolve_their_source_with_pins_and_orders() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("tagged.wav"), "Tagged", Some(MB));
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
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        outputs,
    );
    s.zones_for_all_outputs();
    let app = router(s.clone());
    call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    for _ in 0..500 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let song = format!("https://musicbrainz.org/recording/{MB}");
    let file = "https://archive.org/download/cornellgem/1.%20Minglewood%20Blues.mp3";
    let tidal = "https://tidal.com/track/50850120";
    let (st, pl) = call(
        &app,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({"name": "Mixed", "uris": [song, file, tidal]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{pl}");
    let id = pl["summary"]["id"].as_str().unwrap().to_string();

    let (st, pf) = call(&app, "GET", &format!("/v1/playlists/{id}/play-from"), None).await;
    assert_eq!(st, StatusCode::OK, "{pf}");
    assert_eq!(pf["own_order"], false);
    assert_eq!(
        pf["order"]["order"],
        serde_json::json!(["library", "tidal", "spotify", "jamendo"])
    );
    let items = pf["items"].as_array().unwrap();
    assert_eq!(items[0]["plays_from"]["service"], "library");
    assert!(items[0]["track_id"].is_i64());
    assert_eq!(items[0]["title"], "Tagged");
    assert_eq!(items[1]["plays_from"]["service"], "archive");
    assert!(items[2]["plays_from"].is_null(), "{}", items[2]);
    assert_eq!(items[2]["reason"], "Not on your connected services");
    assert_eq!(items[2]["connect"], serde_json::json!(["tidal"]));
    assert_eq!(items[2]["sources"][0]["mode"], "none");

    // Pin item 0 to TIDAL: not connected here, so the order still decides, and the chip says so
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}/items/0/source"),
        Some(serde_json::json!({"service": "tidal", "uri": tidal})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, pf) = call(&app, "GET", &format!("/v1/playlists/{id}/play-from"), None).await;
    assert_eq!(pf["items"][0]["pin"]["service"], "tidal");
    assert_eq!(pf["items"][0]["pin_unavailable"], true);
    assert_eq!(pf["items"][0]["plays_from"]["service"], "library");
    assert!(pf["items"][0]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["service"] == "tidal"));

    // Reordering keeps the pin with its item
    let (_, d) = call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}"),
        Some(serde_json::json!({"uris": [tidal, file, song]})),
    )
    .await;
    assert_eq!(d["playlist"]["items"][2]["source"]["service"], "tidal", "{d}");
    assert!(d["playlist"]["items"][0].get("source").is_none());
    // Auto clears it
    let (_, d) = call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}/items/2/source"),
        Some(serde_json::json!({"service": null})),
    )
    .await;
    assert!(d["playlist"]["items"][2].get("source").is_none(), "{d}");

    // The playlist's own order, then back to the node's
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}/order"),
        Some(serde_json::json!({"order": ["qobuz"]})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (_, d) = call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}/order"),
        Some(serde_json::json!({"order": ["spotify", "tidal"]})),
    )
    .await;
    assert_eq!(
        d["playlist"]["source_order"],
        serde_json::json!(["spotify", "tidal", "library", "jamendo"])
    );
    let (_, pf) = call(&app, "GET", &format!("/v1/playlists/{id}/play-from"), None).await;
    assert_eq!(pf["own_order"], true);
    assert_eq!(
        pf["items"][2]["plays_from"]["service"], "library",
        "the only usable source"
    );
    let (_, d) = call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}/order"),
        Some(serde_json::json!({"order": null})),
    )
    .await;
    assert!(d["playlist"].get("source_order").is_none());

    // The node's global order
    let (_, o) = call(
        &app,
        "PUT",
        "/v1/play-order",
        Some(serde_json::json!({"order": ["tidal"]})),
    )
    .await;
    assert_eq!(
        o["order"],
        serde_json::json!(["tidal", "library", "spotify", "jamendo"])
    );
    assert_eq!(o["services"][0]["connected"], false);
    let (_, o) = call(&app, "GET", "/v1/play-order", None).await;
    assert_eq!(o["order"][0], "tidal");

    // Playing queues what plays here and reports the rest
    let zone = s.zones.list()[0].id.to_string();
    let (st, r) = call(
        &app,
        "POST",
        &format!("/v1/playlists/{id}/play"),
        Some(serde_json::json!({"zone": zone})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["queued"], 2);
    assert_eq!(r["missing"], serde_json::json!([tidal]));
}

fn settings_for(
    dir: &std::path::Path,
    name: &str,
    music: Option<std::path::PathBuf>,
    token: Option<&str>,
) -> NodeSettings {
    let db = dir.join(format!("{name}.sqlite"));
    NodeSettings {
        name: name.into(),
        version: "0".into(),
        db_path: db,
        scan_folders: music.into_iter().collect(),
        art_dir: dir.join(format!("{name}-art")),
        mirror_dir: dir.join(format!("{name}-homeserver")),
        pipeline: PipelineConfig::default(),
        token: token.map(str::to_string),
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
    }
}

/// Your own Subsonic server as a source (step 5.1): node B connects to node A's Subsonic API
/// (any OpenSubsonic server would do), searches it, plays a song from it, shows its covers and
/// imports its playlist by MusicBrainz id. The password never comes back from the API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_subsonic_server_is_a_source() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("tagged.wav"), "Tagged", Some(MB));
    // Node A: the server
    let a = NodeState::new(
        settings_for(dir.path(), "a", Some(music.clone()), Some("sesame")),
        Library::open(&dir.path().join("a.sqlite")).unwrap(),
        OutputRegistry::new(),
    );
    let app_a = router(a.clone());
    let req = |m: &str, u: &str, b: Option<serde_json::Value>| {
        let mut r = Request::builder()
            .method(m)
            .uri(u)
            .header("authorization", "Bearer sesame");
        if b.is_some() {
            r = r.header("content-type", "application/json");
        }
        r.body(match b {
            Some(b) => Body::from(b.to_string()),
            None => Body::empty(),
        })
        .unwrap()
    };
    let resp = app_a
        .clone()
        .oneshot(req("POST", "/v1/library/scan", Some(serde_json::json!({}))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    for _ in 0..500 {
        if !a.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let resp = app_a
        .clone()
        .oneshot(req("POST", "/v1/playlists", Some(serde_json::json!({"name": "From the NAS", "uris": [format!("https://musicbrainz.org/recording/{MB}")]}))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app_a).await.unwrap() });

    // Node B: connects to it
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let b = NodeState::new(
        settings_for(dir.path(), "b", None, None),
        Library::open(&dir.path().join("b.sqlite")).unwrap(),
        outputs,
    );
    b.zones_for_all_outputs();
    let app = router(b.clone());
    let (st, e) = call(
        &app,
        "PUT",
        "/v1/sources/subsonic/server",
        Some(serde_json::json!({"url": format!("http://127.0.0.1:{port}/rest"), "user": "pubky", "password": "wrong"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_GATEWAY, "a wrong password is refused: {e}");
    let (st, v) = call(
        &app,
        "PUT",
        "/v1/sources/subsonic/server",
        Some(serde_json::json!({"url": format!("http://127.0.0.1:{port}"), "user": "pubky", "password": "sesame"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "Pubky Music node");
    assert!(!v.to_string().contains("sesame"));
    let (_, sources) = call(&app, "GET", "/v1/sources", None).await;
    assert!(sources
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["id"] == "subsonic" && x["status"] == "ready"));

    let (st, r) = call(&app, "GET", "/v1/sources/subsonic/search?q=Tagged", None).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let song = r["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "track")
        .unwrap()
        .clone();
    assert_eq!(song["url"], format!("https://musicbrainz.org/recording/{MB}"));
    let uri = song["uri"].as_str().unwrap().to_string();
    assert!(uri.starts_with("subsonic:track:"), "{uri}");
    assert!(!r.to_string().contains("sesame"));
    let (_, ra) = call(&app, "GET", "/v1/sources/subsonic/search?q=Lists", None).await;
    let album = ra["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "album")
        .unwrap()
        .clone();
    let (st, al) = call(
        &app,
        "GET",
        &format!("/v1/sources/subsonic/items/{}", album["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{al}");
    assert_eq!(al["tracks"][0]["url"], uri);

    // Plays in a zone, from a freshly signed stream URL
    let zid = b.zones.list()[0].id.to_string();
    let (st, q) = call(
        &app,
        "POST",
        &format!("/v1/zones/{zid}/queue"),
        Some(serde_json::json!({"items": [{"uri": uri, "title": "Tagged"}], "replace": true, "play": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{q}");
    let zone = b.zones.get(&ZoneId::new(&zid)).unwrap();
    assert_eq!(zone.wait_stopped().await.error, None);

    // Browsers get the stream and covers relayed
    let id = uri.trim_start_matches("subsonic:track:");
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/v1/sources/subsonic/stream/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..4], b"RIFF", "the original file");

    // Its playlists import by MusicBrainz id
    let (st, pls) = call(&app, "GET", "/v1/sources/subsonic/playlists", None).await;
    assert_eq!(st, StatusCode::OK, "{pls}");
    assert_eq!(pls[0]["name"], "From the NAS");
    let (st, imp) = call(
        &app,
        "POST",
        &format!(
            "/v1/sources/subsonic/playlists/{}/import",
            pls[0]["id"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{imp}");
    assert_eq!(imp["imported"], 1);
    assert_eq!(
        imp["playlist"]["playlist"]["items"][0]["uri"],
        format!("https://musicbrainz.org/recording/{MB}")
    );

    let (_, v) = call(&app, "DELETE", "/v1/sources/subsonic/server", None).await;
    assert_eq!(v["connected"], false);
    let (st, _) = call(&app, "GET", "/v1/sources/subsonic/search?q=x", None).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
}

async fn upload(app: &axum::Router, uri: &str, bytes: Vec<u8>) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/octet-stream")
        .body(Body::from(bytes))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Moving from Roon with its exports (SYNTHETIC fixtures in Roon's shape, tests/fixtures/roon/):
/// a dry run reports, the import applies what was chosen, and every miss says why.
#[tokio::test]
async fn roon_exports_move_over_with_a_report() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("tagged.wav"), "Tagged", Some(MB));
    write_wav(&music.join("untagged.wav"), "Untagged", None);
    let s = NodeState::new(
        settings_for(dir.path(), "roon", Some(music.clone()), None),
        Library::open(&dir.path().join("roon.sqlite")).unwrap(),
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
    let fx = |n: &str| {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/roon")
                .join(n),
        )
        .unwrap()
    };

    // Tracks: a dry run changes nothing
    let (st, r) = upload(
        &app,
        "/v1/import/roon?file=SYNTHETIC-roon-tracks.xlsx",
        fx("SYNTHETIC-roon-tracks.xlsx"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["kind"], "tracks");
    assert_eq!(
        (r["rows"].as_u64(), r["matched"].as_u64(), r["unmatched"].as_u64()),
        (Some(3), Some(2), Some(1)),
        "{r}"
    );
    assert_eq!(
        r["not_identified"], 1,
        "the untagged favourite needs an identity first: {r}"
    );
    assert_eq!(r["favourites"], 1);
    assert_eq!(r["dry_run"], true);
    assert_eq!(r["tags_found"][0], serde_json::json!(["late night", 2]));
    let reasons: Vec<&str> = r["misses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["reason"].as_str().unwrap())
        .collect();
    assert!(
        reasons.iter().any(|x| x.starts_with("not in this library")),
        "{reasons:?}"
    );
    assert!(reasons.iter().any(|x| x.contains("identify it")), "{reasons:?}");
    let (_, favs) = call(&app, "GET", "/v1/favourites", None).await;
    assert!(!favs.to_string().contains(MB), "nothing written by a dry run");

    // Then for real, with the tags made public on purpose
    let (st, r) = upload(
        &app,
        "/v1/import/roon?file=SYNTHETIC-roon-tracks.xlsx&dry_run=false&tags=true",
        fx("SYNTHETIC-roon-tracks.xlsx"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(
        (r["favourites"].as_u64(), r["tags_added"].as_u64()),
        (Some(1), Some(2)),
        "{r}"
    );
    let (_, favs) = call(&app, "GET", "/v1/favourites", None).await;
    assert!(favs.to_string().contains(MB), "{favs}");
    let (_, tags) = call(&app, "GET", "/v1/tags", None).await;
    assert!(
        tags.to_string().contains("classics") && tags.to_string().contains("late-night"),
        "{tags}"
    );

    // Albums
    let (st, r) = upload(
        &app,
        "/v1/import/roon?file=SYNTHETIC-roon-albums.csv",
        fx("SYNTHETIC-roon-albums.csv"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["kind"], "albums");
    assert_eq!(
        (r["matched"].as_u64(), r["unmatched"].as_u64()),
        (Some(1), Some(1)),
        "{r}"
    );

    // A playlist
    let (st, r) = upload(
        &app,
        "/v1/import/roon?file=SYNTHETIC-roon-playlist.xlsx&dry_run=false",
        fx("SYNTHETIC-roon-playlist.xlsx"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["kind"], "playlist");
    assert_eq!(r["name"], "Late night");
    assert_eq!(
        (r["matched"].as_u64(), r["unmatched"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(r["playlist"]["imported"], 1, "{r}");

    // Not an export
    let (st, _) = upload(&app, "/v1/import/roon?file=x.csv", b"Foo,Bar\n1,2\n".to_vec()).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

/// An album with no MusicBrainz ids gets its cover from Apple's iTunes Search API, but only
/// from a result whose title and artist match (mocked here), once, then from the art dir.
#[tokio::test]
async fn album_cover_falls_back_to_itunes_on_an_exact_match() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let searches = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let (n, b) = (searches.clone(), base.clone());
    let mock = axum::Router::new()
        .route(
            "/search",
            axum::routing::get(
                move |axum::extract::Query(q): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| {
                    n.fetch_add(1, Ordering::SeqCst);
                    let b = b.clone();
                    async move {
                        assert_eq!(q["entity"], "album");
                        assert_eq!(q["term"], "Daft Punk Homework");
                        axum::Json(serde_json::json!({"resultCount": 2, "results": [
                            {"collectionName": "Homework", "artistName": "Someone Else",
                             "artworkUrl100": format!("{b}/img/wrong/100x100bb.jpg")},
                            {"collectionName": "Homework (Remastered)", "artistName": "Daft Punk",
                             "artworkUrl100": format!("{b}/img/right/100x100bb.jpg")}]}))
                    }
                },
            ),
        )
        .route(
            "/img/{which}/{size}",
            axum::routing::get(
                |axum::extract::Path((which, size)): axum::extract::Path<(String, String)>| async move {
                    assert_eq!(which, "right", "only the matching result is fetched");
                    assert_eq!(size, "600x600bb.jpg");
                    ([("content-type", "image/jpeg")], b"\xff\xd8itunes".to_vec())
                },
            ),
        );
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    pm_api::art::set_itunes_base(&base);

    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let path = music.join("01.wav");
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
        t.set_title("Da Funk");
        t.set_artist("Daft Punk");
        t.set_album("Homework");
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
    let (_, albums) = call(&app, "GET", "/v1/library/albums", None).await;
    let id = albums
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Homework")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
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
        assert_eq!(resp.headers()["content-type"], "image/jpeg");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"\xff\xd8itunes");
    }
    assert_eq!(
        searches.load(Ordering::SeqCst),
        1,
        "asked once, then served from the art dir"
    );
    assert_eq!(
        s.library.lock().unwrap().album_art(id).unwrap().unwrap().source,
        "itunes"
    );
}
