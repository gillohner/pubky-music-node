//! Your Jellyfin (or Emby) server as a source (STEWARD-PLAN step 5.2). Sign in once with a user
//! and password: the node keeps only the access token Jellyfin hands out (never the password)
//! and sends it in the `Authorization: MediaBrowser … Token="…"` header.
//!
//! Songs queue as `jellyfin:track:<id>` and play the original file; browser streams and covers
//! are relayed, so the token never reaches the page. A song's MusicBrainz recording id
//! (`ProviderIds.MusicBrainzRecording`) makes it canonical for playlists (ADR 0001).

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;
use std::sync::{Arc, RwLock};

pub const SOURCE_ID: &str = "jellyfin";
pub const TRACK_URI_PREFIX: &str = "jellyfin:track:";
const SETTING: &str = "jellyfin.server";
const CLIENT: &str = "Pubky Music";
const DEVICE: &str = "pubky-music-node";

/// The signed-in server. `Debug` never shows the token.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Server {
    pub url: String,
    pub user_id: String,
    pub user_name: String,
    pub token: String,
    /// Random per node, as Jellyfin asks of each client device.
    pub device_id: String,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// `Jellyfin Server`, or Emby's product name.
    #[serde(default)]
    pub product: Option<String>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("url", &self.url)
            .field("user_name", &self.user_name)
            .field("token", &"<set>")
            .finish()
    }
}

pub type Shared = Arc<RwLock<Option<Server>>>;

pub fn load(lib: &pm_library::Library) -> Option<Server> {
    lib.get_setting(SETTING)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

pub fn track_uri(id: &str) -> String {
    format!("{TRACK_URI_PREFIX}{id}")
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("Jellyfin: {e}"))
}

/// Jellyfin ids are 32 hex digits (with or without dashes).
fn valid_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(ApiError::bad_request("bad Jellyfin id"));
    }
    Ok(id)
}

fn auth_header(device_id: &str, token: Option<&str>) -> String {
    let mut h = format!(
        "MediaBrowser Client=\"{CLIENT}\", Device=\"{DEVICE}\", DeviceId=\"{device_id}\", Version=\"{}\"",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(t) = token {
        h.push_str(&format!(", Token=\"{t}\""));
    }
    h
}

fn server(s: &NodeState) -> Result<Server, ApiError> {
    s.jellyfin.read().expect("jellyfin").clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "sign in to your Jellyfin first (Sources › Jellyfin)",
        )
    })
}

pub(crate) fn connected(s: &NodeState) -> bool {
    s.jellyfin.read().expect("jellyfin").is_some()
}

async fn get(s: &NodeState, srv: &Server, path: &str, query: &[(&str, &str)]) -> Result<Value, ApiError> {
    let resp = s
        .http_client()
        .get(format!("{}{path}", srv.url.trim_end_matches('/')))
        .header(
            reqwest::header::AUTHORIZATION,
            auth_header(&srv.device_id, Some(&srv.token)),
        )
        .query(query)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    match resp.status() {
        st if st.is_success() => resp.json().await.map_err(|e| upstream(e.without_url())),
        StatusCode::UNAUTHORIZED => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Jellyfin no longer accepts this sign-in: sign in again (Sources › Jellyfin)",
        )),
        StatusCode::NOT_FOUND => Err(ApiError::not_found("Jellyfin has no such item")),
        st => Err(upstream(format!("{path} answered {st}"))),
    }
}

fn cover_path(id: &str) -> String {
    format!("/v1/sources/jellyfin/cover/{id}")
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// A song's canonical identity: its MusicBrainz recording (`MusicBrainzTrack` is a release
/// track, not a recording, so it is not used).
pub(crate) fn canonical_song(v: &Value) -> Option<pm_model::MusicUri> {
    let m = v["ProviderIds"]["MusicBrainzRecording"].as_str()?;
    pm_model::Mbid::parse(m).ok().map(pm_model::MusicUri::recording)
}

fn artist_of(v: &Value) -> Option<String> {
    v["Artists"]
        .as_array()
        .filter(|a| !a.is_empty())
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
        .or_else(|| str_of(v, "AlbumArtist"))
}

fn duration_ms(v: &Value) -> Option<u64> {
    v["RunTimeTicks"].as_u64().map(|t| t / 10_000)
}

/// The item's own picture, else its album's.
fn image_of(v: &Value) -> Option<String> {
    if v["ImageTags"]["Primary"].is_string() {
        return str_of(v, "Id").map(|id| cover_path(&id));
    }
    if v["AlbumPrimaryImageTag"].is_string() {
        return str_of(v, "AlbumId").map(|id| cover_path(&id));
    }
    None
}

pub(crate) fn hit(v: &Value) -> Option<SourceHit> {
    let id = str_of(v, "Id")?;
    let kind = match v["Type"].as_str()? {
        "Audio" => "track",
        "MusicAlbum" => "album",
        _ => return None,
    };
    Some(SourceHit {
        id: format!("{kind}:{id}"),
        title: str_of(v, "Name")?,
        creator: if kind == "album" {
            str_of(v, "AlbumArtist").or_else(|| artist_of(v))
        } else {
            artist_of(v)
        },
        year: v["ProductionYear"].as_i64().map(|y| y as i32),
        downloads: v["UserData"]["PlayCount"].as_u64(),
        image: image_of(v),
        kind: kind.into(),
        duration_ms: duration_ms(v),
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: (kind == "track").then(|| track_uri(&id)),
        url: match kind {
            "track" => canonical_song(v).map(|u| u.to_url()).unwrap_or_default(),
            _ => v["ProviderIds"]["MusicBrainzAlbum"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map(|m| format!("https://musicbrainz.org/release/{m}"))
                .unwrap_or_default(),
        },
    })
}

fn source_track(v: &Value, n: u32) -> Option<SourceTrack> {
    let id = str_of(v, "Id")?;
    let src = &v["MediaSources"][0];
    let fmt = match (src["Container"].as_str(), src["Bitrate"].as_u64()) {
        (Some(c), Some(b)) => format!("{} {} kbps (Jellyfin)", c.to_uppercase(), b / 1000),
        (Some(c), None) => format!("{} (Jellyfin)", c.to_uppercase()),
        _ => "Jellyfin".into(),
    };
    Some(SourceTrack {
        title: str_of(v, "Name")?,
        track_no: v["IndexNumber"].as_u64().map(|t| t as u32).or(Some(n)),
        duration_ms: duration_ms(v),
        format: fmt,
        url: track_uri(&id),
        artist: artist_of(v),
    })
}

const FIELDS: &str = "ProviderIds,MediaSources";

pub struct Jellyfin;

#[async_trait::async_trait]
impl StreamingService for Jellyfin {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play", "playlists"]
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let srv = s.jellyfin.read().expect("jellyfin").clone();
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Jellyfin".into(),
            description: match &srv {
                Some(v) => format!(
                    "Your Jellyfin{} at {}",
                    v.server_name
                        .as_ref()
                        .map(|n| format!(" “{n}”"))
                        .unwrap_or_default(),
                    v.url
                ),
                None => "Your Jellyfin or Emby server's music".into(),
            },
            status: if srv.is_some() { "ready" } else { "needs_config" }.into(),
        }
    }

    fn unified(&self, s: &NodeState) -> bool {
        connected(s)
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let srv = server(s)?;
        let n = limit.to_string();
        let v = get(
            s,
            &srv,
            "/Items",
            &[
                ("userId", srv.user_id.as_str()),
                ("searchTerm", term),
                ("IncludeItemTypes", "MusicAlbum,Audio"),
                ("Recursive", "true"),
                ("Limit", &n),
                ("Fields", FIELDS),
            ],
        )
        .await?;
        let mut out: Vec<SourceHit> = v["Items"]
            .as_array()
            .map(|a| a.iter().filter_map(hit).collect())
            .unwrap_or_default();
        // Albums first, as the other sources list them
        out.sort_by_key(|h| h.kind != "album");
        Ok(out)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let srv = server(s)?;
        let (kind, raw) = id.split_once(':').unwrap_or(("track", id));
        let raw = valid_id(raw)?;
        let it = get(
            s,
            &srv,
            &format!("/Items/{raw}"),
            &[("userId", srv.user_id.as_str()), ("Fields", FIELDS)],
        )
        .await?;
        match kind {
            "album" => {
                let v = get(
                    s,
                    &srv,
                    "/Items",
                    &[
                        ("userId", srv.user_id.as_str()),
                        ("ParentId", raw),
                        ("IncludeItemTypes", "Audio"),
                        ("SortBy", "ParentIndexNumber,IndexNumber,SortName"),
                        ("Fields", FIELDS),
                    ],
                )
                .await?;
                let tracks = v["Items"]
                    .as_array()
                    .map(|l| {
                        l.iter()
                            .enumerate()
                            .filter_map(|(i, t)| source_track(t, i as u32 + 1))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(SourceItem {
                    source: SOURCE_ID.into(),
                    id: format!("album:{raw}"),
                    title: str_of(&it, "Name").unwrap_or_else(|| "Untitled".into()),
                    creator: str_of(&it, "AlbumArtist").or_else(|| artist_of(&it)),
                    year: it["ProductionYear"].as_i64().map(|y| y as i32),
                    description: None,
                    licence: None,
                    image: image_of(&it),
                    url: String::new(),
                    tracks,
                    library_album_id: None,
                })
            }
            "track" => Ok(SourceItem {
                source: SOURCE_ID.into(),
                id: format!("track:{raw}"),
                title: str_of(&it, "Album")
                    .or_else(|| str_of(&it, "Name"))
                    .unwrap_or_else(|| "Untitled".into()),
                creator: artist_of(&it),
                year: it["ProductionYear"].as_i64().map(|y| y as i32),
                description: None,
                licence: None,
                image: image_of(&it),
                url: canonical_song(&it).map(|u| u.to_url()).unwrap_or_default(),
                tracks: source_track(&it, 1).into_iter().collect(),
                library_album_id: None,
            }),
            _ => Err(ApiError::bad_request(
                "Jellyfin items are track:<id> or album:<id>",
            )),
        }
    }
}

// ---- playback ----

fn stream_url(srv: &Server, id: &str) -> String {
    format!("{}/Audio/{id}/stream?static=true", srv.url.trim_end_matches('/'))
}

/// Open `jellyfin:track:<id>` for a zone: the original file, the token in the header.
pub(crate) fn open_track_blocking(
    shared: &Shared,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: track_uri(id),
        reason,
    };
    valid_id(id).map_err(|e| fail(e.message))?;
    let srv = shared
        .read()
        .map_err(|_| fail("server settings".into()))?
        .clone()
        .ok_or_else(|| fail("no Jellyfin is connected".into()))?;
    // The HTTP reader takes a URL only: the token goes in Jellyfin's `api_key` query here,
    // kept inside the node.
    let url = format!("{}&api_key={}", stream_url(&srv, id), srv.token);
    crate::sources::open_http_stream(&url).map_err(|e| match e {
        pm_zones::SourceError::Open { reason, .. } => fail(reason.replace(&srv.token, "<token>")),
        other => other,
    })
}

async fn relay(
    s: &NodeState,
    url: String,
    fallback_type: &str,
) -> Result<axum::response::Response, ApiError> {
    let srv = server(s)?;
    let resp = s
        .http_client()
        .get(url)
        .header(
            reqwest::header::AUTHORIZATION,
            auth_header(&srv.device_id, Some(&srv.token)),
        )
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(if resp.status() == StatusCode::NOT_FOUND {
            ApiError::not_found("Jellyfin has no such item")
        } else {
            upstream(format!("answered {}", resp.status()))
        });
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(fallback_type)
        .to_string();
    let mut out = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, ctype)
        .header(axum::http::header::CACHE_CONTROL, "private, max-age=3600");
    if let Some(n) = resp.content_length() {
        out = out.header(axum::http::header::CONTENT_LENGTH, n);
    }
    out.body(axum::body::Body::from_stream(resp.bytes_stream()))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// `GET /v1/sources/jellyfin/stream/{id}`: the original file for browsers.
pub(crate) async fn stream(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let id = valid_id(&id)?.to_string();
    let srv = server(&s)?;
    relay(&s, stream_url(&srv, &id), "audio/flac").await
}

/// `GET /v1/sources/jellyfin/cover/{id}`.
pub(crate) async fn cover(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let id = valid_id(&id)?.to_string();
    let srv = server(&s)?;
    let url = format!(
        "{}/Items/{id}/Images/Primary?maxHeight=600",
        srv.url.trim_end_matches('/')
    );
    relay(&s, url, "image/jpeg").await
}

// ---- signing in ----

fn view(srv: Option<&Server>) -> JellyfinView {
    JellyfinView {
        connected: srv.is_some(),
        url: srv.map(|v| v.url.clone()),
        user: srv.map(|v| v.user_name.clone()),
        server_name: srv.and_then(|v| v.server_name.clone()),
        version: srv.and_then(|v| v.version.clone()),
    }
}

/// `GET /v1/sources/jellyfin/server`: never the token.
pub(crate) async fn get_server(State(s): State<Arc<NodeState>>) -> ApiResult<JellyfinView> {
    Ok(Json(view(s.jellyfin.read().expect("jellyfin").as_ref())))
}

/// `PUT /v1/sources/jellyfin/server {url, user, password}`: signs in once; the password is used
/// for that request only, then dropped. The access token is what the node keeps.
pub(crate) async fn sign_in(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<JellyfinSignIn>,
) -> ApiResult<JellyfinView> {
    let url = body.url.trim().trim_end_matches('/').to_string();
    let parsed = url::Url::parse(&url)
        .map_err(|_| ApiError::bad_request("the server address must be an http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("the server address must be an http(s) URL"));
    }
    if body.user.trim().is_empty() {
        return Err(ApiError::bad_request("a user name is required"));
    }
    let device_id = s
        .jellyfin
        .read()
        .expect("jellyfin")
        .as_ref()
        .map(|v| v.device_id.clone())
        .unwrap_or_else(|| crate::tidal::random_urlsafe(16));
    let info: Value = s
        .http_client()
        .get(format!("{url}/System/Info/Public"))
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?
        .json()
        .await
        .map_err(|_| upstream("this address does not answer like a Jellyfin or Emby server"))?;
    let resp = s
        .http_client()
        .post(format!("{url}/Users/AuthenticateByName"))
        .header(reqwest::header::AUTHORIZATION, auth_header(&device_id, None))
        .json(&serde_json::json!({"Username": body.user.trim(), "Pw": body.password}))
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if resp.status() == StatusCode::UNAUTHORIZED {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "Jellyfin refused this user name or password",
        ));
    }
    if !resp.status().is_success() {
        return Err(upstream(format!("sign-in answered {}", resp.status())));
    }
    let v: Value = resp.json().await.map_err(|e| upstream(e.without_url()))?;
    let srv = Server {
        url,
        user_id: str_of(&v["User"], "Id").ok_or_else(|| upstream("no user id in the answer"))?,
        user_name: str_of(&v["User"], "Name").unwrap_or_else(|| body.user.trim().to_string()),
        token: str_of(&v, "AccessToken").ok_or_else(|| upstream("no access token in the answer"))?,
        device_id,
        server_name: str_of(&info, "ServerName"),
        version: str_of(&info, "Version"),
        product: str_of(&info, "ProductName"),
    };
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(SETTING, &serde_json::to_string(&srv).expect("serializes"))?;
    *s.jellyfin.write().expect("jellyfin") = Some(srv.clone());
    tracing::info!(url = %srv.url, user = %srv.user_name, "Jellyfin signed in");
    Ok(Json(view(Some(&srv))))
}

/// `DELETE /v1/sources/jellyfin/server`: signs out (Jellyfin drops the token) and forgets it.
pub(crate) async fn sign_out(State(s): State<Arc<NodeState>>) -> ApiResult<JellyfinView> {
    let current = s.jellyfin.read().expect("jellyfin").clone();
    if let Some(srv) = current {
        let _ = s
            .http_client()
            .post(format!("{}/Sessions/Logout", srv.url.trim_end_matches('/')))
            .header(
                reqwest::header::AUTHORIZATION,
                auth_header(&srv.device_id, Some(&srv.token)),
            )
            .send()
            .await;
    }
    s.library
        .lock()
        .expect("library poisoned")
        .delete_setting(SETTING)?;
    *s.jellyfin.write().expect("jellyfin") = None;
    Ok(Json(view(None)))
}

/// `GET /v1/sources/jellyfin/playlists`.
pub(crate) async fn playlists(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SubsonicPlaylist>> {
    let srv = server(&s)?;
    let v = get(
        &s,
        &srv,
        "/Items",
        &[
            ("userId", srv.user_id.as_str()),
            ("IncludeItemTypes", "Playlist"),
            ("Recursive", "true"),
            ("Fields", "ChildCount"),
        ],
    )
    .await?;
    Ok(Json(
        v["Items"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter(|p| p["MediaType"].as_str().is_none_or(|m| m == "Audio"))
                    .filter_map(|p| {
                        Some(SubsonicPlaylist {
                            id: str_of(p, "Id")?,
                            name: str_of(p, "Name").unwrap_or_else(|| "Playlist".into()),
                            songs: p["ChildCount"].as_u64().unwrap_or(0) as usize,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ))
}

/// `POST /v1/sources/jellyfin/playlists/{id}/import`: a private Pubky playlist of the songs
/// Jellyfin knows by MusicBrainz recording.
pub(crate) async fn import_playlist(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<crate::tidal::ImportReport> {
    let srv = server(&s)?;
    let id = valid_id(&id)?.to_string();
    let head = get(
        &s,
        &srv,
        &format!("/Items/{id}"),
        &[("userId", srv.user_id.as_str())],
    )
    .await?;
    let v = get(
        &s,
        &srv,
        &format!("/Playlists/{id}/Items"),
        &[("userId", srv.user_id.as_str()), ("Fields", FIELDS)],
    )
    .await?;
    let mut uris = Vec::new();
    let mut skipped = 0;
    for e in v["Items"].as_array().cloned().unwrap_or_default() {
        match canonical_song(&e) {
            Some(u) => uris.push(u.to_url()),
            None => skipped += 1,
        }
    }
    if uris.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "none of the playlist's songs has a MusicBrainz recording id in Jellyfin: tag the files (Picard) and refresh the library",
        ));
    }
    let imported = uris.len();
    let Json(detail) = crate::playlists::create(
        State(s.clone()),
        Json(CreatePlaylist {
            name: str_of(&head, "Name").unwrap_or_else(|| "Playlist".into()),
            description: str_of(&head, "Overview"),
            image: None,
            uris,
            track_ids: Vec::new(),
            sources: Vec::new(),
            ..Default::default()
        }),
    )
    .await?;
    Ok(Json(crate::tidal::ImportReport {
        imported,
        skipped,
        playlist: Some(detail),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_map_and_never_carry_the_token() {
        let song = serde_json::json!({"Id": "0123456789abcdef0123456789abcdef", "Name": "Bohemian Rhapsody", "Type": "Audio",
            "Artists": ["Queen"], "Album": "A Night at the Opera", "AlbumId": "fedcba9876543210fedcba9876543210",
            "AlbumPrimaryImageTag": "x", "RunTimeTicks": 3_540_000_000u64, "ProductionYear": 1975, "IndexNumber": 11,
            "ProviderIds": {"MusicBrainzTrack": "00000000-0000-0000-0000-000000000000", "MusicBrainzRecording": "b1a9c0e9-d987-4042-ae91-78d6a3267d69"},
            "MediaSources": [{"Container": "flac", "Bitrate": 1_000_000}]});
        let h = hit(&song).unwrap();
        assert_eq!(
            h.uri.as_deref(),
            Some("jellyfin:track:0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            h.url,
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"
        );
        assert_eq!(h.duration_ms, Some(354_000));
        assert_eq!(
            h.image.as_deref(),
            Some("/v1/sources/jellyfin/cover/fedcba9876543210fedcba9876543210")
        );
        let t = source_track(&song, 1).unwrap();
        assert_eq!(
            (t.track_no, t.format.as_str()),
            (Some(11), "FLAC 1000 kbps (Jellyfin)")
        );
        let track_only = serde_json::json!({"Id": "a", "Name": "x", "Type": "Audio", "ProviderIds": {"MusicBrainzTrack": "b1a9c0e9-d987-4042-ae91-78d6a3267d69"}});
        assert!(
            canonical_song(&track_only).is_none(),
            "a release track id is not a recording"
        );
        assert!(hit(&serde_json::json!({"Id": "a", "Name": "Video", "Type": "Movie"})).is_none());
        let srv = Server {
            token: "T0KEN".into(),
            ..Default::default()
        };
        assert!(!format!("{srv:?}").contains("T0KEN"));
        assert!(valid_id("../etc").is_err());
    }
}
