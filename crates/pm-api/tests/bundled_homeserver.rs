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
    node_with(dir, None)
}

/// A node whose API is behind `token` (None: open, everyone counts as the owner).
fn node_with(dir: &std::path::Path, token: Option<&str>) -> Arc<NodeState> {
    let db = dir.join("node.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(pm_zones::outputs::NullOutput::new()));
    let s = NodeState::new(
        NodeSettings {
            name: "bundle".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: token.map(str::to_string),
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
        outputs,
    );
    s.zones_for_all_outputs();
    s
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
    // At least the owner more (the other test signs people up at the same time)
    assert!(info["users"].as_u64().unwrap() > users_before, "{info}");
    // Signed in now: a second owner sign-up is refused
    let (st, _) = call(&app, "POST", "/v1/homeserver/bundled/owner-signup").await;
    assert_eq!(st, StatusCode::CONFLICT);
}

async fn call_json(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invites_become_people_with_quotas_and_access() {
    let Some(ep) = bundled() else {
        eprintln!("skipped: set PM_BUNDLED_HS_ADMIN and PM_BUNDLED_HS_PASSWORD");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    s.set_bundled_homeserver(ep);
    // With the homeserver's files folder, People shows what each account stores
    let files = std::env::var("PM_BUNDLED_HS_FILES").ok();
    if let Some(f) = &files {
        s.set_homeserver_files_dir(std::path::PathBuf::from(f));
    }
    let app = router(s.clone());
    let pubky = Pubky::new().unwrap();

    // Homeserver + music, 100 MB
    let (st, inv) = call_json(
        &app,
        "POST",
        "/v1/people/invites",
        serde_json::json!({"access": "homeserver_music", "name": "Sam", "storage_quota_mb": 100}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{inv}");
    let link = inv["url"].as_str().unwrap().to_string();
    assert!(link.starts_with("pubkyauth://direct_signup?hs="), "{link}");
    assert!(inv["qr_svg"].as_str().unwrap().starts_with("<svg"));
    let (_, people) = call(&app, "GET", "/v1/people").await;
    assert_eq!(people["invites"].as_array().unwrap().len(), 1);

    // "Pubky Ring" scans it: the account is created on the bundled homeserver
    let sam = pubky.signer(Keypair::random());
    sam.handle_deeplink(&link).await.expect("direct signup");
    let sam_key = sam.public_key().z32();
    let (_, people) = call(&app, "GET", "/v1/people").await;
    assert_eq!(people["invites"].as_array().unwrap().len(), 0, "{people}");
    let p = &people["people"][0];
    assert_eq!(
        (p["key"].as_str(), p["access"].as_str(), p["name"].as_str()),
        (Some(sam_key.as_str()), Some("homeserver_music"), Some("Sam"))
    );
    assert_eq!(p["storage_quota_mb"], 100);
    assert!(pm_api::people::may_use_music(&s, &sam_key));

    // Homeserver only: no music
    let (_, inv) = call_json(
        &app,
        "POST",
        "/v1/people/invites",
        serde_json::json!({"access": "homeserver"}),
    )
    .await;
    let kim = pubky.signer(Keypair::random());
    kim.handle_deeplink(inv["url"].as_str().unwrap()).await.unwrap();
    let kim_key = kim.public_key().z32();
    let (_, people) = call(&app, "GET", "/v1/people").await;
    assert_eq!(people["people"].as_array().unwrap().len(), 2);
    assert!(
        !pm_api::people::may_use_music(&s, &kim_key),
        "a homeserver-only account can't use the music service"
    );

    // Turning Sam's homeserver account off blocks writes; on again, they work
    let session = sam
        .signin(pubky::ClientId::new("pm-test").unwrap())
        .await
        .unwrap();
    let put = |body: &'static str| {
        let st = session.storage();
        async move { st.put("/pub/pm-test/hello.txt", body).await }
    };
    put("one").await.expect("writes while enabled");
    let (st, _) = call_json(
        &app,
        "PATCH",
        &format!("/v1/people/{sam_key}"),
        serde_json::json!({"homeserver_enabled": false}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(put("two").await.is_err(), "a disabled account can't write");
    let sam_in = |people: &serde_json::Value| {
        people["people"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == sam_key.as_str())
            .unwrap()
            .clone()
    };
    let (_, people) = call(&app, "GET", "/v1/people").await;
    assert_eq!(sam_in(&people)["homeserver_enabled"], false, "{people}");
    call_json(
        &app,
        "PATCH",
        &format!("/v1/people/{sam_key}"),
        serde_json::json!({"homeserver_enabled": true}),
    )
    .await;
    put("three").await.expect("writes again");
    let (_, people) = call(&app, "GET", "/v1/people").await;
    let p = sam_in(&people);
    assert_eq!(p["homeserver_enabled"], true, "{people}");
    if files.is_some() {
        assert_eq!(p["used_bytes"], 5, "Sam stores one 5-byte file: {people}");
    } else {
        assert!(p["used_bytes"].is_null());
    }

    // Quota and music access
    let (st, _) = call_json(
        &app,
        "PATCH",
        &format!("/v1/people/{sam_key}"),
        serde_json::json!({"storage_quota_mb": "unlimited", "music": false}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, people) = call(&app, "GET", "/v1/people").await;
    let p = people["people"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == sam_key.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (p["storage_quota_mb"].as_str(), p["music_enabled"].as_bool()),
        (Some("unlimited"), Some(false))
    );
    assert!(!pm_api::people::may_use_music(&s, &sam_key));

    // Music only, by key; bad keys refused
    let other = Keypair::random().public_key().z32();
    let (st, _) = call_json(
        &app,
        "POST",
        "/v1/people/invites",
        serde_json::json!({"access": "music", "key": format!("pubky{other}")}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(pm_api::people::may_use_music(&s, &other));
    let (st, _) = call_json(
        &app,
        "POST",
        "/v1/people/invites",
        serde_json::json!({"access": "music", "key": "nope"}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call_json(
        &app,
        "PATCH",
        &format!("/v1/people/{other}"),
        serde_json::json!({"storage_quota_mb": 5}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no quota for an account elsewhere");
}

async fn put_bytes(app: &axum::Router, uri: &str, body: Vec<u8>) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/octet-stream")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Signs a fresh owner up on the bundled homeserver, as the first test does.
async fn signed_up_owner(app: &axum::Router) -> String {
    let (_, started) = call(app, "POST", "/v1/homeserver/bundled/owner-signup").await;
    let url = started["url"].as_str().unwrap().to_string();
    let hs = param(&url, "hs").unwrap();
    let token = param(&url, "st").unwrap();
    let signer = Pubky::new().unwrap().signer(Keypair::random());
    signer
        .signup(&PublicKey::try_from(hs.as_str()).unwrap(), Some(&token))
        .await
        .unwrap();
    signer.approve_auth(&url).await.unwrap();
    for _ in 0..60 {
        let (_, v) = call(app, "GET", "/v1/pubky/login").await;
        if v["status"] == "signed_in" {
            return signer.public_key().z32();
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("the owner did not get signed in");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn music_added_to_the_homeserver_plays_from_its_own_folder() {
    let (Some(ep), Ok(files)) = (bundled(), std::env::var("PM_BUNDLED_HS_FILES")) else {
        eprintln!("skipped: set PM_BUNDLED_HS_ADMIN, PM_BUNDLED_HS_PASSWORD and PM_BUNDLED_HS_FILES");
        return;
    };
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../pm-audio/tests/fixtures");
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    s.set_bundled_homeserver(ep);
    s.set_homeserver_files_dir(std::path::PathBuf::from(&files));
    let app = router(s.clone());
    let owner = signed_up_owner(&app).await;

    // Upload one file in two chunks, with a wrong offset on the way
    let flac = std::fs::read(fixtures.join("60_mono_audio.flac")).unwrap();
    let (st, up) = call_json(&app, "POST", "/v1/library/uploads",
        serde_json::json!({"name": "Mono Song.flac", "size": flac.len(), "relative_path": "Tests/Mono Song.flac"})).await;
    assert_eq!(st, StatusCode::OK, "{up}");
    let id = up["id"].as_str().unwrap().to_string();
    let half = flac.len() / 2;
    let (st, v) = put_bytes(
        &app,
        &format!("/v1/library/uploads/{id}?offset=0"),
        flac[..half].to_vec(),
    )
    .await;
    assert_eq!((st, v["offset"].as_u64()), (StatusCode::OK, Some(half as u64)));
    let (st, v) = put_bytes(
        &app,
        &format!("/v1/library/uploads/{id}?offset=0"),
        flac[half..].to_vec(),
    )
    .await;
    assert_eq!(
        (st, v["offset"].as_u64()),
        (StatusCode::CONFLICT, Some(half as u64)),
        "resume from the node's offset"
    );
    let (st, _) = put_bytes(
        &app,
        &format!("/v1/library/uploads/{id}?offset={half}"),
        flac[half..].to_vec(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let mut stored = serde_json::Value::Null;
    for _ in 0..100 {
        let (_, v) = call(&app, "GET", &format!("/v1/library/uploads/{id}")).await;
        if v["state"] == "stored" || v["state"] == "failed" {
            stored = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(stored["state"], "stored", "{stored}");
    let remote = stored["stored_as"].as_str().unwrap();
    assert!(
        remote.starts_with("/priv/pubky.music/files/") && remote.ends_with("-Mono_Song.flac"),
        "{remote}"
    );
    // It is on the homeserver's disk, and that folder is now a library folder
    let on_disk = std::path::Path::new(&files)
        .join(&owner)
        .join(remote.trim_start_matches('/'));
    assert_eq!(
        std::fs::read(&on_disk).unwrap(),
        flac,
        "byte-identical in the homeserver's storage"
    );
    let (_, folders) = call(&app, "GET", "/v1/library/folders").await;
    assert!(folders.to_string().contains(&owner), "{folders}");

    // The same content again: nothing sent
    let hash = blake3::hash(&flac).to_hex().to_string();
    let (_, again) = call_json(
        &app,
        "POST",
        "/v1/library/uploads",
        serde_json::json!({"name": "Copy.flac", "size": flac.len(), "blake3": hash}),
    )
    .await;
    assert_eq!(again["already"], true, "{again}");

    // Copy a folder: the file already there is skipped, the new one sent
    let src = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures.join("60_mono_audio.flac"),
        src.path().join("01 Mono.flac"),
    )
    .unwrap();
    std::fs::copy(
        fixtures.join("14_wasted_bits.flac"),
        src.path().join("02 Wasted.flac"),
    )
    .unwrap();
    std::fs::write(src.path().join("cover.txt"), b"not audio").unwrap();
    let (_, dry) = call_json(
        &app,
        "POST",
        "/v1/library/imports",
        serde_json::json!({"path": src.path(), "dry_run": true}),
    )
    .await;
    assert_eq!(dry["total"], 2, "{dry}");
    let (st, job) = call_json(
        &app,
        "POST",
        "/v1/library/imports",
        serde_json::json!({"path": src.path()}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{job}");
    let jid = job["id"].as_str().unwrap().to_string();
    let mut done = serde_json::Value::Null;
    for _ in 0..150 {
        let (_, v) = call(&app, "GET", &format!("/v1/pubky/files/upload/{jid}")).await;
        if v["finished"] == true {
            done = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(
        (
            done["done"].as_u64(),
            done["skipped"].as_u64(),
            done["failed"].as_u64()
        ),
        (Some(1), Some(1), Some(0)),
        "{done}"
    );

    // The library scans the homeserver's folder: both songs, counted on the status line
    let mut st_v = serde_json::Value::Null;
    for _ in 0..100 {
        let (_, v) = call(&app, "GET", "/v1/storage").await;
        if v["music"]["on_homeserver"]["tracks"].as_u64() == Some(2) {
            st_v = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    if st_v.is_null() {
        let (_, v) = call(&app, "GET", "/v1/storage").await;
        let (_, f) = call(&app, "GET", "/v1/library/folders").await;
        let (_, t) = call(&app, "GET", "/v1/library/tracks").await;
        panic!(
            "status {v}\nfolders {f}\ntracks {}",
            t.to_string().chars().take(600).collect::<String>()
        );
    }
    assert_eq!(st_v["music"]["on_homeserver"]["tracks"], 2, "{st_v}");
    assert_eq!(st_v["homeserver"]["mode"], "bundled");
    assert!(st_v["homeserver"]["used_bytes"].as_u64().unwrap() >= flac.len() as u64);
    assert!(
        ["saved", "saving"].contains(&st_v["saving"]["state"].as_str().unwrap()),
        "{st_v}"
    );
}

async fn call_auth(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Ring signs a controller in: start the node's sign-in, approve it as `signer`, poll.
async fn ring_sign_in(app: &axum::Router, signer: &pubky::PubkySigner) -> serde_json::Value {
    let (st, started) = call(app, "POST", "/v1/auth/pubky/start").await;
    assert_eq!(st, StatusCode::OK, "{started}");
    approve_and_poll(app, signer, &started).await
}

/// Approves a started sign-in as `signer` and polls until it ends.
async fn approve_and_poll(
    app: &axum::Router,
    signer: &pubky::PubkySigner,
    started: &serde_json::Value,
) -> serde_json::Value {
    signer
        .approve_auth(started["url"].as_str().unwrap())
        .await
        .expect("approve");
    let id = started["id"].as_str().unwrap().to_string();
    for _ in 0..60 {
        let (_, v) = call(app, "GET", &format!("/v1/auth/pubky/poll?id={id}")).await;
        if v["status"] != "pending" {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("the sign-in never finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listener_signs_in_with_ring_and_plays_a_homeserver_only_account_cannot() {
    let Some(ep) = bundled() else {
        eprintln!("skipped: set PM_BUNDLED_HS_ADMIN and PM_BUNDLED_HS_PASSWORD");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../pm-audio/tests/fixtures");
    let music = dir.path().join("music");
    std::fs::create_dir_all(&music).unwrap();
    std::fs::copy(fixtures.join("60_mono_audio.flac"), music.join("song.flac")).unwrap();
    let s = node(dir.path());
    s.set_bundled_homeserver(ep);
    let app = router(s.clone());
    signed_up_owner(&app).await;
    s.library
        .lock()
        .unwrap()
        .add_folder(&music.canonicalize().unwrap())
        .unwrap();
    s.start_scan(vec![music.canonicalize().unwrap()]).unwrap();
    let pubky = Pubky::new().unwrap();

    // Invited with music: signs up on the homeserver, then in to the node, as a listener
    let (_, inv) = call_json(
        &app,
        "POST",
        "/v1/people/invites",
        serde_json::json!({"access": "homeserver_music", "name": "Sam"}),
    )
    .await;
    let sam = pubky.signer(Keypair::random());
    sam.handle_deeplink(inv["url"].as_str().unwrap()).await.unwrap();
    call(&app, "GET", "/v1/people").await; // the used invite becomes a person
    let v = ring_sign_in(&app, &sam).await;
    assert_eq!(
        (v["status"].as_str(), v["role"].as_str()),
        (Some("ok"), Some("listener")),
        "{v}"
    );
    let tok = v["token"].as_str().unwrap().to_string();
    // (This node has no API token, so every caller counts as the owner; the listener rules are
    // the unit test's. Here: Ring hands a listener a token, and a homeserver-only account none.)
    let (_, me) = call_auth(&app, "GET", "/v1/me", &tok, None).await;
    assert!(me["role"].is_string(), "{me}");
    // They play a song from the shared library
    let mut track = None;
    for _ in 0..50 {
        let (_, t) = call_auth(&app, "GET", "/v1/library/tracks", &tok, None).await;
        if let Some(id) = t["tracks"][0]["id"].as_i64() {
            track = Some(id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let (_, zones) = call_auth(&app, "GET", "/v1/zones", &tok, None).await;
    let zid = zones[0]["id"].as_str().unwrap().to_string();
    let (st, q) = call_auth(
        &app,
        "POST",
        &format!("/v1/zones/{zid}/queue"),
        &tok,
        Some(serde_json::json!({"track_ids": [track.expect("scanned")], "replace": true, "play": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{q}");

    // Homeserver only: the account exists, the music service says no
    let (_, inv) = call_json(
        &app,
        "POST",
        "/v1/people/invites",
        serde_json::json!({"access": "homeserver"}),
    )
    .await;
    let kim = pubky.signer(Keypair::random());
    kim.handle_deeplink(inv["url"].as_str().unwrap()).await.unwrap();
    call(&app, "GET", "/v1/people").await;
    let v = ring_sign_in(&app, &kim).await;
    assert_eq!(v["status"], "denied", "{v}");
    assert!(v["token"].is_null());
}

/// The owner signs the node up on its homeserver (with the API token), and keeps their signer.
async fn owner_with_signer(app: &axum::Router, api: &str) -> pubky::PubkySigner {
    let (_, started) = call_auth(app, "POST", "/v1/homeserver/bundled/owner-signup", api, None).await;
    let url = started["url"].as_str().unwrap().to_string();
    let hs = param(&url, "hs").unwrap();
    let token = param(&url, "st").unwrap();
    let signer = Pubky::new().unwrap().signer(Keypair::random());
    signer
        .signup(&PublicKey::try_from(hs.as_str()).unwrap(), Some(&token))
        .await
        .unwrap();
    signer.approve_auth(&url).await.unwrap();
    for _ in 0..60 {
        let (_, v) = call_auth(app, "GET", "/v1/pubky/login", api, None).await;
        if v["status"] == "signed_in" {
            return signer;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("the owner did not get signed in");
}

/// TASK-login-ux: the owner logs in with Pubky Ring and gets a browser session of their own,
/// which Log out ends (the API token stays); someone else is refused with both names; while the
/// node's session lacks folders, only a log in the owner's page asks to renew asks for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_logs_in_and_out_and_a_wrong_identity_is_named() {
    let Some(ep) = bundled() else {
        eprintln!("skipped: set PM_BUNDLED_HS_ADMIN and PM_BUNDLED_HS_PASSWORD");
        return;
    };
    const API: &str = "the-api-token";
    let dir = tempfile::tempdir().unwrap();
    let s = node_with(dir.path(), Some(API));
    s.set_bundled_homeserver(ep);
    let app = router(s.clone());
    let owner = owner_with_signer(&app, API).await;
    let owner_key = owner.public_key().z32();

    // Log in: a session of its own, not the API token
    let v = ring_sign_in(&app, &owner).await;
    assert_eq!(
        (v["status"].as_str(), v["role"].as_str()),
        (Some("ok"), Some("owner")),
        "{v}"
    );
    let session = v["token"].as_str().unwrap().to_string();
    assert!(session.starts_with("pmo_") && session != API, "{v}");
    let (st, me) = call_auth(&app, "GET", "/v1/me", &session, None).await;
    assert_eq!((st, me["role"].as_str()), (StatusCode::OK, Some("owner")), "{me}");
    // Log out ends it; the API token goes on working
    let (st, out) = call_auth(&app, "DELETE", "/v1/me/token", &session, None).await;
    assert_eq!(
        (st, out["ended"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{out}"
    );
    let (st, _) = call_auth(&app, "GET", "/v1/me", &session, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (_, out) = call_auth(&app, "DELETE", "/v1/me/token", API, None).await;
    assert_eq!(out["ended"], false, "{out}");
    let (st, _) = call_auth(&app, "GET", "/v1/me", API, None).await;
    assert_eq!(st, StatusCode::OK);

    // While the node has no identity loaded (its homeserver didn't answer at start), no browser
    // session is the owner; the API token is. Loaded again, the owner's session works again
    let v = ring_sign_in(&app, &owner).await;
    let session = v["token"].as_str().unwrap().to_string();
    let loaded = s.sync_handle().unwrap();
    s.set_sync(None);
    let (st, _) = call_auth(&app, "GET", "/v1/me", &session, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = call_auth(&app, "GET", "/v1/me", API, None).await;
    assert_eq!(st, StatusCode::OK);
    s.set_sync(Some(loaded));
    let (st, _) = call_auth(&app, "GET", "/v1/me", &session, None).await;
    assert_eq!(st, StatusCode::OK, "the owner's own session stays");

    // Another identity: refused, saying who approved and whose node it is
    let stranger = Pubky::new().unwrap().signer(Keypair::random());
    let (_, inv) = call_auth(
        &app,
        "POST",
        "/v1/people/invites",
        API,
        Some(serde_json::json!({"access": "homeserver"})),
    )
    .await;
    stranger
        .handle_deeplink(inv["url"].as_str().unwrap())
        .await
        .unwrap();
    // Not in People yet (the used invite becomes a person when People is read): "not the owner"
    let v = ring_sign_in(&app, &stranger).await;
    assert_eq!(
        (v["status"].as_str(), v["reason"].as_str()),
        (Some("denied"), Some("not_owner")),
        "{v}"
    );
    assert!(v["token"].is_null());
    assert_eq!(v["approver"]["id"], stranger.public_key().z32(), "{v}");
    assert_eq!(v["owner_face"]["id"], owner_key, "{v}");
    // In People with the homeserver only, no music: "ask for access"
    call_auth(&app, "GET", "/v1/people", API, None).await;
    let v = ring_sign_in(&app, &stranger).await;
    assert_eq!(
        (v["status"].as_str(), v["reason"].as_str()),
        (Some("denied"), Some("no_access")),
        "{v}"
    );

    // The node's session lacks folders (the homeserver refused one). A plain log in (anyone's,
    // a listener's QR scan too) still only reads: Ring never asks them for their homeserver
    s.sync_handle().unwrap().note_refusal();
    let (_, started) = call(&app, "POST", "/v1/auth/pubky/start").await;
    assert_eq!(started["renews"], false, "{started}");
    let caps = param(started["url"].as_str().unwrap(), "caps").unwrap();
    assert!(!caps.contains(":rw"), "{caps}");
    let v = approve_and_poll(&app, &owner, &started).await;
    assert_eq!(v["role"], "owner", "{v}");
    let (_, pk) = call_auth(&app, "GET", "/v1/pubky", API, None).await;
    assert_eq!(pk["needs_resign"], true, "nothing renewed: {pk}");
    // The owner's page asks for the renewal: the log in asks for the folders, and the owner's
    // approval renews the node's session
    let (st, started) = call_json(
        &app,
        "POST",
        "/v1/auth/pubky/start",
        serde_json::json!({"renew": true}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{started}");
    assert_eq!(started["renews"], true, "{started}");
    assert_eq!(started["owner_face"]["id"], owner_key, "{started}");
    let caps = param(started["url"].as_str().unwrap(), "caps").unwrap();
    assert!(caps.contains("/priv/pubky.music/vault/:rw"), "{caps}");
    let v = approve_and_poll(&app, &owner, &started).await;
    assert_eq!(v["role"], "owner", "{v}");
    let (_, pk) = call_auth(&app, "GET", "/v1/pubky", API, None).await;
    assert_eq!(pk["needs_resign"], false, "{pk}");
    assert_eq!(
        pk["user"].as_str().map(|u| u.trim_start_matches("pubky")),
        Some(owner_key.trim_start_matches("pubky")),
        "{pk}"
    );
}
