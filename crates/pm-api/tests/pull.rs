//! A zone playing into a pull output: the HTTP WAV stream is the exact PCM
//! of the source, and a listener is required.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::{OutputRegistry, ZoneId};
use tower::ServiceExt;

fn write_wav(path: &std::path::Path, frames: usize) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let mut pcm = Vec::new();
    for i in 0..frames * 2 {
        let s = ((i as i64 * 7919) % (1 << 24)) as i32 - (1 << 23);
        w.write_sample(s).unwrap();
        pcm.extend_from_slice(&s.to_le_bytes()[..3]);
    }
    w.finalize().unwrap();
    pcm
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_stream_is_the_exact_pcm() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let pcm = write_wav(&wav, 20_000);
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: None,
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            slimproto_port: 0,
            session_secret_path: None,
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    let app = router(s.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/outputs/pull")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"living"}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let zone = s.zones.get(&ZoneId::new("stream-living")).unwrap();
    assert_eq!(zone.output().id().0, "pull:living");

    // Enqueue the file and play; the sink waits for a listener.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/zones/stream-living/queue")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"items":[{{"uri":"{}"}}],"replace":true,"play":true}}"#,
            wav.display()
        )))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/outputs/pull/living/stream.wav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "audio/wav");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[0..4], b"RIFF");
    assert_eq!(&body[36..40], b"data");
    assert_eq!(&body[44..], &pcm[..], "stream PCM differs from the source");
    let st = zone.wait_stopped().await;
    assert_eq!(st.error, None);

    // Unknown stream and duplicate names.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/outputs/pull/nope/stream.wav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/outputs/pull")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"living"}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::CONFLICT
    );
}
