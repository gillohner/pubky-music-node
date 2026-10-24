//! Measured loudness (loudness.rs): files without ReplayGain tags are measured (EBU R128), the
//! measurement reaches the ReplayGain stage, tagged files are left alone, and a changed file is
//! measured again.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use id3::TagLike;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::{OutputRegistry, QueueItem, SourceFactory};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

/// A 1 kHz stereo sine at `amp` of full scale, 2 s, tagged with a title and album.
fn sine(path: &std::path::Path, amp: f64, title: &str, album: &str, rg: Option<&str>) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..44_100 * 2 {
        let s = (amp * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44_100.0).sin() * 32767.0) as i16;
        w.write_sample(s).unwrap();
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    t.set_title(title);
    t.set_artist("Tone");
    t.set_album(album);
    if let Some(g) = rg {
        t.add_frame(id3::frame::ExtendedText {
            description: "REPLAYGAIN_TRACK_GAIN".into(),
            value: g.into(),
        });
    }
    t.write_to_path(path, id3::Version::Id3v24).unwrap();
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn untagged_files_are_measured_and_feed_the_replaygain_stage() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let quiet = music.join("quiet.wav");
    sine(&quiet, 0.1, "Quiet", "Q", None);
    sine(&music.join("silent.wav"), 0.0, "Silent", "S", None);
    sine(&music.join("tagged.wav"), 0.5, "Tagged", "T", Some("-3.50 dB"));
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
    let app = router(s.clone());
    let scan = || async {
        call(&app, "POST", "/v1/library/scan").await;
        for _ in 0..1000 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    scan().await;
    let (_, found) = call(&app, "GET", "/v1/library/tracks?limit=10").await;
    let id = |title: &str| {
        found["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["title"] == title)
            .unwrap_or_else(|| panic!("{title}: {found}"))["id"]
            .as_i64()
            .unwrap()
    };

    // Two files have no ReplayGain; loudness is off everywhere
    let (st, v) = call(&app, "GET", "/v1/library/loudness").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        (v["on"].as_bool(), v["to_measure"].as_u64()),
        (Some(false), Some(2)),
        "{v}"
    );

    assert_eq!(pm_api::loudness::measure_pending(&s, 10).await, 2);
    let (_, v) = call(&app, "GET", "/v1/library/loudness").await;
    assert_eq!(
        (
            v["measured"].as_u64(),
            v["failed"].as_u64(),
            v["to_measure"].as_u64()
        ),
        (Some(1), Some(1), Some(0)),
        "the silent file can't be measured and isn't tried again: {v}"
    );

    // What the ReplayGain stage gets: the measured gain (a -20 dBFS sine: about +2 dB to
    // -18 LUFS), the album's from its one track; the tagged file keeps its tag
    let gain = |tid: i64| {
        let src = s.sources.open(&QueueItem::new(format!("track:{tid}"))).unwrap();
        src.replay_gain().unwrap()
    };
    let q = gain(id("Quiet"));
    let (t, a) = (q.track_db.unwrap(), q.album_db.unwrap());
    assert!((t - 2.0).abs() < 0.6, "{q:?}");
    assert!((a - t).abs() < 0.01, "{q:?}");
    assert_eq!(gain(id("Tagged")).track_db, Some(-3.5));
    assert_eq!(gain(id("Silent")).track_db, None);

    // The file changes (louder, a later mtime): measured again
    sine(&quiet, 0.5, "Quiet", "Q", None);
    let f = std::fs::File::options().write(true).open(&quiet).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
    drop(f);
    scan().await;
    let (_, v) = call(&app, "GET", "/v1/library/loudness").await;
    assert_eq!(v["to_measure"], 1, "{v}");
    assert_eq!(gain(id("Quiet")).track_db, None, "a stale measurement isn't used");
    pm_api::loudness::measure_pending(&s, 10).await;
    let louder = gain(id("Quiet")).track_db.unwrap();
    assert!(
        (louder - -12.0).abs() < 0.6,
        "a -6 dBFS sine reads about -6 LUFS: -12 dB: {louder}"
    );
}
