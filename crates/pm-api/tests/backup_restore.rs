//! Backup and restore (steward next-4, item 1): back up a seeded node, restore it into an empty
//! one, and compare everything that should survive: matches and corrections, playlists,
//! favourites, tags, play history, queues, settings, sign-ins (only with the passphrase), the
//! Pubky session and the remote-access key. Then a rescan keeps the matches, and music that
//! moved is pointed to its new folder without losing them.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use id3::TagLike;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_model::{Confidence, MusicUri};
use pm_zones::OutputRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
const CONFIRMED: &str = "833f00e1-781f-4edd-90e4-e52712618862";
const TIDAL_TOKEN: &str = "TIDAL-SECRET-TOKEN-5f1e";
const LB_TOKEN: &str = "LISTENBRAINZ-SECRET-77aa";

fn write_wav(path: &Path, title: &str, mbid: Option<&str>) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..2205 {
        w.write_sample((i % 50) as i16).unwrap();
        w.write_sample(-((i % 50) as i16)).unwrap();
    }
    w.finalize().unwrap();
    let mut t = id3::Tag::new();
    t.set_title(title);
    t.set_artist("Tester");
    t.set_album("Backups");
    if let Some(m) = mbid {
        t.add_frame(id3::frame::ExtendedText {
            description: "MusicBrainz Track Id".into(),
            value: m.into(),
        });
    }
    t.write_to_path(path, id3::Version::Id3v24).unwrap();
}

fn node(dir: &Path, name: &str, music: Option<PathBuf>) -> Arc<NodeState> {
    let data = dir.join(name);
    std::fs::create_dir_all(&data).unwrap();
    let db = data.join("library.db");
    NodeState::new(
        NodeSettings {
            name: name.into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: music.into_iter().collect(),
            art_dir: data.join("art"),
            mirror_dir: data.join("homeserver"),
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

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    (
        status,
        resp.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

async fn json(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let (st, bytes) = send(app, req).await;
    (
        st,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn restore(app: &axum::Router, bytes: &[u8], pass: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/restore")
        .header("content-type", "application/octet-stream");
    if let Some(p) = pass {
        b = b.header(pm_api::backup::PASSPHRASE_HEADER, p);
    }
    let (st, out) = send(app, b.body(Body::from(bytes.to_vec())).unwrap()).await;
    (
        st,
        serde_json::from_slice(&out).unwrap_or(serde_json::Value::Null),
    )
}

async fn scanned(s: &Arc<NodeState>) {
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if !s.is_scanning() {
            return;
        }
    }
    panic!("scan did not finish");
}

/// Everything that should survive, read the same way on both nodes.
fn snapshot(s: &NodeState) -> serde_json::Value {
    let lib = s.library.lock().unwrap();
    let mut tracks: Vec<(String, Option<String>)> = lib
        .search_tracks("tester", 50)
        .unwrap()
        .into_iter()
        .map(|t| {
            (
                t.title.clone(),
                t.canonical.map(|(u, c)| format!("{} {:?}", u.to_url(), c)),
            )
        })
        .collect();
    tracks.sort();
    let mut playlists: Vec<String> = lib
        .list_music_posts(None)
        .unwrap()
        .into_iter()
        .map(|p| p.json)
        .collect();
    playlists.sort();
    let favs: Vec<String> = lib.list_bookmarks().unwrap().into_iter().map(|f| f.uri).collect();
    let mut tags: Vec<String> = lib
        .tag_ids()
        .unwrap()
        .into_iter()
        .map(|id| {
            let t = lib.tag_by_id(&id).unwrap().unwrap();
            format!("{} {}", t.uri, t.label)
        })
        .collect();
    tags.sort();
    serde_json::json!({
        "counts": lib.backup_counts().unwrap(),
        "tracks": tracks,
        "playlists": playlists,
        "favourites": favs,
        "tags": tags,
        "plays": lib.history(100).unwrap().len(),
        "contact": lib.get_setting("node.contact").unwrap(),
        "queue": lib.get_setting("queue.kitchen").unwrap(),
        "location": lib.get_setting("home.location").unwrap(),
        "tidal": lib.get_setting("tidal.user").unwrap(),
        "listenbrainz": lib.get_setting("listenbrainz.account").unwrap(),
    })
}

#[tokio::test]
async fn a_backup_rebuilds_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("01 tagged.wav"), "Tagged song", Some(MB));
    write_wav(&music.join("02 confirmed.wav"), "Confirmed song", None);

    // ---- a seeded node
    let a = node(dir.path(), "a", Some(music.clone()));
    let app_a = router(a.clone());
    let (st, _) = json(&app_a, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::OK);
    scanned(&a).await;
    let song = format!("https://musicbrainz.org/recording/{MB}");
    let confirmed_uri = MusicUri::parse(&format!("https://musicbrainz.org/recording/{CONFIRMED}")).unwrap();
    {
        let lib = a.library.lock().unwrap();
        let t = lib.search_tracks("confirmed", 5).unwrap().remove(0);
        // The owner's correction: exists nowhere but on this node
        assert!(lib
            .set_identity(t.id.unwrap().0, &confirmed_uri, Confidence::ConfirmedByUser, None)
            .unwrap());
        lib.add_bookmark(
            &pm_pubky::Bookmark::id_for(&MusicUri::parse(&song).unwrap()),
            &song,
            1_700_000_000_000_000,
        )
        .unwrap();
        lib.add_tag("tag1", &song, "road-trip", 1_700_000_000_000_001)
            .unwrap();
        lib.record_play(t.id.unwrap(), Some("kitchen")).unwrap();
        lib.set_setting("queue.kitchen", "{\"items\":[],\"repeat\":\"off\"}")
            .unwrap();
        lib.set_setting(
            "home.location",
            "{\"label\":\"Zurich\",\"lat\":47.37,\"lon\":8.54}",
        )
        .unwrap();
        lib.set_setting("tidal.user", &format!("{{\"access_token\":\"{TIDAL_TOKEN}\"}}"))
            .unwrap();
        lib.set_setting("listenbrainz.account", &format!("{{\"token\":\"{LB_TOKEN}\"}}"))
            .unwrap();
        // A sign-in moved aside after "Sign out everywhere" (vault_cache): still a token
        lib.set_setting(
            "vault.signed_out.spotify.user",
            "{\"access_token\":\"ASIDE-SECRET-31\"}",
        )
        .unwrap();
    }
    let (st, _) = json(
        &app_a,
        "PUT",
        "/v1/node/settings",
        Some(serde_json::json!({"contact": "https://example.org/backup-test"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = json(
        &app_a,
        "POST",
        "/v1/playlists",
        Some(serde_json::json!({"name": "Road trip", "uris": [song]})),
    )
    .await;
    assert!(st.is_success(), "{st}");
    std::fs::write(dir.path().join("a/pubky-session.secret"), b"not-a-real-session").unwrap();
    std::fs::write(dir.path().join("a/remote.key"), [7u8; 32]).unwrap();
    std::fs::write(dir.path().join("a/api-token"), b"made-by-the-node").unwrap();
    let cfg = dir.path().join("a/node.toml");
    std::fs::write(
        &cfg,
        "[tidal]\nclient_id = \"app-id\"\nclient_secret = \"app-secret\"\n",
    )
    .unwrap();
    a.set_config_file(Some(cfg));
    let before = snapshot(&a);
    assert_eq!(before["counts"]["identified"], 2);

    // ---- a backup with the sign-ins locked in, and one without
    let (st, locked) = send(
        &app_a,
        Request::builder()
            .method("POST")
            .uri("/v1/backup")
            .header(pm_api::backup::PASSPHRASE_HEADER, "correct horse battery")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let parsed = pm_api::backup::parse(&locked).unwrap();
    assert!(parsed.manifest.secrets.is_some());
    for secret in [
        TIDAL_TOKEN,
        LB_TOKEN,
        "app-secret",
        "not-a-real-session",
        "ASIDE-SECRET-31",
    ] {
        assert!(
            !contains(&parsed.database, secret),
            "{secret} in the plain database"
        );
        assert!(
            !contains(parsed.secrets.as_ref().unwrap(), secret),
            "{secret} unencrypted"
        );
    }
    let (st, open) = send(
        &app_a,
        Request::builder().uri("/v1/backup").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let plain = pm_api::backup::parse(&open).unwrap();
    assert!(plain.secrets.is_none() && plain.manifest.secrets.is_none());
    assert!(
        plain.manifest.left_out.iter().any(|w| w.contains("TIDAL")),
        "{:?}",
        plain.manifest.left_out
    );
    assert!(!contains(&plain.database, TIDAL_TOKEN));

    // ---- an empty node: the wrong passphrase changes nothing
    let b = node(dir.path(), "b", None);
    let app_b = router(b.clone());
    let empty = snapshot(&b);
    let (st, err) = restore(&app_b, &locked, Some("wrong")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("passphrase"));
    let (st, _) = restore(&app_b, &locked, None).await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "a locked backup asks for its passphrase"
    );
    assert_eq!(snapshot(&b), empty);
    let (st, garbage) = restore(&app_b, b"not a backup", Some("x")).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{garbage}");

    // ---- the right one: everything comes back
    // A key already there, readable by others: replaced, and private afterwards
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let old = dir.path().join("b/remote.key");
        std::fs::write(&old, b"old").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let (st, report) = restore(&app_b, &locked, Some("correct horse battery")).await;
    assert_eq!(st, StatusCode::OK, "{report}");
    assert_eq!(report["expected"], report["restored"], "{report}");
    assert!(report["signed_in"].to_string().contains("TIDAL"), "{report}");
    assert!(
        report["missing_folders"].as_array().unwrap().is_empty(),
        "{report}"
    );
    scanned(&b).await;
    let after = snapshot(&b);
    assert_eq!(after, before, "what survives a backup and restore");
    assert_eq!(
        std::fs::read(dir.path().join("b/pubky-session.secret")).unwrap(),
        b"not-a-real-session"
    );
    assert_eq!(std::fs::read(dir.path().join("b/remote.key")).unwrap(), [7u8; 32]);
    assert_eq!(
        std::fs::read(dir.path().join("b/api-token")).unwrap(),
        b"made-by-the-node"
    );
    #[cfg(unix)]
    for f in ["b/remote.key", "b/pubky-session.secret", "b/api-token"] {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(dir.path().join(f))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600, "{f}");
    }
    assert!(std::fs::read_to_string(dir.path().join("b/node.restored.toml"))
        .unwrap()
        .contains("app-secret"));
    assert_eq!(
        b.contact(),
        "https://example.org/backup-test",
        "Discover works at once"
    );

    // A rescan keeps the matches (the files did not change)
    let (st, _) = json(&app_b, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::OK);
    scanned(&b).await;
    assert_eq!(snapshot(&b), before);

    // ---- without the passphrase: the library, not the sign-ins
    let c = node(dir.path(), "c", None);
    let app_c = router(c.clone());
    let (st, report) = restore(&app_c, &open, None).await;
    assert_eq!(st, StatusCode::OK, "{report}");
    assert!(report["sign_in_again"].to_string().contains("TIDAL"), "{report}");
    let lib = c.library.lock().unwrap();
    assert!(lib.get_setting("tidal.user").unwrap().is_none());
    assert_eq!(
        lib.backup_counts().unwrap(),
        serde_json::from_value(before["counts"].clone()).unwrap()
    );
}

/// Restored on another machine where the music lives elsewhere: the folder is reported missing,
/// pointed to its new place, and every match, playlist and play stays with its track.
#[tokio::test]
async fn music_that_moved_keeps_its_matches() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("old-disk");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("01 tagged.wav"), "Tagged song", Some(MB));
    write_wav(&music.join("02 confirmed.wav"), "Confirmed song", None);
    let a = node(dir.path(), "a", Some(music.clone()));
    let app_a = router(a.clone());
    json(&app_a, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    scanned(&a).await;
    let confirmed_uri = MusicUri::parse(&format!("https://musicbrainz.org/recording/{CONFIRMED}")).unwrap();
    let confirmed_id = {
        let lib = a.library.lock().unwrap();
        let t = lib.search_tracks("confirmed", 5).unwrap().remove(0);
        lib.set_identity(t.id.unwrap().0, &confirmed_uri, Confidence::ConfirmedByUser, None)
            .unwrap();
        lib.record_play(t.id.unwrap(), None).unwrap();
        t.id.unwrap()
    };
    let (_, bytes) = send(
        &app_a,
        Request::builder().uri("/v1/backup").body(Body::empty()).unwrap(),
    )
    .await;

    // The music now lives on a new disk; the old path is gone
    let moved = dir.path().join("new-disk");
    std::fs::rename(&music, &moved).unwrap();
    let b = node(dir.path(), "b", None);
    let app_b = router(b.clone());
    let (st, report) = restore(&app_b, &bytes, None).await;
    assert_eq!(st, StatusCode::OK, "{report}");
    assert!(
        report["missing_folders"].to_string().contains("old-disk"),
        "{report}"
    );
    scanned(&b).await;
    let (st, out) = json(
        &app_b,
        "POST",
        "/v1/library/folders/move",
        Some(serde_json::json!({"from": report["missing_folders"][0], "to": moved})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{out}");
    assert_eq!(out["moved_files"], 2, "{out}");
    scanned(&b).await;
    let lib = b.library.lock().unwrap();
    let t = lib.get_track(confirmed_id).unwrap().expect("the same track");
    assert!(
        t.file.path.starts_with(moved.canonicalize().unwrap()),
        "{:?}",
        t.file.path
    );
    assert_eq!(t.canonical, Some((confirmed_uri, Confidence::ConfirmedByUser)));
    assert_eq!(lib.history(10).unwrap().len(), 1);
    assert_eq!(lib.stats().unwrap().missing_files, 0);
    assert_eq!(lib.stats().unwrap().tracks, 2, "no duplicates after the rescan");
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn hearts_on_songs_without_an_identity_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("01 untagged.wav"), "Untagged song", None);
    let a = node(dir.path(), "a", Some(music.clone()));
    let app_a = router(a.clone());
    let (st, _) = json(&app_a, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::OK);
    scanned(&a).await;
    let id = a.library.lock().unwrap().search_tracks("untagged", 5).unwrap()[0]
        .id
        .unwrap()
        .0;
    let (st, v) = json(
        &app_a,
        "PUT",
        "/v1/favourites",
        Some(serde_json::json!({"track_id": id})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let (st, file) = send(
        &app_a,
        Request::builder().uri("/v1/backup").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let check = Request::builder()
        .method("POST")
        .uri("/v1/restore?check=true")
        .body(Body::from(file.clone()))
        .unwrap();
    let (st, info) = send(&app_a, check).await;
    assert_eq!(st, StatusCode::OK);
    let info: serde_json::Value = serde_json::from_slice(&info).unwrap();
    assert_eq!(info["counts"]["favourites"], 1, "{info}");

    let b = node(dir.path(), "b", Some(music));
    let (st, report) = restore(&router(b.clone()), &file, None).await;
    assert_eq!(st, StatusCode::OK, "{report}");
    assert_eq!(report["expected"]["favourites"], 1, "{report}");
    assert_eq!(report["restored"]["favourites"], 1, "{report}");
}

/// Backups are the owner's alone: a scheduled backup file is 0600 (security review 2026-10-30,
/// #2; the working copy's private folder is `backup.rs`'s test).
#[cfg(unix)]
#[tokio::test]
async fn backups_are_readable_by_their_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path(), "a", None);
    let app = router(s.clone());
    let folder = dir.path().join("backups");
    let (st, v) = json(
        &app,
        "PUT",
        "/v1/backup/schedule",
        Some(serde_json::json!({"folder": folder.to_string_lossy(), "every_hours": 24, "keep": 3})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = json(&app, "POST", "/v1/backup/schedule/run", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let file = folder.join(v["last_file"].as_str().unwrap());
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{}", file.display());
}
