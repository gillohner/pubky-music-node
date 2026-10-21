//! The `/v1/events` resync contract (docs/CONNECTION.md): numbered events, a
//! snapshot with `seq`/`boot`/`library_rev`, ping/pong, and `resync`.

use futures_util::{SinkExt, StreamExt};
use pm_api::{NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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

async fn open(addr: std::net::SocketAddr) -> Ws {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/events?token=tok"))
        .await
        .unwrap();
    ws
}

/// The next JSON text message.
async fn next(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a message within 5 s")
            .unwrap()
            .unwrap();
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Skip to the next message of `kind`.
async fn next_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        let v = next(ws).await;
        if v["type"] == kind {
            return v;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_are_numbered_and_resync_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let addr = node(dir.path()).await;
    let mut a = open(addr).await;
    let mut b = open(addr).await;
    let snap = next(&mut a).await;
    assert_eq!(snap["type"], "snapshot");
    assert!(snap["seq"].is_u64());
    assert_eq!(snap["boot"].as_str().unwrap().len(), 16);
    assert!(snap["library_rev"].is_u64());
    let seq0 = snap["seq"].as_u64().unwrap();
    let snap_b = next(&mut b).await;
    assert_eq!(snap_b["boot"], snap["boot"]);

    a.send(Message::Text(
        json!({"type": "ping", "id": 42}).to_string().into(),
    ))
    .await
    .unwrap();
    assert_eq!(next_of(&mut a, "pong").await["id"], 42);

    // A change reaches both controllers with the same next number.
    let http = reqwest::Client::new();
    let r = http
        .post(format!("http://{addr}/v1/zones"))
        .bearer_auth("tok")
        .json(&json!({"name": "Kitchen", "output": "null"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let ev_a = next(&mut a).await;
    let ev_b = next(&mut b).await;
    assert_eq!(ev_a["seq"].as_u64().unwrap(), seq0 + 1, "{ev_a}");
    assert_eq!(ev_a["seq"], ev_b["seq"]);
    assert_eq!(ev_a["type"], ev_b["type"]);

    // Resync: a fresh snapshot with the latest number and the new zone.
    a.send(Message::Text(json!({"type": "resync"}).to_string().into()))
        .await
        .unwrap();
    let again = next_of(&mut a, "snapshot").await;
    assert!(again["seq"].as_u64().unwrap() > seq0);
    assert!(again["zones"]
        .as_array()
        .unwrap()
        .iter()
        .any(|z| z["id"] == "kitchen"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_node_has_a_new_boot_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut one = open(node(dir.path()).await).await;
    let first = next(&mut one).await;
    let other = tempfile::tempdir().unwrap();
    let mut two = open(node(other.path()).await).await;
    let second = next(&mut two).await;
    assert_ne!(first["boot"], second["boot"]);
}
