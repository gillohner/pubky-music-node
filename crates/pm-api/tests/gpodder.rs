//! gPodder sync against a mock gPodder API v2 server that also serves the feeds.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[derive(Default)]
struct Mock {
    base: String,
    /// What the server answers for subscriptions, by "since".
    subs: HashMap<String, serde_json::Value>,
    /// What it answers for episode actions, by "since".
    actions: HashMap<String, serde_json::Value>,
    /// Bodies the node posted, as "<path> <json>".
    posted: Vec<String>,
}

type Shared = Arc<Mutex<Mock>>;

fn authorised(h: &HeaderMap) -> bool {
    use base64::Engine;
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("ann:secret")
    );
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(want.as_str())
}

fn feed_xml(base: &str, name: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" version="2.0"><channel>
<title>Show {name}</title>
<item><title>{name} one</title><guid>{name}-1</guid><itunes:duration>1000</itunes:duration>
<enclosure url="{base}/ep/{name}1.mp3" type="audio/mpeg"/></item>
</channel></rss>"#
    )
}

async fn mock(listener: tokio::net::TcpListener, shared: Shared) {
    async fn feed(State(m): State<Shared>, Path(name): Path<String>) -> impl IntoResponse {
        let base = m.lock().unwrap().base.clone();
        let name = name.trim_end_matches(".xml").to_string();
        ([("content-type", "application/rss+xml")], feed_xml(&base, &name))
    }
    async fn get_subs(
        State(m): State<Shared>,
        h: HeaderMap,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        if !authorised(&h) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let m = m.lock().unwrap();
        let v = m
            .subs
            .get(&q["since"])
            .cloned()
            .unwrap_or(serde_json::json!({"add": [], "remove": [], "timestamp": 0}));
        axum::Json(v).into_response()
    }
    async fn get_actions(
        State(m): State<Shared>,
        h: HeaderMap,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        if !authorised(&h) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let m = m.lock().unwrap();
        let v = m
            .actions
            .get(&q["since"])
            .cloned()
            .unwrap_or(serde_json::json!({"actions": [], "timestamp": 0}));
        axum::Json(v).into_response()
    }
    async fn post(
        State(m): State<Shared>,
        h: HeaderMap,
        uri: axum::http::Uri,
        body: String,
    ) -> axum::response::Response {
        if !authorised(&h) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        m.lock().unwrap().posted.push(format!("{} {body}", uri.path()));
        axum::Json(serde_json::json!({"timestamp": 150, "update_urls": []})).into_response()
    }
    let app = axum::Router::new()
        .route("/feeds/{name}", axum::routing::get(feed))
        .route("/api/2/devices/ann/pubky-music.json", axum::routing::post(post))
        .route(
            "/api/2/subscriptions/ann/pubky-music.json",
            axum::routing::get(get_subs).post(post),
        )
        .route(
            "/api/2/episodes/ann.json",
            axum::routing::get(get_actions).post(post),
        )
        .with_state(shared);
    axum::serve(listener, app).await.unwrap();
}

#[tokio::test]
async fn subscriptions_and_positions_sync_both_ways() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (fa, fb) = (format!("{base}/feeds/a.xml"), format!("{base}/feeds/b.xml"));
    let shared: Shared = Arc::new(Mutex::new(Mock {
        base: base.clone(),
        ..Mock::default()
    }));
    {
        let mut m = shared.lock().unwrap();
        // First sync: the phone subscribed to B and got 96% into its episode.
        m.subs.insert(
            "0".into(),
            serde_json::json!({"add": [fb], "remove": [], "timestamp": 100}),
        );
        m.actions.insert(
            "0".into(),
            serde_json::json!({"actions": [
            {"podcast": fb, "episode": format!("{base}/ep/b1.mp3"), "action": "play",
             "timestamp": "2026-10-01T10:00:00", "started": 0, "position": 960, "total": 1000},
            {"podcast": fb, "episode": format!("{base}/ep/b1.mp3"), "action": "download",
             "timestamp": "2026-10-01T09:00:00"}], "timestamp": 120}),
        );
        // Second sync: the phone dropped B.
        m.subs.insert(
            "150".into(),
            serde_json::json!({"add": [], "remove": [fb], "timestamp": 200}),
        );
    }
    tokio::spawn(mock(listener, shared.clone()));

    let dir = tempfile::tempdir().unwrap();
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

    // Subscribed on the node before there is an account.
    let (st, _) = call(
        &app,
        "POST",
        "/v1/podcasts/subscriptions",
        Some(serde_json::json!({"feed": fa})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, status) = call(&app, "GET", "/v1/podcasts/gpodder", None).await;
    assert_eq!(status["configured"], false);

    // A wrong password is refused and nothing is kept.
    let wrong = serde_json::json!({"server": base, "username": "ann", "password": "nope"});
    let (st, body) = call(&app, "PUT", "/v1/podcasts/gpodder", Some(wrong)).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.to_string().contains("refused"), "{body}");
    assert_eq!(
        call(&app, "GET", "/v1/podcasts/gpodder", None).await.1["configured"],
        false
    );

    // Sign in: the first sync.
    let right = serde_json::json!({"server": format!("{base}/"), "username": "ann", "password": "secret"});
    let (st, body) = call(&app, "PUT", "/v1/podcasts/gpodder", Some(right)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["status"]["configured"], true);
    assert_eq!(body["status"]["device"], "pubky-music");
    assert!(body["status"].get("password").is_none());
    assert_eq!(body["sync"]["added"], 1);
    assert_eq!(body["sync"]["sent_subscriptions"], 1, "A goes up, B came down");
    assert_eq!(
        body["sync"]["positions"], 1,
        "the download action is not a position"
    );
    let (_, subs) = call(&app, "GET", "/v1/podcasts/subscriptions", None).await;
    let mut urls: Vec<String> = subs["subscriptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["url"].as_str().unwrap().to_string())
        .collect();
    urls.sort();
    assert_eq!(urls, vec![fa.clone(), fb.clone()]);
    let b_id = pm_api::podcasts::feed_id(&fb);
    let (_, show) = call(&app, "GET", &format!("/v1/podcasts/subscriptions/{b_id}"), None).await;
    assert_eq!(show["items"][0]["position_ms"], 960_000);
    assert_eq!(show["items"][0]["played"], true);
    {
        let m = shared.lock().unwrap();
        assert!(
            m.posted[0].starts_with("/api/2/devices/ann/pubky-music.json "),
            "{:?}",
            m.posted
        );
        let (_, json) = m.posted[1].split_once(' ').unwrap();
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v, serde_json::json!({"add": [fa], "remove": []}));
        assert_eq!(
            m.posted.len(),
            2,
            "B's position came from the server and is not sent back"
        );
    }

    // On the node: half way into A's episode. On the phone: B dropped.
    let a_id = pm_api::podcasts::feed_id(&fa);
    let (_, show) = call(&app, "GET", &format!("/v1/podcasts/subscriptions/{a_id}"), None).await;
    let ep = show["items"][0]["id"].as_str().unwrap().to_string();
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/v1/podcasts/progress/{ep}"),
        Some(serde_json::json!({"position_ms": 500_000})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // Progress is kept in whole seconds; make sure it is newer than the first sync.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/v1/podcasts/progress/{ep}"),
        Some(serde_json::json!({"position_ms": 500_000})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, report) = call(&app, "POST", "/v1/podcasts/gpodder/sync", None).await;
    assert_eq!(st, StatusCode::OK, "{report}");
    assert_eq!(report["removed"], 1);
    assert_eq!(report["sent_positions"], 1);
    let (_, subs) = call(&app, "GET", "/v1/podcasts/subscriptions", None).await;
    assert_eq!(subs["subscriptions"].as_array().unwrap().len(), 1);
    {
        let m = shared.lock().unwrap();
        let last = m.posted.last().unwrap();
        assert!(last.starts_with("/api/2/episodes/ann.json "), "{last}");
        let v: serde_json::Value = serde_json::from_str(last.split_once(' ').unwrap().1).unwrap();
        assert_eq!(v[0]["podcast"], fa);
        assert_eq!(v[0]["episode"], format!("{base}/ep/a1.mp3"));
        assert_eq!(v[0]["action"], "play");
        assert_eq!(v[0]["position"], 500);
        assert_eq!(v[0]["total"], 1000);
        assert_eq!(v[0]["device"], "pubky-music");
    }

    // Forget the account: the subscriptions stay.
    let (st, status) = call(&app, "DELETE", "/v1/podcasts/gpodder", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(status["configured"], false);
    let (_, subs) = call(&app, "GET", "/v1/podcasts/subscriptions", None).await;
    assert_eq!(subs["subscriptions"].as_array().unwrap().len(), 1);
}
