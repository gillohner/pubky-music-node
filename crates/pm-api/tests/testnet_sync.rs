//! Playlist sync against a local Pubky testnet. Needs `pubky-testnet`
//! running on the default ports (`cargo install pubky-testnet && pubky-testnet`)
//! and `PM_TESTNET=1`; skipped otherwise, so CI stays hermetic.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::sync::PubkySync;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_pubky::homeserver::NodeHomeserver;
use pm_pubky::{v1, Visibility};
use pm_zones::OutputRegistry;
use pubky::{ClientId, Keypair, Pubky, PublicKey};
use std::sync::Arc;
use tower::ServiceExt;

/// The testnet's built-in homeserver.
const TESTNET_HOMESERVER: &str = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";

fn enabled() -> bool {
    std::env::var("PM_TESTNET").map(|v| v == "1").unwrap_or(false)
}

fn node(dir: &std::path::Path, name: &str) -> Arc<NodeState> {
    let db = dir.join(format!("{name}.sqlite"));
    NodeState::new(
        NodeSettings {
            name: name.into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: None,
            subsonic_user: "pubky".into(),
            // Public reads without a session resolve users on the testnet too
            pubky: pm_pubky::PmPubkyConfig {
                network: pm_pubky::config::Network::Testnet,
                ..Default::default()
            },
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

/// Bring the homeserver's files into `dest` as a job; wait for it and return the last progress.
async fn bring(app: &axum::Router, dest: &std::path::Path) -> serde_json::Value {
    let (st, job) = call(
        app,
        "POST",
        "/v1/pubky/files/mirror",
        Some(serde_json::json!({"dest": dest})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{job}");
    if job["finished"] == true {
        return job;
    }
    let id = job["id"].as_str().unwrap().to_string();
    for _ in 0..1200 {
        let (_, p) = call(app, "GET", &format!("/v1/pubky/files/jobs/{id}"), None).await;
        if p["finished"] == true {
            return p;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("bringing files did not finish");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playlists_round_trip_through_the_private_folder() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let keypair = Keypair::random();
    let signer = pubky.signer(keypair.clone());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let z32 = session.public_key().to_string();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());

    let dir = tempfile::tempdir().unwrap();
    // Node A creates a playlist and pushes it.
    let a = node(dir.path(), "a");
    let app_a = router(a.clone());
    let (st, pl) = call(
        &app_a,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({
            "name": "Testnet mix",
            "uris": ["https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862"]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{pl}");
    let id = pl["summary"]["id"].as_str().unwrap().to_string();
    let sync_a = Arc::new(PubkySync::from_session(homeserver.clone(), session));
    a.set_sync(Some(sync_a.clone()));
    let r = sync_a.sync(&a.library).await.unwrap();
    assert_eq!((r.pushed, r.pulled, r.deleted), (1, 0, 0), "{r:?}");
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let (_, list) = call(&app_a, "GET", "/v1/playlists", None).await;
    assert_eq!(list[0]["synced"], true);

    // The playlist is a private v1 collection (ADR 0014), nothing in the old folder or public.
    let heads = homeserver
        .v1_heads(&z32, v1::Root::Priv, v1::SOCIAL_NAMESPACE, Some(sync_a.session()))
        .await
        .unwrap();
    assert_eq!(heads.len(), 1, "{heads:?}");
    assert_eq!(heads[&id].path, format!("/priv/social/v1/posts/{id}/{id}.json"));
    let body: serde_json::Value = serde_json::from_slice(
        &homeserver
            .get_bytes(
                &z32,
                &heads[&id].path,
                Visibility::Private,
                Some(sync_a.session()),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["kind"], "collection");
    assert!(homeserver
        .read_music_posts(&z32, Visibility::Private, Some(sync_a.session()))
        .await
        .unwrap()
        .is_empty());
    assert!(homeserver
        .v1_heads(&z32, v1::Root::Pub, v1::SOCIAL_NAMESPACE, None)
        .await
        .unwrap()
        .is_empty());

    // An edit is a second version; the pull on A takes nothing back.
    let (st, _) = call(
        &app_a,
        "PUT",
        &format!("/v1/playlists/{id}"),
        Some(serde_json::json!({"name": "Testnet mix 2"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let r = sync_a.sync(&a.library).await.unwrap();
    assert_eq!((r.pushed, r.pulled), (1, 0), "{r:?}");
    let heads = homeserver
        .v1_heads(&z32, v1::Root::Priv, v1::SOCIAL_NAMESPACE, Some(sync_a.session()))
        .await
        .unwrap();
    assert_eq!(heads[&id].versions.len(), 2);

    // Node B (fresh database, same user) pulls it.
    let signer_b = pubky.signer(keypair.clone());
    let session_b = signer_b
        .signin_blocking(ClientId::new("pm-api-test-b").unwrap())
        .await
        .unwrap();
    let b = node(dir.path(), "b");
    let sync_b = Arc::new(PubkySync::from_session(homeserver.clone(), session_b));
    let r = sync_b.sync(&b.library).await.unwrap();
    assert_eq!((r.pushed, r.pulled, r.deleted), (0, 1, 0), "{r:?}");
    let app_b = router(b.clone());
    let (st, got) = call(&app_b, "GET", &format!("/v1/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got["summary"]["name"], "Testnet mix 2");

    // Deleting on A removes it remotely; B's next sync keeps its local copy (no tombstone pull yet).
    let (st, _) = call(&app_a, "DELETE", &format!("/v1/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let r = sync_a.sync(&a.library).await.unwrap();
    assert_eq!(r.deleted, 1, "{r:?}");
    assert!(homeserver
        .v1_heads(&z32, v1::Root::Priv, v1::SOCIAL_NAMESPACE, Some(sync_a.session()))
        .await
        .unwrap()
        .is_empty());

    // Private files: upload a WAV, list it, mirror it into node B's folder.
    let wav = dir.path().join("song.wav");
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for i in 0..800 {
            w.write_sample((i % 100) as i16).unwrap();
        }
        w.finalize().unwrap();
    }
    let bytes = std::fs::read(&wav).unwrap();
    let remote = format!(
        "{}{}",
        pm_pubky::homeserver::PRIVATE_FILES_DIR,
        pm_api::files::remote_name(&bytes, &wav)
    );
    sync_a.upload_private(&remote, bytes.clone()).await.unwrap();
    let listed = sync_a.list_private_files().await.unwrap();
    assert!(listed.iter().any(|f| f.path == remote), "{listed:?}");
    b.set_sync(Some(sync_b.clone()));
    let mirror = dir.path().join("mirror-b");
    let r = bring(&app_b, &mirror).await;
    assert_eq!(
        (r["done"].as_u64(), r["failed"].as_u64()),
        (Some(1), Some(0)),
        "{r}"
    );
    // Under its own name, without the hash prefix
    assert_eq!(std::fs::read(mirror.join("song.wav")).unwrap(), bytes);
    let (_, plan) = call(
        &app_b,
        "POST",
        "/v1/pubky/files/mirror",
        Some(serde_json::json!({"dest": mirror, "dry_run": true})),
    )
    .await;
    assert_eq!(
        (plan["total"].as_u64(), plan["skipped"].as_u64()),
        (Some(0), Some(1)),
        "{plan}"
    );
    // Public listing must not see it.
    assert!(pubky
        .public_storage()
        .list((&PublicKey::try_from(z32.as_str()).unwrap(), "/pub/pubky.music/"))
        .unwrap()
        .send()
        .await
        .map(|v| v.is_empty())
        .unwrap_or(true));
}

/// Tags on external links move from `/pub/pubky.app/tags/` (Nexus never
/// indexes those) to `/pub/pubky.music/tags/` with the same id and body;
/// tags on `pubky://` objects stay; new tags go straight to the new path;
/// the move is idempotent (steward fix, 2026-10-21).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tags_move_to_the_node_namespace() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    use pubky_app_specs::traits::HashId;
    let pubky = Pubky::testnet().unwrap();
    let keypair = Keypair::random();
    let signer = pubky.signer(keypair.clone());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-tags").unwrap())
        .await
        .unwrap();
    let z32 = session.public_key().to_string();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    // What an older node wrote: a tag on a TIDAL link and one on a pubky:// post.
    let external =
        pubky_app_specs::PubkyAppTag::new("https://tidal.com/album/199574241".into(), "lol".into());
    let internal = pubky_app_specs::PubkyAppTag::new(
        format!("pubky://{z32}/pub/pubky.app/posts/0034A0X7NJ52G"),
        "nice".into(),
    );
    let (ext_id, int_id) = (external.create_id(), internal.create_id());
    homeserver
        .write_object(&session, &format!("/pub/pubky.app/tags/{ext_id}"), &external)
        .await
        .unwrap();
    homeserver
        .write_object(&session, &format!("/pub/pubky.app/tags/{int_id}"), &internal)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path(), "tags");
    let app = router(n.clone());
    let (st, t) = call(
        &app,
        "PUT",
        "/v1/tags",
        Some(serde_json::json!({"uri": "https://tidal.com/album/199574241", "label": "music"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{t}");
    let sync = Arc::new(PubkySync::from_session(homeserver.clone(), session));
    n.set_sync(Some(sync.clone()));
    let r = sync.sync(&n.library).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let new: Vec<String> = homeserver
        .public_paths(&z32, "/pub/pubky.music/tags/")
        .await
        .unwrap();
    let old: Vec<String> = homeserver
        .public_paths(&z32, "/pub/pubky.app/tags/")
        .await
        .unwrap();
    assert!(
        new.contains(&format!("/pub/pubky.music/tags/{ext_id}")),
        "moved with its id: {new:?}"
    );
    assert_eq!(new.len(), 2, "the moved tag and the new one: {new:?}");
    assert_eq!(
        old,
        vec![format!("/pub/pubky.app/tags/{int_id}")],
        "a tag on a pubky:// post stays"
    );
    // Read back: both tags on the TIDAL link, from the new path.
    let tags = homeserver.read_music_tags(&z32).await.unwrap();
    assert_eq!(tags.len(), 2, "{tags:?}");
    assert!(tags.iter().all(|(p, _)| p.starts_with("/pub/pubky.music/tags/")));
    // Idempotent.
    let r2 = sync.sync(&n.library).await.unwrap();
    assert!(r2.errors.is_empty(), "{:?}", r2.errors);
    assert_eq!(
        homeserver
            .public_paths(&z32, "/pub/pubky.music/tags/")
            .await
            .unwrap()
            .len(),
        2
    );
}

/// The old private envelopes (ADR 0006) move to v1 on a node's first sync: a playlist becomes a
/// collection, a rated review a music review, a favourite a v1 bookmark. The old files stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_private_objects_move_to_v1_once() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    use pm_pubky::schemas::{MusicContent, MusicPost, Playlist, PlaylistItem, Review};
    let pubky = Pubky::testnet().unwrap();
    let keypair = Keypair::random();
    let signer = pubky.signer(keypair.clone());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let z32 = session.public_key().to_string();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let song = "https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862";

    // What a node wrote before step 3
    let mb = pm_model::MusicUri::parse(song).unwrap();
    let playlist = MusicPost::new(MusicContent::Playlist(Playlist::new(
        "Old mix",
        None,
        None,
        vec![PlaylistItem::new(&mb)],
    )));
    let pl_id = playlist.create_id();
    let review = MusicPost::new(MusicContent::Review(Review::new(&mb, Some(4), "Good")));
    let rv_id = review.create_id();
    for (id, post) in [(&pl_id, &playlist), (&rv_id, &review)] {
        homeserver
            .write_object(&session, &MusicPost::path(id, Visibility::Private), &post.post)
            .await
            .unwrap();
    }
    let fav = pm_pubky::Favourite::new(mb.clone());
    homeserver
        .write_object(
            &session,
            &pm_pubky::Favourite::path(&fav.id, Visibility::Private),
            &fav.to_bookmark(),
        )
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path(), "n");
    let sync = Arc::new(PubkySync::from_session(homeserver.clone(), session));
    let r = sync.sync(&n.library).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!((r.pulled, r.pushed), (3, 3), "{r:?}");

    let social = homeserver
        .v1_heads(&z32, v1::Root::Priv, v1::SOCIAL_NAMESPACE, Some(sync.session()))
        .await
        .unwrap();
    assert_eq!(
        social[&pl_id].path,
        format!("/priv/social/v1/posts/{pl_id}/{pl_id}.json")
    );
    let music = homeserver
        .v1_heads(&z32, v1::Root::Priv, "music", Some(sync.session()))
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &homeserver
            .get_bytes(
                &z32,
                &music[&rv_id].path,
                Visibility::Private,
                Some(sync.session()),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["kind"], "review");
    assert_eq!(body["embed"], song);
    let favs = homeserver.read_v1_favourites(sync.session()).await.unwrap();
    assert_eq!(favs.len(), 1);
    assert_eq!(favs[0].1, mb);
    // The old files are still there
    assert_eq!(
        homeserver
            .read_music_posts(&z32, Visibility::Private, Some(sync.session()))
            .await
            .unwrap()
            .len(),
        2
    );

    // Once only: the next sync moves nothing
    let r = sync.sync(&n.library).await.unwrap();
    assert_eq!((r.pulled, r.pushed, r.deleted), (0, 0, 0), "{r:?}");
}

/// A session approved before the node used its current folders (STEWARD-FIX-resign.md): the
/// status says so, a sync ends with the plain notice and keeps the queue, and a new approval
/// with every folder clears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_old_session_asks_to_sign_in_again() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let approve = |caps: &'static str| {
        let homeserver = homeserver.clone();
        let signer = signer.clone();
        async move {
            let flow = homeserver.start_login("pm-api-test").unwrap();
            let granted: pubky::Capabilities = caps.parse().unwrap();
            signer
                .approve_auth_with_capabilities(flow.url(), &granted)
                .await
                .unwrap();
            flow.await_approval().await.unwrap()
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path(), "old");
    let app = router(n.clone());
    let (st, _) = call(
        &app,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({
            "name": "Queued",
            "uris": ["https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862"]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // The approval the owner's node got before step 3
    let old = approve("/priv/pubky.music/:rw,/priv/pubky.app/:rw,/pub/pubky.app/:rw").await;
    n.set_sync(Some(Arc::new(PubkySync::from_session(homeserver.clone(), old))));
    let (_, pk) = call(&app, "GET", "/v1/pubky", None).await;
    assert_eq!(pk["needs_resign"], true, "{pk}");
    assert!(
        pk["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "/priv/social/v1/:rw"),
        "{pk}"
    );
    assert!(pk["notice"]
        .as_str()
        .unwrap()
        .starts_with("This node needs permission"));
    let (st, r) = call(&app, "POST", "/v1/pubky/sync", None).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let errors = r["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{r}");
    assert_eq!(errors[0], pm_api::sync::RESIGN_NOTICE);
    assert!(!r.to_string().contains("403"), "no raw 403: {r}");
    let (_, pk) = call(&app, "GET", "/v1/pubky", None).await;
    assert_eq!(pk["unsynced"], 1, "the queue stays: {pk}");

    // Sign in again, approving everything the node asks for
    let full = approve(pm_pubky::homeserver::LOGIN_CAPABILITIES).await;
    n.set_sync(Some(Arc::new(PubkySync::from_session(homeserver.clone(), full))));
    let (_, pk) = call(&app, "GET", "/v1/pubky", None).await;
    assert_eq!(pk["needs_resign"], false, "{pk}");
    assert!(pk.get("missing").is_none(), "{pk}");
    let (_, r) = call(&app, "POST", "/v1/pubky/sync", None).await;
    assert!(r["errors"].as_array().unwrap().is_empty(), "{r}");
    let (_, pk) = call(&app, "GET", "/v1/pubky", None).await;
    assert_eq!(pk["unsynced"], 0, "{pk}");
}

/// Sharing a playlist publishes the same version under /pub/social/v1/posts/ (plan_publish, no
/// slug); making it private again removes the public copy and keeps the private versions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_playlist_is_published_and_unpublished() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let z32 = session.public_key().to_string();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path(), "share");
    let app = router(n.clone());
    let sync = Arc::new(PubkySync::from_session(homeserver.clone(), session));
    n.set_sync(Some(sync.clone()));
    let (_, pl) = call(
        &app,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({
            "name": "For everyone",
            "uris": ["https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862"]
        })),
    )
    .await;
    let id = pl["summary"]["id"].as_str().unwrap().to_string();
    let (st, d) = call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}/visibility"),
        Some(serde_json::json!({"public": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{d}");
    assert_eq!(d["summary"]["visibility"], "public");
    let r = sync.sync(&n.library).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let heads = |root| {
        let homeserver = homeserver.clone();
        let z32 = z32.clone();
        let sync = sync.clone();
        async move {
            homeserver
                .v1_heads(&z32, root, v1::SOCIAL_NAMESPACE, Some(sync.session()))
                .await
                .unwrap()
        }
    };
    let public = heads(v1::Root::Pub).await;
    let private = heads(v1::Root::Priv).await;
    assert_eq!(public.len(), 1, "{public:?}");
    assert_eq!(public[&id].edit_id, private[&id].edit_id, "the same version");
    assert_eq!(
        public[&id].path,
        format!("/pub/social/v1/posts/{id}/{}.json", public[&id].edit_id)
    );
    // Anyone can read it: no session
    let body: serde_json::Value = serde_json::from_slice(
        &homeserver
            .get_bytes(&z32, &public[&id].path, Visibility::Public, None)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["kind"], "collection");

    // Private again
    call(
        &app,
        "PUT",
        &format!("/v1/playlists/{id}/visibility"),
        Some(serde_json::json!({"public": false})),
    )
    .await;
    let r = sync.sync(&n.library).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(heads(v1::Root::Pub).await.is_empty(), "the public copy is gone");
    assert_eq!(
        heads(v1::Root::Priv).await[&id].versions.len(),
        2,
        "private versions stay (shared, then private again)"
    );
}

/// Someone else's shared playlist: another node (not signed in) lists it, opens it with where
/// each song plays, and saves a private copy that keeps the pins and the order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_node_opens_and_copies_a_shared_playlist() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let z32 = session.public_key().to_string();
    let key = z32.strip_prefix("pubky").unwrap_or(&z32).to_string();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let dir = tempfile::tempdir().unwrap();

    // The author shares a playlist with one pinned song and its own order
    let a = node(dir.path(), "author");
    let app_a = router(a.clone());
    let sync = Arc::new(PubkySync::from_session(homeserver.clone(), session));
    a.set_sync(Some(sync.clone()));
    let (_, pl) = call(
        &app_a,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({
            "name": "Mixtape",
            "uris": [
                "https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862",
                "https://musicbrainz.org/isrc/USQX91300108"
            ],
            "sources": [{"service": "tidal", "uri": "https://tidal.com/track/20115557"}, null]
        })),
    )
    .await;
    let id = pl["summary"]["id"].as_str().unwrap().to_string();
    call(
        &app_a,
        "PUT",
        &format!("/v1/playlists/{id}/order"),
        Some(serde_json::json!({"order": ["tidal", "library"]})),
    )
    .await;
    call(
        &app_a,
        "PUT",
        &format!("/v1/playlists/{id}/visibility"),
        Some(serde_json::json!({"public": true})),
    )
    .await;
    let r = sync.sync(&a.library).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);

    // Another node, with no Pubky session at all
    let b = node(dir.path(), "listener");
    let app_b = router(b.clone());
    let link = format!("pubky://{key}/pub/social/v1/posts/{id}");
    let (st, t) = call(
        &app_b,
        "GET",
        &format!(
            "/v1/shared/resolve?link={}",
            link.replace(':', "%3A").replace('/', "%2F")
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{t}");
    assert_eq!(t["id"], id.as_str());
    let (st, list) = call(&app_b, "GET", &format!("/v1/shared/{key}/playlists"), None).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list[0]["name"], "Mixtape");
    assert_eq!(list[0]["items"], 2);
    let (st, one) = call(&app_b, "GET", &format!("/v1/shared/{key}/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{one}");
    assert_eq!(one["play_from"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(one["play_from"]["items"][0]["pin"]["service"], "tidal");
    assert_eq!(one["play_from"]["own_order"], true);
    let (st, copy) = call(
        &app_b,
        "POST",
        &format!("/v1/shared/{key}/playlists/{id}/copy"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{copy}");
    assert_eq!(copy["summary"]["visibility"], "private");
    assert_eq!(
        copy["playlist"]["items"][0]["source"]["uri"],
        "https://tidal.com/track/20115557"
    );
    assert_eq!(
        copy["playlist"]["source_order"],
        serde_json::json!(["tidal", "library", "spotify", "jamendo"])
    );
    // A private playlist is not readable
    call(
        &app_a,
        "PUT",
        &format!("/v1/playlists/{id}/visibility"),
        Some(serde_json::json!({"public": false})),
    )
    .await;
    sync.sync(&a.library).await.unwrap();
    let (st, _) = call(&app_b, "GET", &format!("/v1/shared/{key}/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Long playlists (owner, 2026-10-21): a 300-song playlist is a v1 collection like any other.
/// It syncs, edits, reads back on another node, publishes, and opens from a node with no session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_playlist_is_a_collection_too() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let z32 = session.public_key().to_string();
    let key = z32.strip_prefix("pubky").unwrap_or(&z32).to_string();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let dir = tempfile::tempdir().unwrap();
    let a = node(dir.path(), "long-a");
    let app_a = router(a.clone());
    let sync_a = Arc::new(PubkySync::from_session(homeserver.clone(), session));
    a.set_sync(Some(sync_a.clone()));
    let uris: Vec<String> = (0..300)
        .map(|i| format!("https://musicbrainz.org/isrc/USQX9130{i:04}"))
        .collect();
    let (st, pl) = call(
        &app_a,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({"name": "All of it", "uris": uris})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{pl}");
    let id = pl["summary"]["id"].as_str().unwrap().to_string();
    let r = sync_a.sync(&a.library).await.unwrap();
    assert!(r.errors.is_empty() && r.notes.is_empty(), "{r:?}");
    // An edit is a second version
    let mut more = uris.clone();
    more.push("https://musicbrainz.org/isrc/USQX91309999".into());
    call(
        &app_a,
        "PUT",
        &format!("/v1/playlists/{id}"),
        Some(serde_json::json!({"uris": more})),
    )
    .await;
    call(
        &app_a,
        "PUT",
        &format!("/v1/playlists/{id}/visibility"),
        Some(serde_json::json!({"public": true})),
    )
    .await;
    let r = sync_a.sync(&a.library).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let heads = homeserver
        .v1_heads(&z32, v1::Root::Priv, v1::SOCIAL_NAMESPACE, Some(sync_a.session()))
        .await
        .unwrap();
    assert_eq!(heads[&id].versions.len(), 2, "{heads:?}");

    // Node B with the same account reads all 301 songs back
    let session_b = signer
        .signin_blocking(ClientId::new("pm-api-test-b").unwrap())
        .await
        .unwrap();
    let b = node(dir.path(), "long-b");
    let app_b = router(b.clone());
    let sync_b = Arc::new(PubkySync::from_session(homeserver.clone(), session_b));
    b.set_sync(Some(sync_b.clone()));
    sync_b.sync(&b.library).await.unwrap();
    let (st, back) = call(&app_b, "GET", &format!("/v1/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{back}");
    assert_eq!(back["playlist"]["items"].as_array().unwrap().len(), 301);

    // And anyone can open the shared copy
    let c = node(dir.path(), "long-c");
    let app_c = router(c.clone());
    let (st, one) = call(&app_c, "GET", &format!("/v1/shared/{key}/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{one}");
    assert_eq!(one["summary"]["items"], 301);
}

/// Copying an album to the homeserver's private storage (bulk upload): a dry run says how much,
/// the job reports progress, and a second run sends nothing again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_album_is_copied_to_the_homeserver_once() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../pm-audio/tests/fixtures");
    for f in ["14_wasted_bits.flac", "60_mono_audio.flac"] {
        std::fs::copy(fixtures.join(f), music.join(f)).unwrap();
    }
    let n = node(dir.path(), "upload");
    let app = router(n.clone());
    n.set_sync(Some(Arc::new(PubkySync::from_session(
        homeserver.clone(),
        session,
    ))));
    let (st, r) = call(
        &app,
        "POST",
        "/v1/library/folders",
        Some(serde_json::json!({"path": music})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    let mut ids = Vec::new();
    for _ in 0..200 {
        let (_, t) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        ids = t["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_i64().unwrap())
            .collect();
        if ids.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(ids.len(), 2);
    let (_, plan) = call(
        &app,
        "POST",
        "/v1/pubky/files/upload",
        Some(serde_json::json!({"track_ids": ids, "dry_run": true})),
    )
    .await;
    assert_eq!(plan["total"], 2);
    assert!(plan["bytes_total"].as_u64().unwrap() > 1000);
    let run = |expect_done: u64, expect_skipped: u64| {
        let app = app.clone();
        let ids = ids.clone();
        async move {
            let (st, job) = call(
                &app,
                "POST",
                "/v1/pubky/files/upload",
                Some(serde_json::json!({"track_ids": ids})),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{job}");
            let id = job["id"].as_str().unwrap().to_string();
            let mut last = serde_json::Value::Null;
            for _ in 0..400 {
                let (_, p) = call(&app, "GET", &format!("/v1/pubky/files/upload/{id}"), None).await;
                last = p;
                if last["finished"] == true {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert_eq!(last["finished"], true, "{last}");
            assert_eq!(
                (
                    last["done"].as_u64(),
                    last["skipped"].as_u64(),
                    last["failed"].as_u64()
                ),
                (Some(expect_done), Some(expect_skipped), Some(0)),
                "{last}"
            );
            assert_eq!(last["bytes_done"], last["bytes_total"]);
        }
    };
    run(2, 0).await;
    run(0, 2).await;
    let (_, files) = call(&app, "GET", "/v1/pubky/files", None).await;
    assert_eq!(files.as_array().unwrap().len(), 2, "{files}");
    assert!(files[0]["path"]
        .as_str()
        .unwrap()
        .starts_with("/priv/pubky.music/files/"));
}

/// A big change goes out in bounded batches (steward next-3: no request storm): 450 favourites
/// take three passes of at most 200 writes, each marking what landed, and a later sync downloads
/// none of them again. A fresh node still pulls all 450.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_favourites_sync_in_batches() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path(), "n");
    {
        let lib = n.library.lock().unwrap();
        for i in 0..450u32 {
            let uri = pm_model::MusicUri::parse(&format!(
                "https://musicbrainz.org/recording/00000000-0000-4000-8000-{i:012}"
            ))
            .unwrap();
            lib.add_favourite(
                &pm_pubky::Favourite::id_for(&uri),
                &uri.to_url(),
                1_700_000_000_000_000 + i as i64,
            )
            .unwrap();
        }
    }
    let sync = Arc::new(PubkySync::from_session(homeserver.clone(), session));
    let mut passes = Vec::new();
    loop {
        let r = sync.sync(&n.library).await.unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.pushed <= pm_api::sync::WRITES_PER_PASS, "{r:?}");
        passes.push((r.pushed, r.pending));
        if r.pending == 0 {
            break;
        }
        assert!(passes.len() < 5, "{passes:?}");
    }
    assert_eq!(passes, vec![(200, 250), (200, 50), (50, 0)]);
    let r = sync.sync(&n.library).await.unwrap();
    assert_eq!((r.pushed, r.pulled, r.pending), (0, 0, 0), "{r:?}");

    let fresh = node(dir.path(), "fresh");
    let r = sync.sync(&fresh.library).await.unwrap();
    assert_eq!(r.pulled, 450, "{r:?}");
    assert_eq!(
        fresh.library.lock().unwrap().list_favourites().unwrap().len(),
        450
    );
}

/// One Pubky client for the node's lifetime (STEWARD-FIX-pubky-signin-speed.md): the node builds
/// one client however many calls use it, and a first sync of many objects fetches them in
/// batches. Prints how long the first sync of 120 favourites and 40 playlists took.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_client_and_a_batched_first_sync() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let secret = pm_pubky::homeserver::export_session_secret(&session)
        .await
        .unwrap();

    // A first node writes what the owner has on their homeserver
    let dir = tempfile::tempdir().unwrap();
    let a = node(dir.path(), "a");
    {
        let lib = a.library.lock().unwrap();
        for i in 0..120u32 {
            let uri = pm_model::MusicUri::parse(&format!(
                "https://musicbrainz.org/recording/00000000-0000-4000-8000-{i:012}"
            ))
            .unwrap();
            lib.add_favourite(
                &pm_pubky::Favourite::id_for(&uri),
                &uri.to_url(),
                1_700_000_000_000_000 + i as i64,
            )
            .unwrap();
        }
    }
    let app_a = router(a.clone());
    for i in 0..40u32 {
        let (st, _) = call(
            &app_a,
            "POST",
            "/v1/playlists",
            Some(serde_json::json!({"name": format!("Mix {i}"), "uris": ["https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"]})),
        )
        .await;
        assert!(st.is_success());
    }
    let sa = Arc::new(PubkySync::from_session(
        NodeHomeserver::from_pubky(pubky.clone()),
        session,
    ));
    loop {
        let r = sa.sync(&a.library).await.unwrap();
        if r.pending == 0 {
            break;
        }
    }

    // A fresh node signs in with that session: one client, however many calls
    let b = node(dir.path(), "b");
    let before = pm_pubky::homeserver::clients_built();
    for _ in 0..10 {
        b.homeserver().unwrap();
    }
    let sync = Arc::new(
        PubkySync::connect_with(b.homeserver().unwrap(), &secret)
            .await
            .unwrap(),
    );
    let started = std::time::Instant::now();
    let r = sync.sync(&b.library).await.unwrap();
    let ms = started.elapsed().as_millis();
    let (st, _) = call(
        &router(b.clone()),
        "GET",
        &format!("/v1/shared/{}/playlists", sync.z32()),
        None,
    )
    .await;
    assert!(st.is_success() || st == StatusCode::NOT_FOUND, "{st}");
    assert_eq!(
        pm_pubky::homeserver::clients_built() - before,
        1,
        "one Pubky client for the node"
    );
    assert!(r.pulled >= 160, "{r:?}");
    eprintln!(
        "first sync of 120 favourites and 40 playlists: {ms} ms ({} pulled)",
        r.pulled
    );
}

/// A copy to the homeserver must not hold up the rest of the node (owner, 2026-10-22: an artist
/// page stayed on skeletons and a library FLAC did not play in the browser while 1.1 GB were
/// copied). While twenty 20 MB files go up, other calls answer quickly and a library file streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_job_does_not_hold_up_the_node() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    for i in 0..20 {
        // About 20 MB each: two minutes of 44.1 kHz stereo, noise so nothing compresses it away
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(music.join(format!("{i:02} big.wav")), spec).unwrap();
        let mut x: u32 = 12345 + i;
        for _ in 0..(44_100 * 2 * 118) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            w.write_sample((x & 0xffff) as u16 as i16).unwrap();
        }
        w.finalize().unwrap();
    }
    let mut settings_node = node(dir.path(), "n");
    let _ = &mut settings_node;
    let n = settings_node;
    let app = router(n.clone());
    let (st, _) = call(
        &app,
        "POST",
        "/v1/library/folders",
        Some(serde_json::json!({"path": music})),
    )
    .await;
    assert!(st.is_success(), "{st}");
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !n.is_scanning() {
            break;
        }
    }
    n.set_sync(Some(Arc::new(PubkySync::from_session(
        NodeHomeserver::from_pubky(pubky.clone()),
        session,
    ))));
    // Paced (the default is 2.5 MB/s): here 40 MB/s so the test stays short
    let (st, _) = call(
        &app,
        "PUT",
        "/v1/node/settings",
        Some(serde_json::json!({"copy_rate": 40_000_000})),
    )
    .await;
    assert!(st.is_success());
    let copy_started = std::time::Instant::now();
    let (st, job) = call(
        &app,
        "POST",
        "/v1/pubky/files/upload",
        Some(serde_json::json!({"all": true})),
    )
    .await;
    assert!(st.is_success(), "{st}: {job}");
    let id = job["id"].as_str().unwrap().to_string();

    // While it runs: the API and a library file answer quickly
    let mut slowest = 0u128;
    let mut rounds = 0;
    loop {
        let (_, p) = call(&app, "GET", &format!("/v1/pubky/files/upload/{id}"), None).await;
        if p["finished"].as_bool().unwrap_or(false) {
            break;
        }
        for path in [
            "/v1/info",
            "/v1/library/home",
            "/v1/library/albums?limit=60",
            "/v1/library/tracks?limit=50",
            "/v1/health",
        ] {
            let t = std::time::Instant::now();
            let (st, _) = call(&app, "GET", path, None).await;
            assert!(st.is_success(), "{path}: {st}");
            slowest = slowest.max(t.elapsed().as_millis());
        }
        // The browser player's request: the first megabyte of a library file
        let t = std::time::Instant::now();
        let req = Request::builder()
            .uri("/v1/library/tracks/1/file")
            .header("range", "bytes=0-1048575")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(!body.is_empty());
        slowest = slowest.max(t.elapsed().as_millis());
        rounds += 1;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let (_, p) = call(&app, "GET", &format!("/v1/pubky/files/upload/{id}"), None).await;
    let secs = copy_started.elapsed().as_secs_f64();
    eprintln!("copy: {} done, {} failed in {secs:.1} s; {rounds} rounds of calls while it ran, slowest {slowest} ms", p["done"], p["failed"]);
    assert!(
        secs >= 9.0,
        "400 MB at 40 MB/s takes at least 10 s, not {secs:.1} s: the pace was not kept"
    );
    assert!(rounds > 0, "the copy ended before anything was measured");
    assert!(slowest < 1000, "a call waited {slowest} ms during the copy");
}

/// Steward fix-mirror: a paced (streamed) upload lands whole, and the listing reports its size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paced_upload_lands_whole() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let signer = pubky.signer(Keypair::random());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let sync = PubkySync::from_session(NodeHomeserver::from_pubky(pubky.clone()), session);
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let path = "/priv/pubky.music/files/0123456789abcdef-paced.flac";
    sync.upload_private_paced(path, bytes.clone(), Some(1_000_000))
        .await
        .unwrap();
    let path2 = "/priv/pubky.music/files/0123456789abcdee-plain.flac";
    sync.upload_private(path2, bytes.clone()).await.unwrap();
    let files = sync.list_private_files().await.unwrap();
    eprintln!("{files:?}");
    let back = sync.download_private(path).await.unwrap();
    assert_eq!(
        back.len(),
        bytes.len(),
        "the paced upload came back {} bytes",
        back.len()
    );
    for f in &files {
        assert_eq!(f.size, Some(bytes.len() as u64), "{f:?}");
    }
}

/// Steward fix-mirror: "Bring files here" is a paced background job that only downloads what the
/// node lacks (matched by content hash), into a library folder under the files' own names; empty
/// and doubled-name uploads are left out and can be removed; copying brought files back never
/// doubles the name prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bringing_files_here_skips_what_the_node_has() {
    if !enabled() {
        eprintln!("PM_TESTNET not set; skipping");
        return;
    }
    let pubky = Pubky::testnet().unwrap();
    let keypair = Keypair::random();
    let signer = pubky.signer(keypair.clone());
    let hs = PublicKey::try_from(TESTNET_HOMESERVER).unwrap();
    signer.signup(&hs, None).await.unwrap();
    let session = signer
        .signin_blocking(ClientId::new("pm-api-test").unwrap())
        .await
        .unwrap();
    let homeserver = NodeHomeserver::from_pubky(pubky.clone());
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music-a");
    std::fs::create_dir(&music).unwrap();
    for i in 0..100u32 {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(music.join(format!("{i:03} song.wav")), spec).unwrap();
        for k in 0..4000u32 {
            w.write_sample(((k * (i + 3)) % 2000) as i16).unwrap();
        }
        w.finalize().unwrap();
    }
    let wait_scan = |n: Arc<NodeState>| async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        for _ in 0..600 {
            if !n.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    let wait_job = |app: axum::Router, id: String| async move {
        for _ in 0..2400 {
            let (_, p) = call(&app, "GET", &format!("/v1/pubky/files/jobs/{id}"), None).await;
            if p["finished"] == true {
                return p;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the job did not finish");
    };

    // Node A copies its library up
    let a = node(dir.path(), "a");
    let app_a = router(a.clone());
    call(
        &app_a,
        "POST",
        "/v1/library/folders",
        Some(serde_json::json!({"path": music})),
    )
    .await;
    wait_scan(a.clone()).await;
    let sync_a = Arc::new(PubkySync::from_session(homeserver.clone(), session));
    a.set_sync(Some(sync_a.clone()));
    let (st, job) = call(
        &app_a,
        "POST",
        "/v1/pubky/files/upload",
        Some(serde_json::json!({"all": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{job}");
    let done = wait_job(app_a.clone(), job["id"].as_str().unwrap().into()).await;
    assert_eq!(
        (done["done"].as_u64(), done["failed"].as_u64()),
        (Some(100), Some(0)),
        "{done}"
    );

    // A broken upload and a doubled name, as the NAS had
    let listed = sync_a.list_private_files().await.unwrap();
    let one = listed[0].name().to_string();
    let (h, _) = pm_api::files::split_remote_name(&one).unwrap();
    let doubled = format!("{}{h}-{one}", pm_pubky::homeserver::PRIVATE_FILES_DIR);
    let bytes = sync_a.download_private(&listed[0].path).await.unwrap();
    sync_a.upload_private(&doubled, bytes).await.unwrap();
    let empty = format!(
        "{}0000000000000000-empty.wav",
        pm_pubky::homeserver::PRIVATE_FILES_DIR
    );
    sync_a.upload_private(&empty, Vec::new()).await.unwrap();

    // Node A has every file already: nothing to bring
    let (_, plan) = call(
        &app_a,
        "POST",
        "/v1/pubky/files/mirror",
        Some(serde_json::json!({"dry_run": true})),
    )
    .await;
    assert_eq!(
        (
            plan["total"].as_u64(),
            plan["skipped"].as_u64(),
            plan["broken"].as_u64()
        ),
        (Some(0), Some(100), Some(2)),
        "{plan}"
    );
    assert!(
        plan["dest"]
            .as_str()
            .unwrap()
            .ends_with("music-a/From homeserver"),
        "{plan}"
    );
    let (_, files) = call(
        &app_a,
        "DELETE",
        "/v1/pubky/files",
        Some(serde_json::json!({"paths": [doubled, empty]})),
    )
    .await;
    assert_eq!(files.as_array().unwrap().len(), 100, "{files}");

    // Node B (same account, empty library folder) brings all 100 as a paced job with progress
    let signer_b = pubky.signer(keypair.clone());
    let session_b = signer_b
        .signin_blocking(ClientId::new("pm-api-test-b").unwrap())
        .await
        .unwrap();
    let b = node(dir.path(), "b");
    let app_b = router(b.clone());
    let music_b = dir.path().join("music-b");
    std::fs::create_dir(&music_b).unwrap();
    call(
        &app_b,
        "POST",
        "/v1/library/folders",
        Some(serde_json::json!({"path": music_b})),
    )
    .await;
    wait_scan(b.clone()).await;
    b.set_sync(Some(Arc::new(PubkySync::from_session(
        homeserver.clone(),
        session_b,
    ))));
    // 100 × 8 KB at 400 KB/s: about two seconds
    call(
        &app_b,
        "PUT",
        "/v1/node/settings",
        Some(serde_json::json!({"copy_rate": 400_000})),
    )
    .await;
    let (_, plan) = call(
        &app_b,
        "POST",
        "/v1/pubky/files/mirror",
        Some(serde_json::json!({"dry_run": true})),
    )
    .await;
    assert_eq!(
        (plan["total"].as_u64(), plan["skipped"].as_u64()),
        (Some(100), Some(0)),
        "{plan}"
    );
    let started = std::time::Instant::now();
    let (st, job) = call(
        &app_b,
        "POST",
        "/v1/pubky/files/mirror",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{job}");
    assert!(
        started.elapsed().as_millis() < 3000,
        "the request waited for the downloads"
    );
    let id = job["id"].as_str().unwrap().to_string();
    let mut midway = false;
    let last = loop {
        let (_, p) = call(&app_b, "GET", "/v1/pubky/files/jobs", None).await;
        if p.is_null() {
            let (_, p) = call(&app_b, "GET", &format!("/v1/pubky/files/jobs/{id}"), None).await;
            break p;
        }
        let d = p["done"].as_u64().unwrap();
        if d > 0 && d < 100 && p["bytes_done"].as_u64().unwrap() > 0 {
            midway = true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(midway, "no progress was seen while the job ran");
    assert_eq!(
        (last["done"].as_u64(), last["failed"].as_u64()),
        (Some(100), Some(0)),
        "{last}"
    );
    let secs = started.elapsed().as_secs_f64();
    assert!(secs >= 1.5, "the download pace was not kept ({secs:.1} s)");
    let into = music_b.join("From homeserver");
    assert!(into.join("000_song.wav").is_file());
    assert_eq!(
        std::fs::read_dir(&into).unwrap().count(),
        100,
        "no partial files left"
    );
    wait_scan(b.clone()).await;
    let mut tracks = 0;
    for _ in 0..100 {
        let (_, t) = call(&app_b, "GET", "/v1/library/tracks?limit=200", None).await;
        tracks = t["tracks"].as_array().unwrap().len();
        if tracks == 100 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(tracks, 100, "the brought files were not added to the library");

    // Again: nothing left to bring
    let (_, again) = call(
        &app_b,
        "POST",
        "/v1/pubky/files/mirror",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(
        (
            again["total"].as_u64(),
            again["skipped"].as_u64(),
            again["finished"].as_bool()
        ),
        (Some(0), Some(100), Some(true)),
        "{again}"
    );

    // Copying B's library (the brought files) back adds nothing and doubles no names
    let (_, job) = call(
        &app_b,
        "POST",
        "/v1/pubky/files/upload",
        Some(serde_json::json!({"all": true})),
    )
    .await;
    let done = wait_job(app_b.clone(), job["id"].as_str().unwrap().into()).await;
    assert_eq!(
        (done["done"].as_u64(), done["skipped"].as_u64()),
        (Some(0), Some(100)),
        "{done}"
    );
    let names: Vec<String> = sync_a
        .list_private_files()
        .await
        .unwrap()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    assert_eq!(names.len(), 100, "{names:?}");
}
