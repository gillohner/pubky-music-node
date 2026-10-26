//! Downloads to phones and laptops (STEWARD-DOWNLOADS.md): the original with resumable ranges, a
//! transcode made once and served from the cache, the refusals, and a listener's download.
//! Transcodes run a stand-in for ffmpeg (a shell script), so the test needs no ffmpeg.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
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

fn add_track(s: &NodeState, path: &std::path::Path, container: &str) -> i64 {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let t: pm_model::Track = serde_json::from_value(serde_json::json!({
        "id": null, "title": "Song", "artists": [{"id": null, "name": "Artist", "mbid": null}], "album": null,
        "track_no": 1, "disc_no": null, "duration_ms": 1000,
        "file": {"path": path, "size_bytes": len, "mtime_unix": 1, "container": container,
                 "format": serde_json::to_value(pm_model::AudioFormat::pcm(44_100, 16, 2)).unwrap(), "duration_ms": 1000},
        "recording_mbid": null, "isrc": null, "acoustid": null, "canonical": null,
        "replaygain_track_gain_db": null, "replaygain_album_gain_db": null}))
    .unwrap();
    s.library.lock().unwrap().upsert_track(&t).unwrap().0
}

struct Got {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

async fn get(app: &axum::Router, token: &str, uri: &str, extra: &[(&str, &str)]) -> Got {
    let mut b = Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    let resp = app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    Got {
        status,
        headers,
        body,
    }
}

fn h<'a>(g: &'a Got, name: &str) -> &'a str {
    g.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

#[tokio::test]
async fn the_original_with_resumable_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let music = dir.path().join("Owner's music");
    std::fs::create_dir_all(&music).unwrap();
    let file = music.join("01 Song.flac");
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &content).unwrap();
    let id = add_track(&s, &file, "flac");
    let app = router(s.clone());
    let uri = format!("/v1/library/tracks/{id}/download");

    let full = get(&app, OWNER, &uri, &[]).await;
    assert_eq!(full.status, StatusCode::OK);
    assert_eq!(full.body, content);
    assert_eq!(h(&full, "content-type"), "audio/flac");
    assert_eq!(h(&full, "content-length"), content.len().to_string());
    assert_eq!(h(&full, "accept-ranges"), "bytes");
    assert_eq!(
        h(&full, "content-disposition"),
        "attachment; filename=\"01 Song.flac\""
    );
    let etag = h(&full, "etag").to_string();
    assert!(!etag.is_empty());

    // Resume: the rest from byte 200000, with If-Range on the same file
    let rest = get(
        &app,
        OWNER,
        &uri,
        &[("range", "bytes=200000-"), ("if-range", &etag)],
    )
    .await;
    assert_eq!(rest.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(rest.body, content[200_000..]);
    assert_eq!(
        h(&rest, "content-range"),
        format!("bytes 200000-299999/{}", content.len())
    );
    // The file changed meanwhile (another ETag): the whole file again
    let changed = get(
        &app,
        OWNER,
        &uri,
        &[("range", "bytes=200000-"), ("if-range", "\"old\"")],
    )
    .await;
    assert_eq!(
        (changed.status, changed.body.len()),
        (StatusCode::OK, content.len())
    );
    // Unchanged: 304; past the end: 416
    assert_eq!(
        get(&app, OWNER, &uri, &[("if-none-match", &etag)]).await.status,
        StatusCode::NOT_MODIFIED
    );
    let past = get(&app, OWNER, &uri, &[("range", "bytes=999999-")]).await;
    assert_eq!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(h(&past, "content-range"), format!("bytes */{}", content.len()));

    // A listener may download what it may play
    let roster = serde_json::json!({"people": [{"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster.to_string())
        .unwrap();
    let listener = pm_api::accounts::mint(&s, KEY).unwrap();
    let theirs = get(&app, &listener, &uri, &[("range", "bytes=0-9")]).await;
    assert_eq!(
        (theirs.status, theirs.body.as_slice()),
        (StatusCode::PARTIAL_CONTENT, &content[..10])
    );
    // ...and nobody without a token
    assert_eq!(
        get(&app, "nope", &uri, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );

    // Refusals
    assert_eq!(
        get(&app, OWNER, "/v1/library/tracks/999/download", &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, OWNER, &format!("{uri}?format=wav"), &[]).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&app, OWNER, &format!("{uri}?format=mp3&bitrate=999"), &[])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_transcode_is_made_once_and_served_from_the_cache() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let file = dir.path().join("02 Song.flac");
    std::fs::write(&file, vec![7u8; 50_000]).unwrap();
    let id = add_track(&s, &file, "flac");
    // A stand-in for ffmpeg: writes its arguments and 100 kB into the output (the last argument),
    // and counts its runs
    let runs = dir.path().join("runs");
    let fake = dir.path().join("fake-ffmpeg");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho run >> '{}'\nfor last; do :; done\n{{ echo \"$@\"; head -c 100000 /dev/zero; }} > \"$last\"\n",
            runs.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.library
        .lock()
        .unwrap()
        .set_setting(pm_api::downloads::FFMPEG_SETTING, fake.to_str().unwrap())
        .unwrap();
    let app = router(s.clone());
    let uri = format!("/v1/library/tracks/{id}/download?format=aac&bitrate=192");

    let a = get(&app, OWNER, &uri, &[]).await;
    assert_eq!(a.status, StatusCode::OK, "{}", String::from_utf8_lossy(&a.body));
    assert_eq!(h(&a, "content-type"), "audio/mp4");
    assert_eq!(
        h(&a, "content-disposition"),
        "attachment; filename=\"02 Song.m4a\""
    );
    let args =
        String::from_utf8_lossy(&a.body[..a.body.iter().position(|b| *b == b'\n').unwrap()]).to_string();
    assert!(
        args.contains("-c:a aac") && args.contains("-b:a 192k") && args.contains("-f ipod"),
        "{args}"
    );
    assert!(args.contains("02 Song.flac"), "{args}");
    // Again, and resumed: no second run
    let b = get(&app, OWNER, &uri, &[("range", "bytes=50000-")]).await;
    assert_eq!(b.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(b.body, a.body[50_000..]);
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);
    // Another bitrate is another file
    assert_eq!(
        get(
            &app,
            OWNER,
            &format!("/v1/library/tracks/{id}/download?format=mp3"),
            &[]
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 2);

    // ffmpeg failing: 502 with its words, nothing left in the cache
    let failing = dir.path().join("failing-ffmpeg");
    std::fs::write(
        &failing,
        "#!/bin/sh\necho 'Unknown encoder libopus' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.library
        .lock()
        .unwrap()
        .set_setting(pm_api::downloads::FFMPEG_SETTING, failing.to_str().unwrap())
        .unwrap();
    let f = get(
        &app,
        OWNER,
        &format!("/v1/library/tracks/{id}/download?format=opus"),
        &[],
    )
    .await;
    assert_eq!(f.status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&f.body).contains("Unknown encoder libopus"));
    let cache: Vec<_> = std::fs::read_dir(dir.path().join("downloads"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        cache.iter().all(|n| !n.to_string_lossy().contains("opus")),
        "{cache:?}"
    );

    // No ffmpeg at all: 501, and the original still downloads
    s.library
        .lock()
        .unwrap()
        .set_setting(pm_api::downloads::FFMPEG_SETTING, "/nonexistent/ffmpeg")
        .unwrap();
    let n = get(
        &app,
        OWNER,
        &format!("/v1/library/tracks/{id}/download?format=opus"),
        &[],
    )
    .await;
    assert_eq!(n.status, StatusCode::NOT_IMPLEMENTED);
    assert!(String::from_utf8_lossy(&n.body).contains("ffmpeg"));
    assert_eq!(
        get(&app, OWNER, &format!("/v1/library/tracks/{id}/download"), &[])
            .await
            .status,
        StatusCode::OK
    );
}

/// With the real ffmpeg (CI: `PM_REAL_FFMPEG=1` where ffmpeg is installed): a second of a sine wave
/// becomes MP3, AAC in M4A and Opus in Ogg, each with its container's magic bytes.
#[tokio::test]
async fn real_ffmpeg_makes_each_format() {
    if std::env::var("PM_REAL_FFMPEG").ok().as_deref() != Some("1") {
        eprintln!("PM_REAL_FFMPEG not set: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    // 1 s of 440 Hz, 16-bit stereo, 44.1 kHz
    let wav = dir.path().join("03 Tone.wav");
    let frames = 44_100u32;
    let mut b = Vec::new();
    let data_len = frames * 4;
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&44_100u32.to_le_bytes());
    b.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        let v = ((i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 12_000.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(&wav, &b).unwrap();
    let id = add_track(&s, &wav, "wav");
    let app = router(s.clone());
    for (format, mime, magic_at, magic) in [
        ("mp3", "audio/mpeg", None, &b""[..]),
        ("aac", "audio/mp4", Some(4), &b"ftyp"[..]),
        ("opus", "audio/ogg", Some(0), &b"OggS"[..]),
    ] {
        let g = get(
            &app,
            OWNER,
            &format!("/v1/library/tracks/{id}/download?format={format}&bitrate=128"),
            &[],
        )
        .await;
        assert_eq!(
            g.status,
            StatusCode::OK,
            "{format}: {}",
            String::from_utf8_lossy(&g.body)
        );
        assert_eq!(h(&g, "content-type"), mime, "{format}");
        assert!(
            g.body.len() > 1000 && g.body.len() < b.len(),
            "{format}: {} bytes",
            g.body.len()
        );
        match magic_at {
            Some(at) => assert_eq!(&g.body[at..at + magic.len()], magic, "{format}"),
            // MP3: an ID3 tag or a frame sync
            None => assert!(
                g.body.starts_with(b"ID3") || (g.body[0] == 0xFF && g.body[1] & 0xE0 == 0xE0),
                "{format}"
            ),
        }
    }
}
