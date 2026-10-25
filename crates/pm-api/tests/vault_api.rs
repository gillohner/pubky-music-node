//! Settings › Accounts' vault routes (STEWARD-VAULT.md item 5) on a real pubky-homeserver: the
//! device list, registering the node, "Sign out this device" and "Sign out everywhere".
//!
//! Needs the homeserver's admin API in `PM_BUNDLED_HS_ADMIN` and `PM_BUNDLED_HS_PASSWORD` (CI's
//! `bundle` job); skipped otherwise. A test signer stands in for Pubky Ring.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::sync::PubkySync;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_pubky::homeserver::NodeHomeserver;
use pm_pubky::vault::Vault;
use pm_zones::OutputRegistry;
use pubky::{Keypair, Pubky, PublicKey};
use pubky_music_specs::vault::{ClientSecret, SignIn};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn admin() -> Option<(String, String)> {
    Some((
        std::env::var("PM_BUNDLED_HS_ADMIN").ok()?,
        std::env::var("PM_BUNDLED_HS_PASSWORD").ok()?,
    ))
}

async fn admin_call(a: &(String, String), method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
    let mut r = reqwest::Client::new()
        .request(method, format!("{}{path}", a.0))
        .header("X-Admin-Password", &a.1);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let text = r
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

fn node(dir: &std::path::Path) -> Arc<NodeState> {
    let db = dir.join("node.sqlite");
    NodeState::new(
        NodeSettings {
            name: "Living room node".into(),
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
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    )
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_device_list_and_signing_out_through_the_api() {
    let Some(a) = admin() else {
        eprintln!("PM_BUNDLED_HS_ADMIN / PM_BUNDLED_HS_PASSWORD not set: skipped");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let app = router(s.clone());

    // Not signed in yet
    assert_eq!(call(&app, "GET", "/v1/vault").await.0, StatusCode::CONFLICT);

    // A person, the node signed in (one grant) and their phone (another)
    let info = admin_call(&a, reqwest::Method::GET, "/info", None).await;
    let hs = PublicKey::try_from(info["public_key"].as_str().unwrap()).unwrap();
    let token = admin_call(
        &a,
        reqwest::Method::POST,
        "/generate_signup_token",
        Some(json!({"storage_quota_mb": "unlimited"})),
    )
    .await;
    let signer = Pubky::new().unwrap().signer(Keypair::random());
    signer.signup(&hs, Some(token.as_str().unwrap())).await.unwrap();
    let homeserver = NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap();
    let mut sessions = Vec::new();
    for id in ["node.pubky.music", "phone.pubky.music"] {
        let flow = homeserver.start_login(id).unwrap();
        signer.approve_auth(flow.url()).await.unwrap();
        sessions.push(flow.await_approval().await.unwrap());
    }
    let phone_session = sessions.pop().unwrap();
    *s.sync.lock().unwrap() = Some(Arc::new(PubkySync::from_session(
        NodeHomeserver::new(pm_pubky::config::Network::Mainnet).unwrap(),
        sessions.pop().unwrap(),
    )));

    // The node registers under its name; its key file is made once, 0600
    let (st, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["registered"], false);
    let (st, v) = call(&app, "POST", "/v1/vault/register").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["registered"], true);
    assert_eq!(v["devices"][0]["name"], "Living room node");
    assert_eq!(v["devices"][0]["this_node"], true);
    assert_eq!(v["devices"][0]["device"], "node");
    let node_key = v["this_device"].as_str().unwrap().to_string();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(dir.path().join("vault/client.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
    }

    // The phone joins and signs in to TIDAL for everyone; the node lists both and reads it
    let phone_key = ClientSecret::generate();
    let phone = Vault::new(&phone_session, &phone_key);
    phone.register("Phone").await.unwrap();
    let tidal: SignIn = serde_json::from_value(json!({"service": "tidal", "way": "app",
        "tokens": {"access_token": "never-shown", "refresh_token": "never-shown-either"}}))
    .unwrap();
    phone.write_shared("tidal", &tidal).await.unwrap();
    let (_, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(v["devices"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(v["sign_ins"][0]["service"], "tidal");
    assert_eq!(v["sign_ins"][0]["readable"], true);
    assert!(
        !v.to_string().contains("never-shown"),
        "the API shows a token: {v}"
    );

    // The node can't sign itself out; a bad key is refused
    assert_eq!(
        call(&app, "POST", &format!("/v1/vault/devices/{node_key}/sign-out"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, "POST", "/v1/vault/devices/not-a-key/sign-out").await.0,
        StatusCode::BAD_REQUEST
    );

    // Sign the phone out: TIDAL rotates past it
    let (st, r) = call(
        &app,
        "POST",
        &format!("/v1/vault/devices/{}/sign-out", phone.client_key().to_hex()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["rotated"], json!(["tidal"]));
    assert_eq!(r["refreshed_at_service"], json!([]));
    assert!(phone.read("tidal").await.is_err());
    let (_, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(v["devices"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["sign_ins"][0]["epoch"], 2);

    // Sign out everywhere
    let (st, r) = call(&app, "POST", "/v1/vault/sign-out-everywhere").await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["removed"], json!(["tidal"]));
    let (_, v) = call(&app, "GET", "/v1/vault").await;
    assert_eq!(v["sign_ins"], json!([]));
}
