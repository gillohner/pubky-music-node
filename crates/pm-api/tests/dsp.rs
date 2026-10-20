//! Per-zone DSP settings: default, set, validation, reset, persistence.

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

fn state(dir: &std::path::Path) -> Arc<NodeState> {
    let db = dir.join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
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
        },
        Library::open(&db).unwrap(),
        outputs,
    );
    s.zones_for_all_outputs();
    s
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
async fn dsp_per_zone_round_trip_and_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let s = state(dir.path());
    let app = router(s.clone());
    let (st, d) = call(&app, "GET", "/v1/zones/null-output/dsp", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(d["eq"].as_array().unwrap().len(), 0);
    assert_eq!(d["resample"], true);

    let cfg = serde_json::json!({
        "eq": [{"kind": "peaking", "freq_hz": 120.0, "gain_db": -4.0, "q": 1.2},
               {"kind": "high_shelf", "freq_hz": 8000.0, "gain_db": 2.0}],
        "headroom_db": -3.0
    });
    let (st, d) = call(&app, "PUT", "/v1/zones/null-output/dsp", Some(cfg)).await;
    assert_eq!(st, StatusCode::OK, "{d}");
    assert_eq!(d["eq"][1]["q"].as_f64().unwrap(), std::f64::consts::FRAC_1_SQRT_2);
    assert_eq!(d["eq"][1]["enabled"], true);
    let (st, d) = call(&app, "GET", "/v1/zones/null-output/dsp", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(d["eq"].as_array().unwrap().len(), 2);
    assert_eq!(d["headroom_db"], -3.0);

    // Invalid bands and ranges are refused.
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/zones/null-output/dsp",
        Some(serde_json::json!({"eq": [{"kind": "peaking", "freq_hz": 50000.0}]})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/zones/null-output/dsp",
        Some(serde_json::json!({"volume_db": 5.0})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&app, "GET", "/v1/zones/nope/dsp", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Survives a restart: a fresh state over the same database restores it.
    drop(app);
    drop(s);
    let s2 = state(dir.path());
    let app2 = router(s2.clone());
    let (st, d) = call(&app2, "GET", "/v1/zones/null-output/dsp", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(d["eq"].as_array().unwrap().len(), 2);
    let (st, _) = call(&app2, "DELETE", "/v1/zones/null-output/dsp", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, d) = call(&app2, "GET", "/v1/zones/null-output/dsp", None).await;
    assert_eq!(d["eq"].as_array().unwrap().len(), 0);
}
