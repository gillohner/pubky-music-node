//! A node reached through the iroh tunnel (ADR 0013, `pm-remote`): the REST API and the event
//! WebSocket work unchanged. Relays are off, so the test needs no internet: the client dials the
//! node's direct addresses.

use pm_api::{NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_remote::{ClientTunnel, NodeTunnel, Relays, Route, SecretKey};
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use std::sync::Arc;

async fn node(dir: &std::path::Path) -> std::net::SocketAddr {
    let db = dir.join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let state = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some("tok".into()),
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
    state.zones_for_all_outputs();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(pm_api::serve_listener(state, listener));
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_api_and_events_work_through_the_tunnel() {
    let dir = tempfile::tempdir().unwrap();
    let api = node(dir.path()).await;
    let server = NodeTunnel::start(SecretKey::generate(), &Relays::Off, api)
        .await
        .unwrap();
    let client = ClientTunnel::start(server.addr(), &Relays::Off).await.unwrap();
    assert_eq!(client.connect().await.unwrap(), Route::Direct);

    let c =
        pm_client::NodeClient::new(&format!("http://{}", client.local_addr()), Some("tok".into())).unwrap();
    let info = c.info().await.unwrap();
    assert_eq!(info.name, "t");
    // Several requests share the one QUIC connection, each on its own stream.
    let (a, b) = tokio::join!(c.info(), c.zones());
    assert!(a.is_ok() && b.is_ok());
    let mut events = c.events().await.unwrap();
    let first = events.next().await.unwrap().unwrap();
    assert!(matches!(first, pm_api::types::ApiEvent::Snapshot { .. }));

    // A wrong token is still refused: the tunnel carries requests, it does not authorize them.
    let bad =
        pm_client::NodeClient::new(&format!("http://{}", client.local_addr()), Some("nope".into())).unwrap();
    assert!(matches!(
        bad.info().await,
        Err(pm_client::ClientError::Api { status: 401, .. })
    ));
    client.close().await;
    server.close().await;
}

#[test]
fn the_node_key_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote.key");
    let a = pm_remote::load_or_create_key(&path).unwrap();
    let b = pm_remote::load_or_create_key(&path).unwrap();
    assert_eq!(a.public(), b.public());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
