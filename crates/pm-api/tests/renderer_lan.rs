//! The renderer routes take no token, so the node serves them to LAN peers only: not through the
//! iroh tunnel's listener, not through a proxy or Cloudflare. A node without an API token answers
//! nothing through the tunnel (security review 2026-10-27, #2).

use pm_api::{NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use std::sync::Arc;

fn node(dir: &std::path::Path, token: Option<&str>) -> Arc<NodeState> {
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
            token: token.map(str::to_string),
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

/// The node's own listener and the tunnel's, as base URLs.
async fn serve(s: Arc<NodeState>) -> (String, String) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let direct = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(pm_api::serve_listener(s.clone(), l));
    let remote = format!("http://{}", pm_api::peer::serve_remote(s).await.unwrap());
    (direct, remote)
}

async fn status(url: &str, headers: &[(&str, &str)]) -> u16 {
    let mut r = reqwest::Client::new().get(url);
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    r.send().await.unwrap().status().as_u16()
}

#[tokio::test]
async fn the_renderer_answers_lan_peers_only() {
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path(), Some("owner-token"));
    let _ssdp = s.start_renderer(0, false);
    let (direct, remote) = serve(s).await;
    // No zones here, so a request that gets through is a 404; one refused is a 403
    let desc = "/upnp/zone/kitchen/desc.xml";
    assert_eq!(
        status(&format!("{direct}{desc}"), &[]).await,
        404,
        "a peer on this machine"
    );
    assert_eq!(status(&format!("{remote}{desc}"), &[]).await, 403, "through iroh");
    for h in ["x-forwarded-for", "cf-connecting-ip", "forwarded", "x-real-ip"] {
        assert_eq!(
            status(&format!("{direct}{desc}"), &[(h, "203.0.113.9")]).await,
            403,
            "through a proxy ({h})"
        );
    }
    // The rest of the API still needs the token, the same on both listeners
    assert_eq!(status(&format!("{remote}/v1/info"), &[]).await, 401);
    let bearer = [("authorization", "Bearer owner-token")];
    assert_eq!(status(&format!("{remote}/v1/info"), &bearer).await, 200);
    assert_eq!(status(&format!("{direct}/v1/info"), &bearer).await, 200);
}

#[tokio::test]
async fn a_node_without_a_token_answers_only_directly() {
    let dir = tempfile::tempdir().unwrap();
    let (direct, remote) = serve(node(dir.path(), None)).await;
    assert_eq!(status(&format!("{direct}/v1/info"), &[]).await, 200);
    assert_eq!(status(&format!("{remote}/v1/info"), &[]).await, 401);
    assert_eq!(
        status(
            &format!("{direct}/v1/info"),
            &[("cf-connecting-ip", "203.0.113.9")]
        )
        .await,
        401
    );
    assert_eq!(
        status(&format!("{remote}/rest/ping"), &[]).await,
        401,
        "Subsonic too"
    );
}
