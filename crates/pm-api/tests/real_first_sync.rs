//! How long a first sync of a real account takes (STEWARD-FIX-pubky-signin-speed.md): a fresh,
//! empty library signs in with an existing session and pulls everything. Ignored by default;
//! run with `PM_REAL_SESSION_FILE=<path to a session secret> cargo test -p pm-api --test
//! real_first_sync -- --ignored --nocapture`. It reads from the homeserver and writes only to a
//! temporary library.

use pm_api::sync::PubkySync;
use pm_api::{NodeSettings, NodeState};
use pm_library::Library;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn first_sync_of_a_real_account() {
    let Ok(file) = std::env::var("PM_REAL_SESSION_FILE") else {
        eprintln!("PM_REAL_SESSION_FILE not set");
        return;
    };
    let secret = std::fs::read_to_string(file).unwrap();
    // RUST_LOG=pubky=debug,pm_api=debug shows where the time goes
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lib.db");
    let s = NodeState::new(
        NodeSettings {
            name: "timing".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("hs"),
            pipeline: Default::default(),
            token: None,
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:1".into(),
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
        pm_zones::OutputRegistry::new(),
    );
    let t0 = std::time::Instant::now();
    let sync = Arc::new(
        PubkySync::connect(pm_pubky::Network::Mainnet, &secret)
            .await
            .unwrap(),
    );
    let session_ms = t0.elapsed().as_millis();
    let t1 = std::time::Instant::now();
    let r = sync.sync(&s.library).await.unwrap();
    eprintln!(
        "session restored in {session_ms} ms; first sync {} ms: pulled {}, pushed {}, errors {}",
        t1.elapsed().as_millis(),
        r.pulled,
        r.pushed,
        r.errors.len()
    );
}
