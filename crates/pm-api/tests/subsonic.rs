//! OpenSubsonic surface: auth, ID3 browsing, search, album lists, streaming
//! with ranges, cover art, playlists and scrobbles.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use id3::TagLike;
use md5::{Digest, Md5};
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use tower::ServiceExt;

const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";

fn write_wav(path: &std::path::Path, title: &str, artist: &str, album: &str, no: u32, mbid: Option<&str>) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..4410 {
        w.write_sample((i % 50) as i16).unwrap();
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    t.set_title(title);
    t.set_artist(artist);
    t.set_album(album);
    t.set_track(no);
    t.set_year(2013);
    if let Some(m) = mbid {
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Track Id".into(),
            value: m.into(),
        });
        t.add_frame(id3::frame::Picture {
            mime_type: "image/jpeg".into(),
            picture_type: id3::frame::PictureType::CoverFront,
            description: String::new(),
            data: vec![0xff, 0xd8, 1, 2, 3],
        });
    }
    t.write_to_path(path, id3::Version::Id3v24).unwrap();
}

async fn get(
    app: &axum::Router,
    uri: &str,
    range: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().uri(uri);
    if let Some(r) = range {
        req = req.header("range", r);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, bytes)
}

async fn call(app: &axum::Router, uri: &str) -> serde_json::Value {
    let (st, _, bytes) = get(app, uri, None).await;
    assert_eq!(st, StatusCode::OK, "{uri}");
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["subsonic-response"].clone()
}

#[tokio::test]
async fn browse_stream_and_playlists_like_a_subsonic_client() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(
        &music.join("01.wav"),
        "Get Lucky",
        "Daft Punk",
        "Random Access Memories",
        1,
        Some(MB),
    );
    write_wav(
        &music.join("02.wav"),
        "Instant Crush",
        "Daft Punk",
        "Random Access Memories",
        2,
        None,
    );
    write_wav(
        &music.join("03.wav"),
        "Blue Monday",
        "New Order",
        "Substance",
        1,
        None,
    );
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
            token: Some("pw".into()),
            subsonic_user: "alice".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            slimproto_port: 0,
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    let app = router(s.clone());
    let salt = "abc123";
    let mut h = Md5::new();
    h.update(b"pw");
    h.update(salt.as_bytes());
    let tok = hex::encode(h.finalize());
    let auth = format!("u=alice&t={tok}&s={salt}&v=1.16.1&c=test&f=json");

    // Auth: token auth, plain, wrong, missing.
    let r = call(&app, &format!("/rest/ping.view?{auth}")).await;
    assert_eq!(r["status"], "ok");
    assert_eq!(r["openSubsonic"], true);
    let r = call(&app, "/rest/ping?u=alice&p=pw&v=1.16.1&c=test&f=json").await;
    assert_eq!(r["status"], "ok");
    let r = call(&app, "/rest/ping?u=alice&p=enc:7077&v=1.16.1&c=test&f=json").await;
    assert_eq!(r["status"], "ok");
    let r = call(&app, "/rest/ping?u=alice&p=nope&v=1.16.1&c=test&f=json").await;
    assert_eq!(r["status"], "failed");
    assert_eq!(r["error"]["code"], 40);
    let r = call(&app, "/rest/ping?u=bob&p=pw&v=1.16.1&c=test&f=json").await;
    assert_eq!(r["error"]["code"], 40);

    // Without f=json the answer is XML (Subsonic's default), with the same auth.
    let (st, h, bytes) = get(&app, "/rest/ping.view?u=alice&p=pw&v=1.16.1&c=test", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(h["content-type"].to_str().unwrap().starts_with("application/xml"));
    let xml = String::from_utf8(bytes).unwrap();
    assert!(
        xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?><subsonic-response "),
        "{xml}"
    );
    assert!(xml.contains("xmlns=\"http://subsonic.org/restapi\""), "{xml}");
    assert!(xml.contains("status=\"ok\""));

    // Scan through the Subsonic verb, then browse by ID3.
    call(&app, &format!("/rest/startScan?{auth}")).await;
    for _ in 0..500 {
        if !s.is_scanning() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let r = call(&app, &format!("/rest/getArtists?{auth}")).await;
    let index = r["artists"]["index"].as_array().unwrap();
    let names: Vec<&str> = index
        .iter()
        .flat_map(|i| {
            i["artist"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["name"].as_str().unwrap())
        })
        .collect();
    assert_eq!(names, ["Daft Punk", "New Order"]);
    let daft = index[0]["artist"][0]["id"].as_str().unwrap().to_string();
    assert!(daft.starts_with("ar-"));
    assert_eq!(index[0]["artist"][0]["albumCount"], 1);

    let r = call(&app, &format!("/rest/getArtist?id={daft}&{auth}")).await;
    let album = r["artist"]["album"][0].clone();
    assert_eq!(album["name"], "Random Access Memories");
    assert_eq!(album["songCount"], 2);
    assert_eq!(album["year"], 2013);
    let album_id = album["id"].as_str().unwrap().to_string();
    assert_eq!(album["coverArt"], album_id);

    let r = call(&app, &format!("/rest/getAlbum?id={album_id}&{auth}")).await;
    let songs = r["album"]["song"].as_array().unwrap();
    assert_eq!(songs.len(), 2);
    assert_eq!(songs[0]["title"], "Get Lucky");
    assert_eq!(songs[0]["track"], 1);
    assert_eq!(songs[0]["suffix"], "wav");
    assert_eq!(songs[0]["samplingRate"], 44100);
    assert_eq!(songs[0]["bitDepth"], 16);
    assert_eq!(songs[0]["musicBrainzId"], MB);
    let get_lucky = songs[0]["id"].as_str().unwrap().to_string();
    let instant_crush = songs[1]["id"].as_str().unwrap().to_string();

    let r = call(
        &app,
        &format!("/rest/getAlbumList2?type=alphabeticalByName&size=10&{auth}"),
    )
    .await;
    let list = r["albumList2"]["album"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["name"], "Random Access Memories");
    let r = call(&app, &format!("/rest/getAlbumList2?type=newest&size=1&{auth}")).await;
    assert_eq!(r["albumList2"]["album"].as_array().unwrap().len(), 1);
    let r = call(&app, &format!("/rest/getAlbumList2?type=random&size=5&{auth}")).await;
    assert_eq!(r["albumList2"]["album"].as_array().unwrap().len(), 2);

    let r = call(&app, &format!("/rest/search3?query=lucky&{auth}")).await;
    assert_eq!(r["searchResult3"]["song"][0]["title"], "Get Lucky");
    let r = call(&app, &format!("/rest/search3?query=order&{auth}")).await;
    assert_eq!(r["searchResult3"]["artist"][0]["name"], "New Order");
    // Albums match on their artist too.
    assert_eq!(r["searchResult3"]["album"][0]["name"], "Substance");
    let r = call(&app, &format!("/rest/search3?query=substance&{auth}")).await;
    assert_eq!(r["searchResult3"]["album"][0]["name"], "Substance");

    // Stream: whole file, then a range.
    let (st, h, bytes) = get(&app, &format!("/rest/stream?id={get_lucky}&{auth}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h["content-type"], "audio/wav");
    assert_eq!(bytes, std::fs::read(music.join("01.wav")).unwrap());
    let (st, h, part) = get(
        &app,
        &format!("/rest/stream?id={get_lucky}&{auth}"),
        Some("bytes=10-19"),
    )
    .await;
    assert_eq!(st, StatusCode::PARTIAL_CONTENT);
    assert_eq!(h["content-range"], format!("bytes 10-19/{}", bytes.len()));
    assert_eq!(part, bytes[10..20].to_vec());
    let (st, h, art) = get(&app, &format!("/rest/getCoverArt?id={album_id}&{auth}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h["content-type"], "image/jpeg");
    assert_eq!(art, vec![0xff, 0xd8, 1, 2, 3]);

    // Playlists: create with the tagged song; the untagged one is refused with a clear message.
    let r = call(
        &app,
        &format!("/rest/createPlaylist?name=Favourites&songId={get_lucky}&{auth}"),
    )
    .await;
    assert_eq!(r["status"], "ok", "{r}");
    let pl_id = r["playlist"]["id"].as_str().unwrap().to_string();
    assert!(pl_id.starts_with("pl-"));
    assert_eq!(r["playlist"]["songCount"], 1);
    assert_eq!(r["playlist"]["entry"][0]["title"], "Get Lucky");
    let r = call(
        &app,
        &format!("/rest/updatePlaylist?playlistId={pl_id}&songIdToAdd={instant_crush}&{auth}"),
    )
    .await;
    assert_eq!(r["status"], "failed");
    assert!(r["error"]["message"].as_str().unwrap().contains("MusicBrainz"));
    let r = call(
        &app,
        &format!("/rest/updatePlaylist?playlistId={pl_id}&name=Faves&comment=best&{auth}"),
    )
    .await;
    assert_eq!(r["status"], "ok");
    let r = call(&app, &format!("/rest/getPlaylists?{auth}")).await;
    assert_eq!(r["playlists"]["playlist"][0]["name"], "Faves");
    assert_eq!(r["playlists"]["playlist"][0]["comment"], "best");
    let r = call(&app, &format!("/rest/getPlaylist?id={pl_id}&{auth}")).await;
    assert_eq!(r["playlist"]["entry"].as_array().unwrap().len(), 1);
    // The same playlist is visible on /v1 (one store underneath).
    let (st, _, body) = get(&app, "/v1/playlists?token=pw", None).await;
    assert_eq!(st, StatusCode::OK);
    let v1: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v1[0]["name"], "Faves");
    let r = call(&app, &format!("/rest/deletePlaylist?id={pl_id}&{auth}")).await;
    assert_eq!(r["status"], "ok");
    let r = call(&app, &format!("/rest/getPlaylists?{auth}")).await;
    assert!(r["playlists"]["playlist"].as_array().unwrap().is_empty());

    // Scrobble lands as a private activity post; untagged scrobbles are ignored quietly.
    call(
        &app,
        &format!("/rest/scrobble?id={get_lucky}&submission=true&{auth}"),
    )
    .await;
    call(
        &app,
        &format!("/rest/scrobble?id={instant_crush}&submission=true&{auth}"),
    )
    .await;
    let (st, _, body) = get(&app, "/v1/activity?token=pw", None).await;
    assert_eq!(st, StatusCode::OK);
    let acts: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(acts.as_array().unwrap().len(), 1);
    assert_eq!(acts[0]["track"]["title"], "Get Lucky");

    // Unknown method and unknown song.
    let r = call(&app, &format!("/rest/getVideos?{auth}")).await;
    assert_eq!(r["status"], "failed");
    let r = call(&app, &format!("/rest/getSong?id=999&{auth}")).await;
    assert_eq!(r["error"]["code"], 70);
}
