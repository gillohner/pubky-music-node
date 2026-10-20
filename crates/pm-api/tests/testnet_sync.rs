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
use pm_pubky::Visibility;
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
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            slimproto_port: 0,
            session_secret_path: None,
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    )
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

    // The post sits in the private folder: readable with the session, not publicly.
    let private = homeserver
        .read_music_posts(&z32, Visibility::Private, Some(sync_a.session()))
        .await
        .unwrap();
    assert_eq!(private.len(), 1);
    assert_eq!(private[0].0, format!("/priv/pubky.app/posts/{id}"));
    assert_eq!(private[0].1.as_playlist().unwrap().name, "Testnet mix");
    let public = homeserver.read_playlists_of(&z32).await.unwrap();
    assert!(public.is_empty());

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
    assert_eq!(got["summary"]["name"], "Testnet mix");

    // Deleting on A removes it remotely; B's next sync keeps its local copy (no tombstone pull yet).
    let (st, _) = call(&app_a, "DELETE", &format!("/v1/playlists/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let r = sync_a.sync(&a.library).await.unwrap();
    assert_eq!(r.deleted, 1, "{r:?}");
    let private = homeserver
        .read_music_posts(&z32, Visibility::Private, Some(sync_a.session()))
        .await
        .unwrap();
    assert!(private.is_empty());

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
    let mirror = dir.path().join("mirror-b");
    let r = pm_api::files::mirror_into(&sync_b, &mirror).await.unwrap();
    assert_eq!(r.fetched, 1, "{r:?}");
    assert_eq!(
        std::fs::read(mirror.join(remote.rsplit('/').next().unwrap())).unwrap(),
        bytes
    );
    let r = pm_api::files::mirror_into(&sync_b, &mirror).await.unwrap();
    assert_eq!((r.fetched, r.present), (0, 1));
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
