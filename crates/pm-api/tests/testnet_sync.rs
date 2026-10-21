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
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
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
