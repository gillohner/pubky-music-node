//! The owner's account on the node's bundled homeserver, against a real pubky-homeserver.
//! Needs its admin API in `PM_BUNDLED_HS_ADMIN` (e.g. `http://127.0.0.1:6288`, as the bundle
//! publishes it) and `PM_BUNDLED_HS_PASSWORD`; skipped otherwise, so CI stays hermetic except in
//! the `bundle` job, which runs it against `docker compose up`. A test signer stands in for
//! Pubky Ring: it creates the account with the link's token and approves the node's access.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use pubky::{Keypair, Pubky, PublicKey};
use std::sync::Arc;
use tower::ServiceExt;

fn bundled() -> Option<pm_api::bundled::AdminEndpoint> {
    pm_api::bundled::AdminEndpoint::new(
        &std::env::var("PM_BUNDLED_HS_ADMIN").ok()?,
        &std::env::var("PM_BUNDLED_HS_PASSWORD").ok()?,
    )
}

fn node(dir: &std::path::Path) -> Arc<NodeState> {
    let db = dir.join("node.sqlite");
    NodeState::new(
        NodeSettings {
            name: "bundle".into(),
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
            session_secret_path: Some(dir.join("session")),
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    )
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    url::form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_signs_up_on_the_bundled_homeserver() {
    let Some(ep) = bundled() else {
        eprintln!("skipped: set PM_BUNDLED_HS_ADMIN and PM_BUNDLED_HS_PASSWORD");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    s.set_bundled_homeserver(ep);
    let app = router(s.clone());

    let (st, info) = call(&app, "GET", "/v1/homeserver/bundled").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(info["reachable"], true, "{info}");
    let hs = info["public_key"].as_str().unwrap().to_string();
    let users_before = info["users"].as_u64().unwrap();

    let (st, started) = call(&app, "POST", "/v1/homeserver/bundled/owner-signup").await;
    assert_eq!(st, StatusCode::OK, "{started}");
    let url = started["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("pubkyauth://signup"), "{url}");
    assert_eq!(param(&url, "hs").as_deref(), Some(hs.as_str()));
    let token = param(&url, "st").expect("a signup token in the link");
    assert!(started["qr_svg"].as_str().unwrap().starts_with("<svg"));
    // Asking again before anyone signed up reuses the same token
    let (_, again) = call(&app, "POST", "/v1/homeserver/bundled/owner-signup").await;
    assert_eq!(
        param(again["url"].as_str().unwrap(), "st").as_deref(),
        Some(token.as_str())
    );
    let url = again["url"].as_str().unwrap().to_string();
    let (_, pending) = call(&app, "GET", "/v1/pubky/login").await;
    assert_eq!(pending["status"], "pending", "{pending}");

    // "Pubky Ring": create the account with the token, then approve the node's access
    let signer = Pubky::new().unwrap().signer(Keypair::random());
    let owner = signer.public_key().z32();
    signer
        .signup(&PublicKey::try_from(hs.as_str()).unwrap(), Some(&token))
        .await
        .expect("sign-up on the bundled homeserver");
    signer.approve_auth(&url).await.expect("approve");

    let mut done = serde_json::Value::Null;
    for _ in 0..60 {
        let (_, v) = call(&app, "GET", "/v1/pubky/login").await;
        if v["status"] == "signed_in" {
            done = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert_eq!(done["status"], "signed_in", "{done}");
    // The node names users in the `pubky<z32>` display form
    assert_eq!(done["user"].as_str(), Some(format!("pubky{owner}").as_str()));
    let (_, info) = call(&app, "GET", "/v1/homeserver/bundled").await;
    assert_eq!(info["users"].as_u64(), Some(users_before + 1));
    // Signed in now: a second owner sign-up is refused
    let (st, _) = call(&app, "POST", "/v1/homeserver/bundled/owner-signup").await;
    assert_eq!(st, StatusCode::CONFLICT);
}
