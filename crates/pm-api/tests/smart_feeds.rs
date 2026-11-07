//! A smart playlist's music feed never stays on the homeserver with nothing pointing to it (review
//! of #159, L6): a delete that doesn't reach the homeserver is kept and tried again at the next
//! smart playlist list or write, and a playlist that can't be stored takes its new feed with it.
//! The bookkeeping without a homeserver; the deletes on a testnet (`PM_TESTNET=1`).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OWNER: &str = "owner-token";
/// The testnet's built-in homeserver.
const TESTNET_HOMESERVER: &str = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {OWNER}"))
        .header("content-type", "application/json")
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

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

fn gone(s: &NodeState) -> Vec<String> {
    s.library
        .lock()
        .unwrap()
        .get_setting(pm_api::smart::GONE_FEEDS_KEY)
        .unwrap()
        .map(|j| {
            serde_json::from_str::<Vec<Value>>(&j)
                .unwrap()
                .iter()
                .map(|f| f["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn store_with_feed(s: &NodeState, id: &str, feed: &str) {
    let p = json!({"id": id, "name": "Techno", "reach": "all", "tags": ["techno"],
        "feed": {"id": feed, "uri": format!("pubky://x/priv/music/v1/feeds/{feed}.json"), "public": false}});
    s.library
        .lock()
        .unwrap()
        .set_setting(&format!("{}{id}", pm_api::smart::KEY_PREFIX), &p.to_string())
        .unwrap();
}

/// Without a sign-in a feed can't be deleted: deleting its playlist keeps the feed to delete later
/// (once), and the list of feeds left is no smart playlist.
#[tokio::test]
async fn a_feed_that_cant_be_deleted_now_is_kept_to_delete_later() {
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let app = router(s.clone());
    store_with_feed(&s, "p1", "0035WKVKQDHSM");
    let (st, r) = call(&app, "DELETE", "/v1/smart/p1", None).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(gone(&s), ["0035WKVKQDHSM"], "kept to delete later");
    // Kept once, and still kept while there is no homeserver to delete it on
    store_with_feed(&s, "p2", "0035WKVKQDHSM");
    call(&app, "DELETE", "/v1/smart/p2", None).await;
    let (st, list) = call(&app, "GET", "/v1/smart", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list, json!([]), "the feeds left are no smart playlist");
    assert_eq!(gone(&s), ["0035WKVKQDHSM"]);
}

/// On a testnet: a feed left to delete goes at the next list, and a playlist that can't be stored
/// (the database refuses) leaves no feed behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feeds_left_behind_are_deleted_on_the_homeserver() {
    if std::env::var("PM_TESTNET").as_deref() != Ok("1") {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = pubky::Pubky::testnet().unwrap();
    let signer = pubky.signer(pubky::Keypair::random());
    let hs = pubky::PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(pubky::ClientId::new("pm-api-smart-feeds").unwrap())
        .await
        .unwrap();
    let z32 = session.public_key().to_string();
    let homeserver = pm_pubky::homeserver::NodeHomeserver::from_pubky(pubky.clone());
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    s.set_sync(Some(Arc::new(pm_api::sync::PubkySync::from_session(
        homeserver.clone(),
        session.clone(),
    ))));
    let app = router(s.clone());
    let on_homeserver = |id: String| {
        let (hs, z32, session) = (homeserver.clone(), z32.clone(), session.clone());
        async move {
            hs.get_bytes(
                &z32,
                &format!("/priv/music/v1/feeds/{id}.json"),
                pm_pubky::Visibility::Private,
                Some(&session),
            )
            .await
            .is_ok()
        }
    };

    // A feed whose delete failed earlier goes at the next list
    let left = "0035WKVKQDHSM";
    homeserver
        .write_object(
            &session,
            &format!("/priv/music/v1/feeds/{left}.json"),
            &json!({"name": "x"}),
        )
        .await
        .unwrap();
    s.library
        .lock()
        .unwrap()
        .set_setting(
            pm_api::smart::GONE_FEEDS_KEY,
            &json!([{"id": left, "uri": "pubky://x", "public": false}]).to_string(),
        )
        .unwrap();
    assert!(on_homeserver(left.into()).await);
    call(&app, "GET", "/v1/smart", None).await;
    for _ in 0..100 {
        if gone(&s).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(gone(&s).is_empty(), "the list of feeds left empties");
    assert!(
        !on_homeserver(left.into()).await,
        "the feed is gone from the homeserver"
    );

    // A new playlist the node can't store: its feed doesn't stay
    rusqlite::Connection::open(dir.path().join("lib.sqlite"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER no_smart BEFORE INSERT ON settings WHEN NEW.key LIKE 'smart.%'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END;",
        )
        .unwrap();
    let before = homeserver
        .list_tree(
            &z32,
            "/priv/music/v1/feeds/",
            pm_pubky::Visibility::Private,
            Some(&session),
            pm_pubky::homeserver::ListBounds::OWNER,
        )
        .await
        .map(|l| l.entries)
        .unwrap_or_default();
    let (st, e) = call(
        &app,
        "POST",
        "/v1/smart",
        Some(json!({"name": "Techno", "reach": "all", "tags": ["techno"]})),
    )
    .await;
    assert!(st.is_server_error(), "{st} {e}");
    let after = homeserver
        .list_tree(
            &z32,
            "/priv/music/v1/feeds/",
            pm_pubky::Visibility::Private,
            Some(&session),
            pm_pubky::homeserver::ListBounds::OWNER,
        )
        .await
        .map(|l| l.entries)
        .unwrap_or_default();
    assert_eq!(after, before, "no feed left behind");
    assert!(gone(&s).is_empty());
}
