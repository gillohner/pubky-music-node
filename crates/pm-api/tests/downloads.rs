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

/// Players seek by asking for ranges: a range of a file far bigger than memory comes back at once,
/// from the browser stream and from Subsonic's `stream`, because the file is read from the range's
/// start in bounded chunks, never whole (security review 2026-10-27, #8).
#[tokio::test]
async fn a_huge_file_streams_a_range_without_being_read_whole() {
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    // A sparse terabyte: no room on disk, and no allocator would give a whole read the memory
    let file = dir.path().join("04 Long.flac");
    let total: u64 = 1 << 40;
    let marker = b"the end of a very long song";
    {
        use std::io::{Seek, Write};
        let mut f = std::fs::File::create(&file).unwrap();
        f.set_len(total).unwrap();
        f.seek(std::io::SeekFrom::Start(total - marker.len() as u64))
            .unwrap();
        f.write_all(marker).unwrap();
    }
    let id = add_track(&s, &file, "flac");
    let app = router(s.clone());
    let range = format!("bytes={}-{}", total - marker.len() as u64, total - 1);
    let within = std::time::Duration::from_secs(30);
    for uri in [
        format!("/v1/library/tracks/{id}/file"),
        format!("/rest/stream?id={id}&u=pubky&p={OWNER}&v=1.16.1&c=test&f=json"),
    ] {
        let g = tokio::time::timeout(within, get(&app, OWNER, &uri, &[("range", &range)]))
            .await
            .expect("answered without reading the whole file");
        assert_eq!(
            g.status,
            StatusCode::PARTIAL_CONTENT,
            "{uri}: {}",
            String::from_utf8_lossy(&g.body)
        );
        assert_eq!(g.body, marker, "{uri}");
        assert_eq!(
            h(&g, "content-range"),
            format!("bytes {}-{}/{total}", total - marker.len() as u64, total - 1),
            "{uri}"
        );
        assert_eq!(h(&g, "accept-ranges"), "bytes", "{uri}");
    }
    let g = get(
        &app,
        OWNER,
        &format!("/v1/library/tracks/{id}/file"),
        &[("range", "bytes=0-9")],
    )
    .await;
    assert_eq!(
        (g.status, g.body.as_slice()),
        (StatusCode::PARTIAL_CONTENT, &[0u8; 10][..])
    );
    assert_eq!(h(&g, "content-disposition"), "inline; filename=\"04 Long.flac\"");
}

/// A song on a WebDAV share is fetched once into the download cache, in chunks, and every range
/// after that (a seek, Subsonic) is served from there: before, each request read the whole file
/// from the share into memory (#8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_share_file_is_fetched_once_and_seeks_come_from_the_cache() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let content: Vec<u8> = (0..400_000u32).map(|i| (i % 253) as u8).collect();
    let fetches = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (body, n) = (content.clone(), fetches.clone());
    // The share answers the whole file whatever the range: the node never needs it to
    let share = axum::Router::new().route(
        "/dav/{name}",
        axum::routing::get(move || {
            let (body, n) = (body.clone(), n.clone());
            async move {
                n.fetch_add(1, Ordering::SeqCst);
                body
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, share).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lib.sqlite");
    Library::open(&db)
        .unwrap()
        .set_setting(
            "webdav.shares",
            &serde_json::json!([{"id": "nas", "name": "NAS", "url": format!("{base}/dav/")}]).to_string(),
        )
        .unwrap();
    let s = node(dir.path());
    let id = add_track(&s, std::path::Path::new("webdav://nas/05 Far.flac"), "flac");
    s.library
        .lock()
        .unwrap()
        .set_setting(pm_api::downloads::CACHE_MB_SETTING, "64")
        .unwrap();
    let app = router(s.clone());
    let uri = format!("/v1/library/tracks/{id}/file");
    let full = get(&app, OWNER, &uri, &[]).await;
    assert_eq!(
        full.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&full.body)
    );
    assert_eq!(full.body, content);
    let seek = get(&app, OWNER, &uri, &[("range", "bytes=300000-300099")]).await;
    assert_eq!(seek.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(seek.body, content[300_000..300_100]);
    let sub = get(
        &app,
        OWNER,
        &format!("/rest/stream?id={id}&u=pubky&p={OWNER}&v=1.16.1&c=test&f=json"),
        &[("range", "bytes=10-19")],
    )
    .await;
    assert_eq!(
        (sub.status, sub.body.as_slice()),
        (StatusCode::PARTIAL_CONTENT, &content[10..20])
    );
    assert_eq!(fetches.load(Ordering::SeqCst), 1, "fetched from the share once");

    // A file bigger than the cache is refused, and nothing is left behind
    s.library
        .lock()
        .unwrap()
        .set_setting(pm_api::downloads::CACHE_MB_SETTING, "0")
        .unwrap();
    let other = add_track(&s, std::path::Path::new("webdav://nas/06 Big.flac"), "flac");
    let big = get(&app, OWNER, &format!("/v1/library/tracks/{other}/file"), &[]).await;
    assert_eq!(
        big.status,
        StatusCode::INSUFFICIENT_STORAGE,
        "{}",
        String::from_utf8_lossy(&big.body)
    );
    assert!(
        std::fs::read_dir(dir.path().join("downloads"))
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".part")),
        "no partial file left"
    );
}

/// Every transcode waits for one of the node's slots (one per CPU), so a burst of requests never
/// runs more ffmpegs than that at once (#9).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transcodes_run_at_most_one_per_cpu() {
    use std::os::unix::fs::PermissionsExt;
    let slots = pm_api::downloads::max_transcodes();
    // Bitrates 64 to 320 make the distinct transcodes; with more CPUs than that the cap can't show
    let n = slots + 3;
    if n > 257 {
        eprintln!("{slots} CPUs: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let file = dir.path().join("07 Song.flac");
    std::fs::write(&file, vec![7u8; 1000]).unwrap();
    let id = add_track(&s, &file, "flac");
    // A stand-in for ffmpeg that notes how many of it run at once
    let runs = dir.path().join("runs");
    std::fs::create_dir(&runs).unwrap();
    let fake = dir.path().join("fake-ffmpeg");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nD='{}'\ntouch \"$D/running.$$\"\nls \"$D\" | grep -c '^running' >> \"$D/../seen\"\nsleep 1\nfor last; do :; done\nhead -c 1000 /dev/zero > \"$last\"\nrm \"$D/running.$$\"\n",
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
    let all = (0..n).map(|i| {
        let app = app.clone();
        async move {
            get(
                &app,
                OWNER,
                &format!("/v1/library/tracks/{id}/download?format=mp3&bitrate={}", 64 + i),
                &[],
            )
            .await
            .status
        }
    });
    let statuses = futures_util::future::join_all(all).await;
    assert!(statuses.iter().all(|s| *s == StatusCode::OK), "{statuses:?}");
    let seen = std::fs::read_to_string(dir.path().join("seen")).unwrap();
    let counts: Vec<usize> = seen.lines().map(|l| l.trim().parse().unwrap()).collect();
    assert_eq!(counts.len(), n, "{seen}");
    let most = *counts.iter().max().unwrap();
    assert!(most <= slots, "{most} ffmpegs at once, {slots} slots");
}

/// A transcode that runs past its time limit is stopped: ffmpeg is killed, the answer is 504 and
/// nothing is left in the cache (#9).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_stuck_transcode_is_killed_at_its_time_limit() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let file = dir.path().join("08 Song.flac");
    std::fs::write(&file, vec![7u8; 1000]).unwrap();
    let id = add_track(&s, &file, "flac");
    let pid = dir.path().join("pid");
    let fake = dir.path().join("stuck-ffmpeg");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nfor last; do :; done\necho partial > \"$last\"\nexec sleep 60\n",
            pid.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    {
        let lib = s.library.lock().unwrap();
        lib.set_setting(pm_api::downloads::FFMPEG_SETTING, fake.to_str().unwrap())
            .unwrap();
        lib.set_setting(pm_api::downloads::TRANSCODE_TIMEOUT_SETTING, "1")
            .unwrap();
    }
    let app = router(s.clone());
    let g = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        get(
            &app,
            OWNER,
            &format!("/v1/library/tracks/{id}/download?format=opus"),
            &[],
        ),
    )
    .await
    .expect("stopped at its limit");
    assert_eq!(
        g.status,
        StatusCode::GATEWAY_TIMEOUT,
        "{}",
        String::from_utf8_lossy(&g.body)
    );
    assert!(String::from_utf8_lossy(&g.body).contains("original"));
    // ffmpeg is gone (or a zombie waiting to be reaped), and so is its partial file
    let pid = std::fs::read_to_string(&pid).unwrap().trim().to_string();
    let mut alive = true;
    for _ in 0..100 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if stat.is_empty() || stat.contains(") Z ") {
            alive = false;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!alive, "ffmpeg {pid} still runs");
    assert!(std::fs::read_dir(dir.path().join("downloads"))
        .unwrap()
        .flatten()
        .all(|e| !e.file_name().to_string_lossy().contains("opus")));
}

/// A big file from a slow share streams into the cache for longer than the node's usual 20 s
/// request limit: only a share that stops sending is given up on (review of #130).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_share_file_gets_through_past_twenty_seconds() {
    let chunks = 23u8;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    // One chunk a second: steady, never idle, longer than 20 s in all
    let share = axum::Router::new().route(
        "/dav/{name}",
        axum::routing::get(move || async move {
            let body = futures_util::stream::unfold(0u8, move |i| async move {
                if i == chunks {
                    return None;
                }
                if i > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                Some((Ok::<_, std::io::Error>(vec![i; 1000]), i + 1))
            });
            axum::body::Body::from_stream(body)
        }),
    );
    tokio::spawn(async move { axum::serve(listener, share).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lib.sqlite");
    Library::open(&db)
        .unwrap()
        .set_setting(
            "webdav.shares",
            &serde_json::json!([{"id": "slow", "name": "Slow", "url": format!("{base}/dav/")}]).to_string(),
        )
        .unwrap();
    let s = node(dir.path());
    let id = add_track(&s, std::path::Path::new("webdav://slow/09 Long.flac"), "flac");
    let app = router(s.clone());
    let g = get(&app, OWNER, &format!("/v1/library/tracks/{id}/file"), &[]).await;
    assert_eq!(g.status, StatusCode::OK, "{}", String::from_utf8_lossy(&g.body));
    let want: Vec<u8> = (0..chunks).flat_map(|i| vec![i; 1000]).collect();
    assert_eq!(g.body, want);
}

/// A client that goes away mid-transcode drops the request: ffmpeg is killed and its partial file
/// is removed at once (review of #130).
#[cfg(unix)]
#[tokio::test]
async fn a_dropped_transcode_leaves_no_partial_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let file = dir.path().join("10 Song.flac");
    std::fs::write(&file, vec![7u8; 1000]).unwrap();
    let id = add_track(&s, &file, "flac");
    let fake = dir.path().join("slow-ffmpeg");
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor last; do :; done\necho partial > \"$last\"\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.library
        .lock()
        .unwrap()
        .set_setting(pm_api::downloads::FFMPEG_SETTING, fake.to_str().unwrap())
        .unwrap();
    let app = router(s.clone());
    let parts = || {
        std::fs::read_dir(dir.path().join("downloads"))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
                    .count()
            })
            .unwrap_or(0)
    };
    let uri = format!("/v1/library/tracks/{id}/download?format=mp3");
    // Boxed, so dropping it drops the request itself
    let mut request = Box::pin(get(&app, OWNER, &uri, &[]));
    // Running: ffmpeg has started its partial file
    let mut started = false;
    for _ in 0..100 {
        if tokio::time::timeout(std::time::Duration::from_millis(50), &mut request)
            .await
            .is_ok()
        {
            panic!("the stuck transcode answered");
        }
        if parts() == 1 {
            started = true;
            break;
        }
    }
    assert!(started, "ffmpeg started");
    // The client goes away
    drop(request);
    assert_eq!(parts(), 0, "the partial file went with the request");
}
