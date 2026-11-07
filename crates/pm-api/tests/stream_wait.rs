//! A zone that plays a stream whose server never answers takes Stop and Next while the open
//! is still waiting, and a pass over several such items ends at the Stop. Its own process:
//! the stream wait is shortened for tests before the node's stream client is first built.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use std::sync::Arc;
use tower::ServiceExt;

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
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// A server that accepts every connection, reads the request and never answers; `opened`
/// counts the connections.
fn silent_server(opened: &'static std::sync::atomic::AtomicUsize) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/live", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::spawn(move || {
                use std::io::Read;
                let mut c = c;
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf);
                std::thread::sleep(std::time::Duration::from_secs(120));
            });
        }
    });
    url
}

async fn transport_is(app: &axum::Router, zid: &str, want: &str, within: std::time::Duration) -> bool {
    let t = std::time::Instant::now();
    while t.elapsed() < within {
        let (_, z) = call(app, "GET", &format!("/v1/zones/{zid}"), None).await;
        if z["transport"] == want {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zone_on_a_stream_that_never_answers_takes_stop_and_next() {
    pm_api::sources::STREAM_WAIT_MS_FOR_TESTS.store(1_000, std::sync::atomic::Ordering::Relaxed);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
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
    let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
    let zid = zones[0]["id"].as_str().unwrap().to_string();
    static OPENED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let opened = || OPENED.load(std::sync::atomic::Ordering::SeqCst);
    let url = silent_server(&OPENED);
    let zone = s.zones.get(&pm_zones::ZoneId(zid.clone())).unwrap();
    // Three streams that never answer: each open costs the (test-sized) wait
    let items: Vec<serde_json::Value> = (0..3)
        .map(|i| serde_json::json!({"uri": format!("{url}?{i}"), "title": format!("Dead {i}")}))
        .collect();
    let (st, r) = call(
        &app,
        "POST",
        &format!("/v1/zones/{zid}/queue"),
        Some(serde_json::json!({"items": items, "play": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    // Play, not waiting for it; Stop while the first open waits: no other stream is tried
    zone.play();
    let t = std::time::Instant::now();
    while opened() < 1 {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(3),
            "the first open starts"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    zone.stop();
    assert!(
        transport_is(&app, &zid, "stopped", std::time::Duration::from_millis(2_500)).await,
        "Stop is taken within one wait, not three"
    );
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    assert_eq!(opened(), 1, "only the stream being opened when Stop came");
    // Play again (from the item that failed), then Next while its open waits: the pass ends
    // there and the zone goes on from the next item; nothing answers, so it ends stopped with
    // the reason within a few waits rather than never
    zone.play();
    while opened() < 2 {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "the second open starts"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    zone.next();
    // Next skips the item that failed (never one nobody tried) and opens the last; that fails
    // too, and the zone stops with the reason
    let t = std::time::Instant::now();
    loop {
        let (_, z) = call(&app, "GET", &format!("/v1/zones/{zid}"), None).await;
        if z["error"]
            .as_str()
            .is_some_and(|e| e.contains("nothing in the queue could be played"))
        {
            assert_eq!(z["transport"], "stopped", "{z}");
            break;
        }
        assert!(
            t.elapsed() < std::time::Duration::from_secs(6),
            "the pass ends: {z}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // The first stream (Stop), the first again (Play), then the other two (Next): each once a pass
    assert_eq!(opened(), 4, "no stream tried twice in a pass");
}

/// A stream someone other than the owner queued goes through the guarded client, built with
/// the same wait (read when it is built). Here, in its own process, so the shortened wait never
/// reaches another test's client.
#[test]
fn a_guarded_stream_that_never_answers_fails_within_the_wait() {
    static OPENED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let url = silent_server(&OPENED);
    let here = pm_zones::FetchLimit {
        peer: Some("127.0.0.1".parse().unwrap()),
        lan: true,
    };
    pm_api::sources::STREAM_WAIT_MS_FOR_TESTS.store(400, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(pm_api::sources::open_http_stream_guarded(&url, here).is_err());
    });
    let failed = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("still waiting: no deadline");
    assert!(failed, "no answer is an error");
    assert!(
        t.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        t.elapsed()
    );
}
