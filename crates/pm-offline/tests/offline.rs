//! A device syncing with a stand-in node, then playing offline from its local server.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use pm_offline::{Event, Keep, Node, Store, SyncOptions};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Seen {
    plays: Vec<Value>,
    hearts_off: Vec<String>,
    ranges: Vec<(i64, Option<String>)>,
    manifests: Vec<Value>,
    devices: Vec<Value>,
}

type Fake = Arc<Mutex<Seen>>;

fn song(id: i64) -> Vec<u8> {
    format!("song-{id}-").repeat(100).into_bytes()
}

/// Album 7 holds songs 1 and 2.
fn kept_by(keep: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = keep["tracks"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if keep["albums"].as_array().is_some_and(|a| a.contains(&json!(7))) {
        ids.extend([1, 2]);
    }
    ids.sort();
    ids.dedup();
    ids
}

async fn manifest(State(f): State<Fake>, Json(b): Json<Value>) -> Json<Value> {
    f.lock().unwrap().manifests.push(b.clone());
    let kept = kept_by(&b["keep"]);
    let have: Vec<i64> = b["have"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h["track_id"].as_i64()).collect())
        .unwrap_or_default();
    let wanted: Vec<Value> = kept
        .iter()
        .filter(|i| !have.contains(i))
        .map(|i| json!({"track_id": i, "title": format!("Song {i}"), "bytes": song(*i).len()}))
        .collect();
    let unkept: Vec<i64> = have.iter().copied().filter(|i| !kept.contains(i)).collect();
    Json(json!({"wanted": wanted, "unkept": unkept}))
}

async fn track(Path(id): Path<i64>) -> Response {
    if id != 1 && id != 2 {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such track"}))).into_response();
    }
    Json(json!({
        "id": id, "title": format!("Song {id}"), "track_no": id, "disc_no": 1, "duration_ms": 1000,
        "artists": [{"id": 3, "name": "Artist"}],
        "album": {"id": 7, "title": "Album"},
        "file": {"path": format!("/srv/Owner's music/0{id}.flac"), "container": "flac", "size_bytes": song(id).len()}
    }))
    .into_response()
}

async fn download(State(f): State<Fake>, Path(id): Path<i64>, h: HeaderMap) -> Response {
    let range = h
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    f.lock().unwrap().ranges.push((id, range.clone()));
    let body = song(id);
    let etag = format!("\"e{id}\"");
    let if_range_ok = h.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) == Some(etag.as_str());
    if let (Some(r), true) = (range, if_range_ok) {
        let from: usize = r
            .trim_start_matches("bytes=")
            .trim_end_matches('-')
            .parse()
            .unwrap();
        return (
            StatusCode::PARTIAL_CONTENT,
            [(header::ETAG, etag)],
            body[from..].to_vec(),
        )
            .into_response();
    }
    ([(header::ETAG, etag)], body).into_response()
}

async fn history(State(f): State<Fake>, Json(b): Json<Value>) -> Response {
    if b["track_id"] == 555 {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "track 555"}))).into_response();
    }
    f.lock().unwrap().plays.push(b);
    Json(json!({"plays": []})).into_response()
}

fn fake_node(f: Fake) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<html>the web UI</html>") }),
        )
        .route("/app.js", get(|| async { "console.log('app')" }))
        .route(
            "/v1/info",
            get(|| async { Json(json!({"id": "n1", "name": "Home", "version": "0.1.0"})) }),
        )
        .route(
            "/v1/me",
            get(|| async { Json(json!({"role": "owner", "zones": []})) }),
        )
        .route(
            "/v1/devices/{id}",
            put(|State(f): State<Fake>, Json(b): Json<Value>| async move {
                f.lock().unwrap().devices.push(b);
                Json(json!({}))
            }),
        )
        .route("/v1/devices/{id}/downloads", put(manifest))
        .route("/v1/library/tracks/{id}", get(track))
        .route("/v1/library/tracks/{id}/download", get(download))
        .route(
            "/v1/library/albums/{id}/cover",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    vec![0x89u8, b'P', b'N', b'G'],
                )
            }),
        )
        .route(
            "/v1/playlists/{id}",
            get(|Path(id): Path<String>| async move {
                Json(json!({"summary": {"id": id, "name": "Mix"}, "tracks": [{"id": 2}, {"id": 99}]}))
            }),
        )
        .route(
            "/v1/favourites",
            get(|| async {
                Json(json!({"tracks": [{"id": 1}], "albums": [], "artists": [], "other": [], "uris": []}))
            })
            .delete(|State(f): State<Fake>, uri: axum::http::Uri| async move {
                f.lock()
                    .unwrap()
                    .hearts_off
                    .push(uri.query().unwrap_or_default().to_string());
                Json(json!({"favourite": false}))
            }),
        )
        .route("/v1/library/history", axum::routing::post(history))
        .with_state(f)
}

async fn start_node(f: Fake) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, fake_node(f)).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_device_syncs_then_plays_offline() {
    let seen: Fake = Arc::default();
    let base = start_node(seen.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    store
        .set_keep(Keep {
            albums: vec![7],
            playlists: vec!["p1".into()],
            favourites: true,
            ..Default::default()
        })
        .unwrap();
    // A download of song 2 that stopped after 300 bytes
    std::fs::write(dir.path().join("files/2.flac.part"), &song(2)[..300]).unwrap();
    std::fs::write(dir.path().join("files/2.flac.etag"), "\"e2\"").unwrap();
    // Done offline before this sync
    store.push(Event::Play { track_id: 1, at: 100 }).unwrap();
    store
        .push(Event::Play {
            track_id: 555,
            at: 101,
        })
        .unwrap();
    store
        .push(Event::Favourite {
            track_id: 1,
            on: false,
            at: 102,
        })
        .unwrap();

    let node = Node::new(&base, "tok").unwrap();
    let opts = SyncOptions {
        name: "Pixel 8".into(),
        kind: "android".into(),
    };
    let r = pm_offline::sync(&store, &node, &opts).await.unwrap();
    assert_eq!(r.sent, 3, "two sent, one refused for good (an unknown song)");
    assert_eq!(r.downloaded, [1, 2]);
    assert!(r.failed.is_empty(), "{:?}", r.failed);

    // The files, whole; song 2 resumed from byte 300
    assert_eq!(std::fs::read(dir.path().join("files/1.flac")).unwrap(), song(1));
    assert_eq!(std::fs::read(dir.path().join("files/2.flac")).unwrap(), song(2));
    {
        let s = seen.lock().unwrap();
        assert!(
            s.ranges.contains(&(2, Some("bytes=300-".into()))),
            "{:?}",
            s.ranges
        );
        assert!(s.ranges.contains(&(1, None)));
        assert_eq!(
            s.plays,
            [json!({"track_id": 1, "zone": "offline", "played_at": 100})]
        );
        assert_eq!(s.hearts_off, ["track_id=1"]);
        assert_eq!(s.devices, [json!({"name": "Pixel 8", "kind": "android"})]);
        let last = s.manifests.last().unwrap();
        assert_eq!(
            last["have"].as_array().unwrap().len(),
            2,
            "the node hears what the device has"
        );
        assert_eq!(last["keep"]["albums"], json!([7]));
    }
    assert!(store.queued().is_empty());
    let ix = store.index();
    assert_eq!(
        ix.tracks[&1].track["file"]["path"], "01.flac",
        "no owner's folders on the device"
    );
    assert_eq!(ix.covers[&7], "7.png");
    assert!(ix.playlists.contains_key("p1"));
    assert!(ix.favourite_changes.is_empty());
    assert!(ix.synced_at.is_some());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ui/index.html")).unwrap(),
        "<html>the web UI</html>"
    );

    // Offline: the local server
    let (addr, _task) = pm_offline::server::serve(store.clone()).await.unwrap();
    let local = format!("http://{addr}");
    let http = reqwest::Client::new();
    let info: Value = http
        .get(format!("{local}/v1/info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["offline"], true);
    assert_eq!(info["name"], "Home");
    assert_eq!(info["library"]["tracks"], 2);
    let off: Value = http
        .get(format!("{local}/v1/offline"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(off["albums"][0]["track_ids"], json!([1, 2]));
    assert_eq!(off["albums"][0]["cover"], true);
    assert_eq!(off["playlists"][0]["name"], "Mix");
    assert_eq!(
        off["playlists"][0]["track_ids"],
        json!([2]),
        "only what the device has"
    );
    assert_eq!(off["favourites"]["track_ids"], json!([1]));
    let part = http
        .get(format!("{local}/v1/library/tracks/2/file?token=x"))
        .header(header::RANGE, "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(
        part.headers()[header::CONTENT_RANGE],
        format!("bytes 0-9/{}", song(2).len())
    );
    assert_eq!(part.bytes().await.unwrap().as_ref(), &song(2)[..10]);
    let played = http
        .post(format!("{local}/v1/library/history"))
        .json(&json!({"track_id": 2, "zone": "browser"}))
        .send()
        .await
        .unwrap();
    assert_eq!(played.status(), 200);
    let hearted = http
        .put(format!("{local}/v1/favourites"))
        .json(&json!({"track_id": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(hearted.status(), 200);
    assert!(store.index().is_favourite(2));
    assert_eq!(store.queued().len(), 2);
    let page = http.get(format!("{local}/albums")).send().await.unwrap();
    assert_eq!(
        page.text().await.unwrap(),
        "<html>the web UI</html>",
        "the UI's routes get the page"
    );
    let away = http
        .post(format!("{local}/v1/zones/z/queue"))
        .send()
        .await
        .unwrap();
    assert_eq!(away.status(), 503);

    // Back online, keeping only song 1: song 2 goes, and the queue is sent
    store
        .set_keep(Keep {
            tracks: vec![1],
            ..Default::default()
        })
        .unwrap();
    let r = pm_offline::sync(&store, &node, &opts).await.unwrap();
    assert_eq!(r.sent, 2);
    assert_eq!(r.removed, [2]);
    assert!(!dir.path().join("files/2.flac").exists());
    assert_eq!(seen.lock().unwrap().plays.len(), 2);
    assert_eq!(store.index().favourites, None, "favourites no longer kept");
}

#[tokio::test]
async fn an_unreachable_node_keeps_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.push(Event::Play { track_id: 1, at: 1 }).unwrap();
    // Nothing listens there
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let node = Node::new(&base, "tok").unwrap();
    let opts = SyncOptions {
        name: "Laptop".into(),
        kind: "desktop".into(),
    };
    assert!(matches!(
        pm_offline::sync(&store, &node, &opts).await,
        Err(pm_offline::SyncError::Unreachable(_))
    ));
    assert_eq!(store.queued().len(), 1);
}
