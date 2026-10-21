//! ListenBrainz (STEWARD-PLAN step 5.5): the listen history both ways, and its recommendations.
//!
//! - **Send:** what the node's zones play goes to ListenBrainz with the user's token: "playing now"
//!   when a song starts, a listen once it has played half its length or four minutes (their
//!   rule). Listens that cannot be sent wait in a queue and go with the next one.
//! - **Import:** the user's listens come into the node's history (private `listen` posts,
//!   ADR 0014), with their MusicBrainz recording when ListenBrainz knows it.
//! - **For you:** recommended recordings and the "created for you" playlists (Weekly Jams, Daily
//!   Jams, Weekly Exploration), which play through the resolver and import as Pubky playlists.
//!
//! The token is kept in the node's database and never returned by the API.

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

const ACCOUNT_KEY: &str = "listenbrainz.account";
const PENDING_KEY: &str = "listenbrainz.pending";
const IMPORTED_KEY: &str = "listenbrainz.imported_until";
const PENDING_MAX: usize = 1000;
pub const API_BASE: &str = "https://api.listenbrainz.org";

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Account {
    token: String,
    user: String,
    #[serde(default = "yes")]
    submit: bool,
}

fn yes() -> bool {
    true
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("ListenBrainz: {e}"))
}

fn base(s: &NodeState) -> String {
    s.listenbrainz_base
        .read()
        .expect("listenbrainz base")
        .trim_end_matches('/')
        .to_string()
}

fn account(s: &NodeState) -> Option<Account> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(ACCOUNT_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

fn need_account(s: &NodeState) -> Result<Account, ApiError> {
    account(s).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "connect ListenBrainz first (Settings › ListenBrainz)",
        )
    })
}

fn pending(s: &NodeState) -> Vec<Value> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(PENDING_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

fn set_pending(s: &NodeState, list: &[Value]) {
    let lib = s.library.lock().expect("library poisoned");
    let _ = if list.is_empty() {
        lib.delete_setting(PENDING_KEY).map(|_| ())
    } else {
        lib.set_setting(PENDING_KEY, &Value::from(list.to_vec()).to_string())
    };
}

async fn get_json(
    s: &NodeState,
    path: &str,
    token: Option<&str>,
    query: &[(&str, String)],
) -> Result<Option<Value>, ApiError> {
    let mut req = s.http_client().get(format!("{}{path}", base(s))).query(query);
    if let Some(t) = token {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Token {t}"));
    }
    let resp = req.send().await.map_err(|e| upstream(e.without_url()))?;
    match resp.status() {
        // Recommendations not computed yet
        StatusCode::NO_CONTENT => Ok(None),
        StatusCode::NOT_FOUND => Err(ApiError::not_found("ListenBrainz has no such user or playlist")),
        st if st.is_success() => Ok(Some(resp.json().await.map_err(|e| upstream(e.without_url()))?)),
        st => Err(upstream(format!("{path} answered {st}"))),
    }
}

/// `POST /1/submit-listens`; `false` when ListenBrainz could not take them (kept for later).
async fn submit(s: &NodeState, token: &str, listen_type: &str, payload: Vec<Value>) -> Result<(), String> {
    let resp = s
        .http_client()
        .post(format!("{}/1/submit-listens", base(s)))
        .header(reqwest::header::AUTHORIZATION, format!("Token {token}"))
        .json(&json!({"listen_type": listen_type, "payload": payload}))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("answered {}", resp.status()))
    }
}

/// The ListenBrainz form of a queue item: names from the item, identity from the library.
fn listen_of(s: &NodeState, item: &pm_zones::QueueItem) -> Option<Value> {
    let mut artist = item.artist.clone().filter(|a| !a.trim().is_empty());
    let mut title = item.title.clone().filter(|t| !t.trim().is_empty());
    let mut album = item.album.clone();
    let mut extra = serde_json::Map::new();
    extra.insert("media_player".into(), json!("Pubky Music"));
    extra.insert("submission_client".into(), json!("pubky-music-node"));
    extra.insert(
        "submission_client_version".into(),
        json!(env!("CARGO_PKG_VERSION")),
    );
    if let Some(d) = item.duration_ms {
        extra.insert("duration_ms".into(), json!(d));
    }
    let service = item.uri.split(':').next().unwrap_or("");
    match service {
        "track" => {
            let id = item
                .uri
                .strip_prefix(crate::sources::TRACK_SCHEME)?
                .parse::<i64>()
                .ok()?;
            let t = s
                .library
                .lock()
                .expect("library poisoned")
                .get_track(pm_model::TrackId(id))
                .ok()??;
            title = Some(t.title.clone());
            artist = Some(
                t.artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .filter(|a| !a.is_empty());
            album = t.album.as_ref().map(|a| a.title.clone());
            if let Some(m) = t.recording_mbid {
                extra.insert("recording_mbid".into(), json!(m.to_string()));
            }
            if let Some(i) = &t.isrc {
                extra.insert("isrc".into(), json!(i.to_string()));
            }
            if let Some(d) = t.duration_ms {
                extra.insert("duration_ms".into(), json!(d));
            }
        }
        "tidal" => {
            extra.insert("music_service".into(), json!("tidal.com"));
        }
        "jamendo" => {
            extra.insert("music_service".into(), json!("jamendo.com"));
        }
        "deezer" => {
            extra.insert("music_service".into(), json!("deezer.com"));
        }
        _ => {}
    }
    let mut meta = json!({"artist_name": artist?, "track_name": title?, "additional_info": extra});
    if let Some(a) = album.filter(|a| !a.trim().is_empty()) {
        meta["release_name"] = json!(a);
    }
    Some(json!({"track_metadata": meta}))
}

/// Plays in progress: (zone, queue entry) → the listen to send and whether it went.
/// Keyed by node, zone and queue entry (several nodes can share a process: tests, the app).
type Playing = std::collections::HashMap<(String, String, u64), (Value, Arc<std::sync::atomic::AtomicBool>)>;

fn playing() -> &'static std::sync::Mutex<Playing> {
    static P: std::sync::OnceLock<std::sync::Mutex<Playing>> = std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

/// Send the listen for this play once, whichever of "played long enough" and "played to the
/// end" comes first.
fn count_once(s: &Arc<NodeState>, key: &(String, String, u64)) {
    let Some((listen, sent)) = playing().lock().expect("playing").get(key).cloned() else {
        return;
    };
    if sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(acct) = account(s).filter(|a| a.submit) else {
        return;
    };
    let s = s.clone();
    tokio::spawn(async move { send_listens(&s, &acct.token, vec![listen]).await });
}

/// A zone started `item`: "playing now", then a listen once it has played half its length or
/// four minutes (ListenBrainz's rule), or when it plays to its end, whichever comes first.
pub(crate) fn track_started(s: &Arc<NodeState>, zone: Arc<pm_zones::Zone>, item: pm_zones::QueueItem) {
    let Some(acct) = account(s).filter(|a| a.submit) else {
        return;
    };
    let Some(mut listen) = listen_of(s, &item) else {
        return;
    };
    let now_playing = listen.clone();
    listen["listened_at"] = json!(pm_library::db::now_unix());
    let key = (s.node_id.clone(), zone.id().0.clone(), item.id);
    playing()
        .lock()
        .expect("playing")
        .insert(key.clone(), (listen.clone(), Default::default()));
    let s = s.clone();
    tokio::spawn(async move {
        let _ = submit(&s, &acct.token, "playing_now", vec![now_playing]).await;
        let length = item
            .duration_ms
            .or_else(|| listen["track_metadata"]["additional_info"]["duration_ms"].as_u64());
        let need_ms = length.map_or(240_000, |d| (d / 2).min(240_000));
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let st = zone.state();
            match st
                .now_playing
                .filter(|np| np.item.id == item.id && np.item.uri == item.uri)
            {
                Some(np) if np.position_ms >= need_ms => {
                    count_once(&s, &key);
                    break;
                }
                Some(_) => {}
                None => break, // ended (counted there) or skipped before it counted
            }
        }
        playing().lock().expect("playing").remove(&key);
    });
}

/// A zone played `item` to its end: that counts.
pub(crate) fn track_ended(s: &Arc<NodeState>, zone: &pm_zones::ZoneId, item: &pm_zones::QueueItem) {
    count_once(s, &(s.node_id.clone(), zone.0.clone(), item.id));
}

/// Send listens with any that are waiting; keep them all when ListenBrainz cannot take them.
async fn send_listens(s: &NodeState, token: &str, new: Vec<Value>) {
    let mut all = pending(s);
    all.extend(new);
    let mut left: Vec<Value> = Vec::new();
    // ListenBrainz takes up to 1000 per import; listens are small
    for chunk in all.chunks(100) {
        let kind = if chunk.len() == 1 { "single" } else { "import" };
        if let Err(e) = submit(s, token, kind, chunk.to_vec()).await {
            tracing::debug!("listenbrainz: {e}; keeping {} listens for later", chunk.len());
            left.extend_from_slice(chunk);
        }
    }
    let skip = left.len().saturating_sub(PENDING_MAX);
    set_pending(s, &left[skip..]);
}

fn view(s: &NodeState) -> ListenBrainzView {
    let a = account(s);
    ListenBrainzView {
        connected: a.is_some(),
        user: a.as_ref().map(|a| a.user.clone()),
        submit: a.as_ref().is_some_and(|a| a.submit),
        pending: pending(s).len(),
    }
}

/// `GET /v1/listenbrainz`.
pub(crate) async fn status(State(s): State<Arc<NodeState>>) -> ApiResult<ListenBrainzView> {
    Ok(Json(view(&s)))
}

/// `PUT /v1/listenbrainz {token?, submit?}`: connect (the token is checked with ListenBrainz),
/// or switch sending on and off.
pub(crate) async fn connect(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<ListenBrainzForm>,
) -> ApiResult<ListenBrainzView> {
    let mut acct = match body.token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(token) => {
            let v = get_json(&s, "/1/validate-token", Some(token), &[])
                .await?
                .unwrap_or_default();
            if v["valid"].as_bool() != Some(true) {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "ListenBrainz does not know this token",
                ));
            }
            Account {
                token: token.to_string(),
                user: v["user_name"].as_str().unwrap_or("").to_string(),
                submit: true,
            }
        }
        None => need_account(&s)?,
    };
    if let Some(on) = body.submit {
        acct.submit = on;
    }
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(ACCOUNT_KEY, &serde_json::to_string(&acct).expect("serializes"))?;
    if acct.submit && !pending(&s).is_empty() {
        send_listens(&s, &acct.token, Vec::new()).await;
    }
    Ok(Json(view(&s)))
}

/// `DELETE /v1/listenbrainz`: forget the token (and the waiting listens).
pub(crate) async fn disconnect(State(s): State<Arc<NodeState>>) -> ApiResult<ListenBrainzView> {
    {
        let lib = s.library.lock().expect("library poisoned");
        lib.delete_setting(ACCOUNT_KEY)?;
        lib.delete_setting(PENDING_KEY)?;
    }
    Ok(Json(view(&s)))
}

/// `POST /v1/listenbrainz/import`: the user's listens since the last import (at most 1000 per
/// run) into the node's history, by MusicBrainz recording.
pub(crate) async fn import_listens(State(s): State<Arc<NodeState>>) -> ApiResult<ListenImport> {
    let acct = need_account(&s)?;
    let since: i64 = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(IMPORTED_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut max_ts: Option<i64> = None;
    let mut newest = since;
    let mut out = ListenImport::default();
    'pages: for _ in 0..10 {
        let mut q: Vec<(&str, String)> = vec![("count", "100".into())];
        if let Some(m) = max_ts {
            q.push(("max_ts", m.to_string()));
        }
        let v = get_json(
            &s,
            &format!("/1/user/{}/listens", acct.user),
            Some(&acct.token),
            &q,
        )
        .await?
        .unwrap_or_default();
        let listens = v["payload"]["listens"].as_array().cloned().unwrap_or_default();
        if listens.is_empty() {
            break;
        }
        for l in &listens {
            let at = l["listened_at"].as_i64().unwrap_or(0);
            if at <= since {
                break 'pages;
            }
            newest = newest.max(at);
            max_ts = Some(max_ts.map_or(at, |m| m.min(at)));
            let meta = &l["track_metadata"];
            let mbid = meta["mbid_mapping"]["recording_mbid"]
                .as_str()
                .or(meta["additional_info"]["recording_mbid"].as_str())
                .and_then(|m| pm_model::Mbid::parse(m).ok());
            let Some(mbid) = mbid else {
                out.skipped += 1;
                continue;
            };
            let uri = pm_model::MusicUri::recording(mbid);
            let mut a = pm_pubky::Activity::new(
                &uri,
                pm_pubky::ActivitySource::Service {
                    name: "ListenBrainz".into(),
                },
                meta["additional_info"]["duration_ms"].as_u64(),
            );
            a.played_at = at * 1_000_000;
            let post = pm_pubky::MusicPost::new(pm_pubky::MusicContent::Activity(a));
            let id = post.create_id();
            if post.validate(Some(&id)).is_err() {
                out.skipped += 1;
                continue;
            }
            let lib = s.library.lock().expect("library poisoned");
            if lib.get_music_post(&id)?.is_none() {
                crate::playlists::store(&lib, &post, &id, pm_pubky::Visibility::Private)?;
                out.imported += 1;
            }
        }
        if listens.len() < 100 {
            break;
        }
    }
    if newest > since {
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(IMPORTED_KEY, &newest.to_string())?;
    }
    s.notify_library();
    Ok(Json(out))
}

/// `GET /v1/listenbrainz/recommendations?count=`: recordings ListenBrainz suggests, described
/// like any song (title, artist, where it plays here). Public: the user name is enough.
pub(crate) async fn recommendations(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<Limit>,
) -> ApiResult<Vec<DiscoverItem>> {
    let acct = need_account(&s)?;
    let count = q.limit.unwrap_or(25).clamp(1, 100).to_string();
    let Some(v) = get_json(
        &s,
        &format!("/1/cf/recommendation/user/{}/recording", acct.user),
        None,
        &[("count", count)],
    )
    .await?
    else {
        return Ok(Json(Vec::new()));
    };
    let uris: Vec<String> = v["payload"]["mbids"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|m| m["recording_mbid"].as_str())
                .filter_map(|m| pm_model::Mbid::parse(m).ok())
                .map(|m| pm_model::MusicUri::recording(m).to_url())
                .collect()
        })
        .unwrap_or_default();
    let mut items = crate::discover::describe_uris(&s, &uris).await;
    // ListenBrainz names its own recordings in one call (and points at their covers)
    let missing: Vec<String> = items
        .iter()
        .filter(|it| it.title.is_none() || it.image.is_none())
        .filter_map(|it| {
            it.uri
                .strip_prefix("https://musicbrainz.org/recording/")
                .map(str::to_string)
        })
        .collect();
    if !missing.is_empty() {
        if let Ok(Some(meta)) = get_json(
            &s,
            "/1/metadata/recording/",
            None,
            &[
                ("recording_mbids", missing.join(",")),
                ("inc", "artist release".into()),
            ],
        )
        .await
        {
            for it in items.iter_mut() {
                let Some(m) = it
                    .uri
                    .strip_prefix("https://musicbrainz.org/recording/")
                    .map(|id| &meta[id])
                    .filter(|m| m.is_object())
                else {
                    continue;
                };
                if it.title.is_none() {
                    it.title = m["recording"]["name"].as_str().map(str::to_string);
                }
                if it.artist.is_none() {
                    it.artist = m["artist"]["name"].as_str().map(str::to_string);
                }
                if it.image.is_none() {
                    it.image = m["release"]["caa_release_mbid"]
                        .as_str()
                        .filter(|r| m["release"]["caa_id"].is_number() && !r.is_empty())
                        .map(|r| format!("https://coverartarchive.org/release/{r}/front-250"));
                }
            }
        }
    }
    Ok(Json(items))
}

/// `GET /v1/listenbrainz/playlists`: the playlists ListenBrainz made for the user.
pub(crate) async fn playlists(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SubsonicPlaylist>> {
    let acct = need_account(&s)?;
    let v = get_json(
        &s,
        &format!("/1/user/{}/playlists/createdfor", acct.user),
        None,
        &[("count", "25".into())],
    )
    .await?
    .unwrap_or_default();
    Ok(Json(
        v["playlists"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|p| {
                        let p = &p["playlist"];
                        Some(SubsonicPlaylist {
                            id: p["identifier"].as_str()?.rsplit('/').next()?.to_string(),
                            name: p["title"].as_str().unwrap_or("Playlist").to_string(),
                            songs: p["track"].as_array().map_or(0, Vec::len),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ))
}

/// `POST /v1/listenbrainz/playlists/{mbid}/import`: a private Pubky playlist of its recordings.
pub(crate) async fn import_playlist(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<crate::tidal::ImportReport> {
    need_account(&s)?;
    let id = pm_model::Mbid::parse(&id)
        .map_err(|_| ApiError::bad_request("ListenBrainz playlists are named by a MusicBrainz id"))?
        .to_string();
    let v = get_json(&s, &format!("/1/playlist/{id}"), None, &[])
        .await?
        .unwrap_or_default();
    let p = &v["playlist"];
    let mut uris = Vec::new();
    let mut skipped = 0;
    for t in p["track"].as_array().cloned().unwrap_or_default() {
        let found = t["identifier"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(std::iter::once(&t["identifier"]))
            .filter_map(Value::as_str)
            .find_map(|u| pm_model::MusicUri::parse(u).ok());
        match found {
            Some(u) => uris.push(u.to_url()),
            None => skipped += 1,
        }
    }
    if uris.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the playlist has no songs to import",
        ));
    }
    let imported = uris.len();
    let Json(detail) = crate::playlists::create(
        State(s.clone()),
        Json(CreatePlaylist {
            name: p["title"].as_str().unwrap_or("ListenBrainz playlist").to_string(),
            description: Some("From ListenBrainz".into()),
            image: None,
            uris,
            track_ids: Vec::new(),
            sources: Vec::new(),
        }),
    )
    .await?;
    Ok(Json(crate::tidal::ImportReport {
        imported,
        skipped,
        playlist: Some(detail),
    }))
}
