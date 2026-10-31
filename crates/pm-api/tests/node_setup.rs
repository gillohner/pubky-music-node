//! Setting a node up: the config's library folders (scanned at start, listed with where they come
//! from), replacing a token the node made itself, grouping every kind of output the API groups,
//! and Home's "Get started" steps.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::outputs::{NullOutput, PullOutput};
use pm_zones::{Output, OutputRegistry};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const OWNER: &str = "owner-token";
const KEY: &str = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";

fn node(dir: &Path, folders: Vec<PathBuf>, outputs: OutputRegistry) -> Arc<NodeState> {
    let db = dir.join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: folders,
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some(OWNER.into()),
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
    *s.deezer_base.write().unwrap() = "http://127.0.0.1:9".into();
    s.zones_for_all_outputs();
    s
}

/// A person with music access, signed in on a device.
fn listener(s: &NodeState) -> String {
    let roster = json!({"people": [{"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster.to_string())
        .unwrap();
    pm_api::accounts::mint(s, KEY).unwrap()
}

fn just_null() -> OutputRegistry {
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    outputs
}

async fn call(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn write_wav(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..4410u32 {
        w.write_sample((i % 100) as i16).unwrap();
        w.write_sample(-((i % 100) as i16)).unwrap();
    }
    w.finalize().unwrap();
}

async fn scanned(s: &NodeState) -> pm_api::types::LastScan {
    for _ in 0..1000 {
        if !s.is_scanning() {
            if let Some(l) = s.last_scan.lock().unwrap().clone() {
                return l;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no scan finished");
}

#[tokio::test]
async fn the_configs_folders_are_listed_as_such_and_not_removed_through_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let gone = dir.path().join("unplugged");
    let s = node(dir.path(), vec![music.clone(), gone.clone()], just_null());
    let app = router(s);
    let path = |p: &Path| p.to_string_lossy().into_owned();
    // Listed before any scan: present, and missing
    let (st, f) = call(&app, OWNER, "GET", "/v1/library/folders", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(f, json!([[path(&music), true], [path(&gone), false]]));
    let (_, ns) = call(&app, OWNER, "GET", "/v1/node/settings", None).await;
    assert_eq!(ns["config_folders"], json!([path(&music), path(&gone)]));
    // One added through the API is not from the config
    let extra = dir.path().join("more");
    std::fs::create_dir(&extra).unwrap();
    let (st, r) = call(
        &app,
        OWNER,
        "POST",
        "/v1/library/folders",
        Some(json!({"path": path(&extra)})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let extra = path(&extra.canonicalize().unwrap());
    assert_eq!(r["folders"].as_array().unwrap().len(), 3, "{r}");
    assert_eq!(r["config_folders"], json!([path(&music), path(&gone)]));
    // The config brings its folders back at every start: removing one is for the config
    let (st, r) = call(
        &app,
        OWNER,
        "DELETE",
        &format!("/v1/library/folders?path={}", path(&music)),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    assert!(r["error"].as_str().unwrap().contains("[library] folders"), "{r}");
    for _ in 0..500 {
        let (st, _) = call(
            &app,
            OWNER,
            "DELETE",
            &format!("/v1/library/folders?path={extra}"),
            None,
        )
        .await;
        if st != StatusCode::CONFLICT {
            assert_eq!(st, StatusCode::OK);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, f) = call(&app, OWNER, "GET", "/v1/library/folders", None).await;
    assert_eq!(f, json!([[path(&music), true], [path(&gone), false]]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_started_with_a_config_folder_has_its_tracks_and_a_restart_reads_nothing_again() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("one.wav"));
    let gone = dir.path().join("unplugged");
    let s = node(dir.path(), vec![gone.clone(), music.clone()], just_null());
    s.scan_config_folders();
    let first = scanned(&s).await;
    assert_eq!((first.added, first.error_count), (1, 0), "{first:?}");
    let (_, t) = call(
        &router(s.clone()),
        OWNER,
        "GET",
        "/v1/library/tracks?limit=10",
        None,
    )
    .await;
    assert_eq!(t["tracks"].as_array().unwrap().len(), 1, "{t}");
    drop(s);
    // The same node again: the unchanged file is skipped
    let s = node(dir.path(), vec![gone, music], just_null());
    s.scan_config_folders();
    let again = scanned(&s).await;
    assert_eq!(
        (again.added, again.updated, again.removed, again.error_count),
        (0, 0, 0, 0),
        "{again:?}"
    );
}

#[tokio::test]
async fn the_owner_replaces_a_token_the_node_made_and_the_old_one_stops_working() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("api-token");
    std::fs::write(&file, OWNER).unwrap();
    let s = node(dir.path(), vec![], just_null());
    let listener = listener(&s);
    // An owner's browser session (only valid with the node's identity loaded, which needs a
    // homeserver: tests/bundled_homeserver.rs; here, that the node keeps it)
    pm_api::accounts::mint_owner(&s, "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy").unwrap();
    let browsers = || {
        let kept = s
            .library
            .lock()
            .unwrap()
            .get_setting("accounts.owner_sessions")
            .unwrap()
            .unwrap_or_default();
        serde_json::from_str::<Vec<serde_json::Value>>(&kept)
            .unwrap_or_default()
            .len()
    };
    assert_eq!(browsers(), 1);
    let app = router(s.clone());
    // Set in the config: changed there, not here
    let (st, r) = call(&app, OWNER, "POST", "/v1/node/token", None).await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    let (_, ns) = call(&app, OWNER, "GET", "/v1/node/settings", None).await;
    assert_eq!(ns["token_replaceable"], false);
    s.set_token_file(file.clone());
    let (_, ns) = call(&app, OWNER, "GET", "/v1/node/settings", None).await;
    assert_eq!(ns["token_replaceable"], true);
    let (st, _) = call(&app, &listener, "POST", "/v1/node/token", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "only the owner");
    let (st, r) = call(&app, OWNER, "POST", "/v1/node/token", None).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let new = r["token"].as_str().unwrap().to_string();
    assert_eq!(new.len(), 64);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        new,
        "kept for the next start"
    );
    let (st, _) = call(&app, OWNER, "GET", "/v1/info", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "the old token no longer works");
    let (st, _) = call(&app, &new, "GET", "/v1/info", None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(&app, &listener, "GET", "/v1/info", None).await;
    assert_eq!(st, StatusCode::OK, "listeners keep their own tokens");
    assert_eq!(
        browsers(),
        0,
        "the owner's browsers are logged out (the answer to a leak)"
    );
}

/// Replacements at once: one wins, and the file holds the token the node checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacements_at_once_leave_the_file_and_the_node_agreeing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("api-token");
    std::fs::write(&file, OWNER).unwrap();
    let s = node(dir.path(), vec![], just_null());
    s.set_token_file(file.clone());
    let app = router(s.clone());
    let mut kept = String::new();
    for round in 0..20 {
        let current = s.token().unwrap();
        let calls = (0..8).map(|_| {
            let (app, t) = (app.clone(), current.clone());
            tokio::spawn(async move { call(&app, &t, "POST", "/v1/node/token", None).await })
        });
        let mut answered = Vec::new();
        for c in calls {
            // A call that comes in once another has replaced the token is refused
            let (st, v) = c.await.unwrap();
            assert!(st == StatusCode::OK || st == StatusCode::UNAUTHORIZED, "{st} {v}");
            if st == StatusCode::OK {
                answered.push(v["token"].as_str().unwrap().to_string());
            }
        }
        kept = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            Some(kept.clone()),
            s.token(),
            "round {round}: the file and the node agree"
        );
        assert!(answered.contains(&kept));
    }
    let (st, _) = call(&app, &kept, "GET", "/v1/info", None).await;
    assert_eq!(st, StatusCode::OK);
    // Nothing left beside the token but the time it was replaced
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("api-token"))
        .collect();
    names.sort();
    assert_eq!(names, ["api-token", "api-token.replaced"]);
    assert!(pm_api::access_token::replaced_after(dir.path(), 0));
    assert!(
        !pm_api::access_token::replaced_after(dir.path(), i64::MAX),
        "a backup made later keeps its token"
    );
}

/// The owner's event socket, opened with the old token, ends when the token is replaced; a
/// listener's stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_sockets_opened_with_the_old_token_end_when_it_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("api-token");
    std::fs::write(&file, OWNER).unwrap();
    let s = node(dir.path(), vec![], just_null());
    s.set_token_file(file);
    let person = listener(&s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(pm_api::serve_listener(s.clone(), l));
    let open = |t: String| async move {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/events?token={t}"))
            .await
            .unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .unwrap();
        assert!(
            matches!(first, Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))),
            "the snapshot"
        );
        ws
    };
    let mut owner = open(OWNER.to_string()).await;
    let mut theirs = open(person).await;
    let (st, _) = call(&router(s.clone()), OWNER, "POST", "/v1/node/token", None).await;
    assert_eq!(st, StatusCode::OK);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match owner.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None | Some(Err(_)) => {
                    return true
                }
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert_eq!(ended, Ok(true), "the owner's socket with the old token ended");
    // The listener's is still there: nothing arrives, and it hasn't closed
    let still = tokio::time::timeout(std::time::Duration::from_millis(500), theirs.next()).await;
    assert!(
        still.is_err()
            || matches!(
                still,
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_))))
            ),
        "{still:?}"
    );
}

/// A config folder taken out of the config and the node restarted: an ordinary folder now, which
/// Remove takes out with its songs (what the 409 says to do). And a config folder isn't moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_taken_out_of_the_config_is_removed_with_its_songs() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("one.wav"));
    let s = node(dir.path(), vec![music.clone()], just_null());
    s.scan_config_folders();
    scanned(&s).await;
    let app = router(s.clone());
    let path = music.to_string_lossy().into_owned();
    let (st, r) = call(
        &app,
        OWNER,
        "POST",
        "/v1/library/folders/move",
        Some(json!({"from": path, "to": dir.path()})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    let (st, r) = call(
        &app,
        OWNER,
        "DELETE",
        &format!("/v1/library/folders?path={path}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(
        r["error"].as_str().unwrap().contains("then remove it here"),
        "{r}"
    );
    drop((app, s));
    // Restarted without it in the config
    let s = node(dir.path(), vec![], just_null());
    let app = router(s);
    let (_, ns) = call(&app, OWNER, "GET", "/v1/node/settings", None).await;
    assert_eq!(ns["config_folders"], json!([]));
    let (_, f) = call(&app, OWNER, "GET", "/v1/library/folders", None).await;
    assert_eq!(f, json!([[path, true]]));
    let (st, r) = call(
        &app,
        OWNER,
        "DELETE",
        &format!("/v1/library/folders?path={path}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["removed_tracks"], 1);
    assert_eq!(r["folders"], json!([]));
}

fn upnp(name: &str, udn: &str) -> Arc<pm_out_upnp::UpnpOutput> {
    let r = pm_out_upnp::Renderer {
        udn: format!("uuid:{udn}"),
        friendly_name: name.into(),
        manufacturer: None,
        model: None,
        av_transport: "http://127.0.0.1:9/av".into(),
        rendering_control: None,
        connection_manager: None,
        zone_group_topology: None,
        location: "http://127.0.0.1:9/desc.xml".into(),
    };
    Arc::new(pm_out_upnp::UpnpOutput::new(
        r,
        Arc::new(PullOutput::new(udn)),
        format!("http://127.0.0.1:7788/v1/outputs/pull/{udn}/stream.wav"),
    ))
}

fn bluos(name: &str, mac: &str) -> Arc<pm_out_bluos::BluosOutput> {
    Arc::new(pm_out_bluos::BluosOutput::new(
        pm_out_bluos::Player {
            id: mac.into(),
            name: name.into(),
            host: "127.0.0.1".into(),
            port: 9,
            model: None,
        },
        Arc::new(PullOutput::new(mac)),
        "http://127.0.0.1:7788/s".into(),
    ))
}

/// What Settings › Group outputs offers: every output, each marked with the speaker system that
/// groups it itself. Each family on its own becomes that system's group, a mix a node group; only
/// outputs that share no sample rate are refused.
#[tokio::test]
async fn every_kind_of_output_groups_as_the_api_says() {
    let dir = tempfile::tempdir().unwrap();
    let outputs = OutputRegistry::new();
    let mut caps = pm_model::OutputCaps::unlimited();
    caps.pcm_rates = vec![8_000];
    outputs.register(Arc::new(NullOutput::with_caps(caps)));
    let (sa, sb) = (upnp("Sonos A", "RINCON_A"), upnp("Sonos B", "RINCON_B"));
    let (ba, bb) = (bluos("Living", "aa01"), bluos("Kitchen", "aa02"));
    let plain = upnp("Streamer", "plain-1");
    for o in [&sa, &sb, &plain] {
        outputs.register(o.clone());
    }
    for o in [&ba, &bb] {
        outputs.register(o.clone());
    }
    let s = node(dir.path(), vec![], outputs);
    for o in [&sa, &sb] {
        s.sonos.lock().unwrap().insert(o.id(), o.clone());
    }
    for o in [&ba, &bb] {
        s.bluos_players.lock().unwrap().insert(o.id(), o.clone());
    }
    let app = router(s);
    let (_, outs) = call(&app, OWNER, "GET", "/v1/outputs", None).await;
    let by = |id: &str| {
        outs.as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {outs}"))["group_by"]
            .clone()
    };
    let id = |o: &dyn Output| o.id().0;
    assert_eq!(by(&id(sa.as_ref())), "sonos");
    assert_eq!(by(&id(ba.as_ref())), "bluos");
    assert_eq!(by(&id(plain.as_ref())), Value::Null);
    let group = |ids: Vec<String>| {
        let app = app.clone();
        async move {
            call(
                &app,
                OWNER,
                "POST",
                "/v1/zones",
                Some(json!({"name": "G", "outputs": ids})),
            )
            .await
        }
    };
    let (st, z) = group(vec![id(sa.as_ref()), id(sb.as_ref())]).await;
    assert_eq!(st, StatusCode::OK, "{z}");
    assert!(z["output"].as_str().unwrap().starts_with("sonos-group:"), "{z}");
    let (st, z) = group(vec![id(ba.as_ref()), id(bb.as_ref())]).await;
    assert_eq!(st, StatusCode::OK, "{z}");
    assert!(z["output"].as_str().unwrap().starts_with("bluos-group:"), "{z}");
    // A mix: the node's own group, loosely synced
    let (st, z) = group(vec![id(sa.as_ref()), id(ba.as_ref()), id(plain.as_ref())]).await;
    assert_eq!(st, StatusCode::OK, "{z}");
    assert!(z["output"].as_str().unwrap().starts_with("group:"), "{z}");
    assert_eq!(z["sync"], "loose", "{z}");
    // No sample rate in common: refused (the page doesn't offer it)
    let (st, z) = group(vec![id(sa.as_ref()), "null".into()]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{z}");
}

fn steps(v: &Value) -> Vec<(String, bool)> {
    v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["id"].as_str().unwrap().to_string(),
                x["done"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn ids(v: &Value) -> Vec<String> {
    steps(v).into_iter().map(|(id, _)| id).collect()
}

#[tokio::test]
async fn get_started_shows_only_the_steps_that_matter() {
    let dir = tempfile::tempdir().unwrap();
    // No folder, no account, no speaker: every step
    let s = node(dir.path(), vec![], just_null());
    let listener = listener(&s);
    let app = router(s.clone());
    let (st, v) = call(&app, OWNER, "GET", "/v1/me/first-steps", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ids(&v), ["music", "play", "contact", "speakers", "pubky"], "{v}");
    assert_eq!(v["show"], true);
    // A listener: only what a listener can do
    let (st, v) = call(&app, &listener, "GET", "/v1/me/first-steps", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ids(&v), ["play"], "{v}");
    // A folder added, with something in it: no music step
    let more = dir.path().join("more");
    std::fs::create_dir(&more).unwrap();
    write_wav(&more.join("one.wav"));
    let (st, _) = call(
        &app,
        OWNER,
        "POST",
        "/v1/library/folders",
        Some(json!({"path": more})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, v) = call(&app, OWNER, "GET", "/v1/me/first-steps", None).await;
    assert!(!ids(&v).contains(&"music".to_string()), "{v}");

    // A folder from the config with nothing in it (Docker's /music, nothing mounted): the music
    // step stays; with files in it (a mounted /music), and an output: no music or speaker step
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let outputs = just_null();
    outputs.register(Arc::new(PullOutput::new("kitchen")));
    let app = router(node(dir.path(), vec![music.clone()], outputs));
    let (_, v) = call(&app, OWNER, "GET", "/v1/me/first-steps", None).await;
    assert_eq!(
        ids(&v),
        ["music", "play", "contact", "pubky"],
        "an empty folder: {v}"
    );
    write_wav(&music.join("one.wav"));
    let (_, v) = call(&app, OWNER, "GET", "/v1/me/first-steps", None).await;
    assert_eq!(ids(&v), ["play", "contact", "pubky"], "{v}");

    // Streaming only: a server of their own, no folder
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path(), vec![], just_null());
    let token = ["t", "1"].concat();
    let server = json!({"url": "http://127.0.0.1:9", "user_id": "u", "user_name": "n", "token": token, "device_id": "d"});
    *s.jellyfin.write().unwrap() = Some(serde_json::from_value(server).unwrap());
    let (_, v) = call(&router(s), OWNER, "GET", "/v1/me/first-steps", None).await;
    assert!(!ids(&v).contains(&"music".to_string()), "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_started_goes_away_when_its_steps_are_done_or_the_owner_hides_it() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("one.wav"));
    let s = node(dir.path(), vec![music], just_null());
    let listener = listener(&s);
    let app = router(s.clone());
    // Hidden for everyone, from the node's settings; shown again from there
    let (st, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/node/settings",
        Some(json!({"first_steps_hidden": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, ns) = call(&app, OWNER, "GET", "/v1/node/settings", None).await;
    assert_eq!(ns["first_steps_hidden"], true);
    for who in [OWNER, listener.as_str()] {
        let (_, v) = call(&app, who, "GET", "/v1/me/first-steps", None).await;
        assert_eq!(
            (v["show"].clone(), v["hidden"].clone()),
            (json!(false), json!(true)),
            "{v}"
        );
    }
    let (st, _) = call(
        &app,
        &listener,
        "PUT",
        "/v1/node/settings",
        Some(json!({"first_steps_hidden": false})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "the owner's setting");
    call(
        &app,
        OWNER,
        "PUT",
        "/v1/node/settings",
        Some(json!({"first_steps_hidden": false})),
    )
    .await;
    let (_, v) = call(&app, OWNER, "GET", "/v1/me/first-steps", None).await;
    assert_eq!(v["show"], true, "{v}");
    // Every step that isn't optional done: the card goes by itself
    s.scan_config_folders();
    scanned(&s).await;
    let (_, t) = call(&app, OWNER, "GET", "/v1/library/tracks?limit=1", None).await;
    let track = pm_model::TrackId(t["tracks"][0]["id"].as_i64().unwrap());
    s.library.lock().unwrap().record_play(track, None).unwrap();
    let (_, v) = call(&app, OWNER, "GET", "/v1/me/first-steps", None).await;
    assert_eq!(v["show"], true, "the contact is still to do: {v}");
    s.set_contact("music@example.org").unwrap();
    let (_, v) = call(&app, OWNER, "GET", "/v1/me/first-steps", None).await;
    assert_eq!(
        steps(&v),
        [
            ("play".into(), true),
            ("contact".into(), true),
            ("speakers".into(), false),
            ("pubky".into(), false)
        ],
        "{v}"
    );
    assert_eq!(v["show"], false, "only optional steps left: {v}");
    assert_eq!(v["hidden"], false);
}

/// Library › Node library: the owner edits a song's tags on the node (the file untouched), the
/// edit survives a rescan of a changed file and is undone; an album is renamed in place; a song
/// taken out stays out across scans and comes back on restore. Listeners may do none of it, and
/// My music needs a Pubky session.
#[tokio::test]
async fn the_owner_edits_and_removes_songs_without_touching_files() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let file = music.join("one.wav");
    write_wav(&file);
    let s = node(dir.path(), vec![music.clone()], just_null());
    s.scan_config_folders();
    scanned(&s).await;
    let app = router(s.clone());
    let (_, t) = call(&app, OWNER, "GET", "/v1/library/tracks?limit=10", None).await;
    let id = t["tracks"][0]["id"].as_i64().unwrap();
    let before = std::fs::read(&file).unwrap();
    // Edit: title and artist; blank fields keep the file's value
    let (st, t) = call(
        &app,
        OWNER,
        "PUT",
        &format!("/v1/library/tracks/{id}/edit"),
        Some(json!({"title": "Morning", "artist": "The Band", "album": "  "})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{t}");
    assert_eq!(t["title"], "Morning");
    assert_eq!(t["artists"][0]["name"], "The Band");
    assert_eq!(std::fs::read(&file).unwrap(), before, "the file is not touched");
    // The file changes: the next scan reads it again and the edit stays on top
    write_wav(&file);
    let mtime = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert!(s.start_scan(vec![music.clone()]).unwrap());
    let sc = scanned(&s).await;
    assert_eq!(sc.updated, 1, "{sc:?}");
    let (_, t) = call(&app, OWNER, "GET", &format!("/v1/library/tracks/{id}"), None).await;
    assert_eq!(t["title"], "Morning", "{t}");
    // Undo
    let (st, t) = call(
        &app,
        OWNER,
        "DELETE",
        &format!("/v1/library/tracks/{id}/edit"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{t}");
    assert_eq!(t["title"], "One", "{t}");
    // An album: renamed in place
    let (_, t) = call(
        &app,
        OWNER,
        "PUT",
        &format!("/v1/library/tracks/{id}/edit"),
        Some(json!({"album": "Mornings"})),
    )
    .await;
    let album = t["album"]["id"].as_i64().unwrap();
    let (st, a) = call(
        &app,
        OWNER,
        "PUT",
        &format!("/v1/library/albums/{album}/edit"),
        Some(json!({"title": "Mornings, Vol. 1"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{a}");
    let (_, t) = call(&app, OWNER, "GET", &format!("/v1/library/tracks/{id}"), None).await;
    assert_eq!(t["album"]["id"], album);
    assert_eq!(t["album"]["title"], "Mornings, Vol. 1");
    let (st, _) = call(
        &app,
        OWNER,
        "PUT",
        &format!("/v1/library/albums/{album}/edit"),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // The edit and what the file says, for the dialog
    let (_, e) = call(&app, OWNER, "GET", &format!("/v1/library/tracks/{id}/edit"), None).await;
    assert_eq!(e["edit"]["album"], "Mornings, Vol. 1", "{e}");
    assert_eq!(e["file"]["title"], "One", "{e}");
    // Listeners: none of it
    let l = listener(&s);
    for (m, uri, body) in [
        (
            "PUT",
            format!("/v1/library/tracks/{id}/edit"),
            Some(json!({"title": "x"})),
        ),
        ("GET", format!("/v1/library/tracks/{id}/edit"), None),
        ("DELETE", format!("/v1/library/tracks/{id}/edit"), None),
        (
            "PUT",
            format!("/v1/library/albums/{album}/edit"),
            Some(json!({"title": "x"})),
        ),
        ("DELETE", format!("/v1/library/tracks/{id}"), None),
        ("GET", "/v1/library/hidden".to_string(), None),
        (
            "POST",
            "/v1/library/hidden/restore".to_string(),
            Some(json!({"path": "/x"})),
        ),
        ("GET", "/v1/me/music".to_string(), None),
    ] {
        let (st, e) = call(&app, &l, m, &uri, body).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {uri}: {e}");
    }
    // My music needs a Pubky session
    let (st, e) = call(&app, OWNER, "GET", "/v1/me/music", None).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED, "{e}");
    // Out of the library: gone from the list, listed as hidden, not read again by a scan
    let (st, _) = call(&app, OWNER, "DELETE", &format!("/v1/library/tracks/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, t) = call(&app, OWNER, "GET", "/v1/library/tracks?limit=10", None).await;
    assert_eq!(t["tracks"].as_array().unwrap().len(), 0, "{t}");
    assert!(s.start_scan(vec![music.clone()]).unwrap());
    scanned(&s).await;
    let (_, t) = call(&app, OWNER, "GET", "/v1/library/tracks?limit=10", None).await;
    assert_eq!(
        t["tracks"].as_array().unwrap().len(),
        0,
        "a scan does not bring it back: {t}"
    );
    let (_, h) = call(&app, OWNER, "GET", "/v1/library/hidden", None).await;
    let path = h[0]["path"].as_str().unwrap().to_string();
    assert!(path.ends_with("one.wav"), "{h}");
    assert!(file.exists(), "the file stays");
    // Restore: the folder is read again and the song is back
    let (st, r) = call(
        &app,
        OWNER,
        "POST",
        "/v1/library/hidden/restore",
        Some(json!({"path": path})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["scan_started"], true, "{r}");
    scanned(&s).await;
    let mut back = 0;
    for _ in 0..200 {
        let (_, t) = call(&app, OWNER, "GET", "/v1/library/tracks?limit=10", None).await;
        back = t["tracks"].as_array().unwrap().len();
        if back == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(back, 1);
}
