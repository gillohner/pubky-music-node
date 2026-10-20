//! Client for the node's `/v1` API (see `docs/api.md`): the HTTP resources
//! as async methods and the `/v1/events` WebSocket as a stream of
//! [`ApiEvent`]s. The controller (`pm-app`) and tests use it; a future
//! remote-access layer (M5) only changes how the socket is reached.

use futures_util::StreamExt;
pub use pm_api::radio::Station as RadioStation;
use pm_api::radio::{PlayStation, RadioSearch, Station, Stations};
use pm_api::types::*;
use pm_model::{Album, Track, Volume};
use pm_zones::{OutputInfo, QueueItem, Repeat, ZoneState};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid node URL: {0}")]
    Url(String),
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("{status}: {message}")]
    Api { status: u16, message: String },
    #[error("decode: {0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Clone)]
pub struct NodeClient {
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
}

impl NodeClient {
    /// `base` is the node's HTTP origin, e.g. `http://127.0.0.1:7788`.
    pub fn new(base: &str, token: Option<String>) -> Result<Self> {
        let mut base = Url::parse(base).map_err(|e| ClientError::Url(e.to_string()))?;
        if base.cannot_be_a_base() || !matches!(base.scheme(), "http" | "https") {
            return Err(ClientError::Url(format!("{base} is not an http(s) origin")));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            base,
            token: token.filter(|t| !t.is_empty()),
            http: reqwest::Client::new(),
        })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    fn url(&self, path: &str) -> Url {
        self.base
            .join(&format!("v1/{}", path.trim_start_matches('/')))
            .expect("relative api path")
    }

    /// The WebSocket URL for `/v1/events`, token included as a query when set.
    pub fn events_url(&self) -> Url {
        let mut u = self.url("events");
        let scheme = if u.scheme() == "https" { "wss" } else { "ws" };
        let _ = u.set_scheme(scheme);
        if let Some(t) = &self.token {
            u.query_pairs_mut().append_pair("token", t);
        }
        u
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    async fn finish<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            let message = serde_json::from_slice::<ErrorBody>(&bytes)
                .map(|e| e.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
            return Err(ClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
        if bytes.is_empty() {
            // 204: callers ask for `()`.
            return serde_json::from_slice(b"null").map_err(|e| ClientError::Decode(e.to_string()));
        }
        serde_json::from_slice(&bytes).map_err(|e| ClientError::Decode(e.to_string()))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let mut url = self.url(path);
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        let resp = self.authed(self.http.get(url)).send().await?;
        Self::finish(resp).await
    }

    async fn send<T: DeserializeOwned, B: Serialize>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        let mut rb = self.http.request(method, self.url(path));
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = self.authed(rb).send().await?;
        Self::finish(resp).await
    }

    // ---- node ----

    pub async fn info(&self) -> Result<Info> {
        self.get("info", &[]).await
    }

    pub async fn outputs(&self) -> Result<Vec<OutputInfo>> {
        self.get("outputs", &[]).await
    }

    /// Discover outputs again (endpoints started after the node); new ones get zones.
    pub async fn rescan_outputs(&self) -> Result<Vec<OutputInfo>> {
        self.send::<Vec<OutputInfo>, ()>(reqwest::Method::POST, "outputs/rescan", None)
            .await
    }

    // ---- zones ----

    pub async fn zones(&self) -> Result<Vec<ZoneState>> {
        self.get("zones", &[]).await
    }

    pub async fn zone(&self, id: &str) -> Result<ZoneState> {
        self.get(&format!("zones/{id}"), &[]).await
    }

    pub async fn create_zone(&self, name: &str, output: &str) -> Result<ZoneState> {
        self.send(
            reqwest::Method::POST,
            "zones",
            Some(&CreateZone {
                name: name.into(),
                output: Some(output.into()),
                outputs: vec![],
            }),
        )
        .await
    }

    /// A zone that plays one stream to several outputs (the first leads).
    pub async fn create_group_zone(&self, name: &str, outputs: Vec<String>) -> Result<ZoneState> {
        self.send(
            reqwest::Method::POST,
            "zones",
            Some(&CreateZone {
                name: name.into(),
                output: None,
                outputs,
            }),
        )
        .await
    }

    pub async fn delete_zone(&self, id: &str) -> Result<()> {
        self.send::<(), ()>(reqwest::Method::DELETE, &format!("zones/{id}"), None)
            .await
    }

    pub async fn queue(&self, zone: &str) -> Result<Vec<QueueItem>> {
        self.get(&format!("zones/{zone}/queue"), &[]).await
    }

    pub async fn enqueue(&self, zone: &str, req: &Enqueue) -> Result<Enqueued> {
        self.send(reqwest::Method::POST, &format!("zones/{zone}/queue"), Some(req))
            .await
    }

    /// Replace the queue with these library tracks and start playing.
    pub async fn play_tracks(&self, zone: &str, track_ids: Vec<i64>) -> Result<Enqueued> {
        self.enqueue(
            zone,
            &Enqueue {
                track_ids,
                replace: true,
                play: true,
                ..Default::default()
            },
        )
        .await
    }

    pub async fn clear_queue(&self, zone: &str) -> Result<()> {
        self.send::<(), ()>(reqwest::Method::DELETE, &format!("zones/{zone}/queue"), None)
            .await
    }

    pub async fn remove_item(&self, zone: &str, item: u64) -> Result<()> {
        self.send::<(), ()>(
            reqwest::Method::DELETE,
            &format!("zones/{zone}/queue/{item}"),
            None,
        )
        .await
    }

    pub async fn move_item(&self, zone: &str, item: u64, to_index: usize) -> Result<()> {
        self.send(
            reqwest::Method::PUT,
            &format!("zones/{zone}/queue/{item}"),
            Some(&MoveItem { to_index }),
        )
        .await
    }

    pub async fn transport(&self, zone: &str, action: TransportAction) -> Result<ZoneState> {
        self.send(
            reqwest::Method::POST,
            &format!("zones/{zone}/transport"),
            Some(&action),
        )
        .await
    }

    pub async fn set_volume(&self, zone: &str, volume: Volume) -> Result<()> {
        self.send(
            reqwest::Method::POST,
            &format!("zones/{zone}/volume"),
            Some(&SetVolume { volume }),
        )
        .await
    }

    pub async fn dsp(&self, zone: &str) -> Result<pm_audio::PipelineConfig> {
        self.get(&format!("zones/{zone}/dsp"), &[]).await
    }

    pub async fn set_dsp(
        &self,
        zone: &str,
        cfg: &pm_audio::PipelineConfig,
    ) -> Result<pm_audio::PipelineConfig> {
        self.send(reqwest::Method::PUT, &format!("zones/{zone}/dsp"), Some(cfg))
            .await
    }

    pub async fn reset_dsp(&self, zone: &str) -> Result<()> {
        self.send::<(), ()>(reqwest::Method::DELETE, &format!("zones/{zone}/dsp"), None)
            .await
    }

    pub async fn set_repeat(&self, zone: &str, repeat: Repeat) -> Result<()> {
        self.send(
            reqwest::Method::PUT,
            &format!("zones/{zone}/repeat"),
            Some(&SetRepeat { repeat }),
        )
        .await
    }

    // ---- library ----

    pub async fn library_stats(&self) -> Result<pm_library_stats::LibraryStats> {
        self.get("library/stats", &[]).await
    }

    pub async fn tracks(&self, after: Option<i64>, limit: u32) -> Result<Vec<Track>> {
        let mut q = vec![("limit", limit.to_string())];
        if let Some(a) = after {
            q.push(("after", a.to_string()));
        }
        Ok(self.get::<Tracks>("library/tracks", &q).await?.tracks)
    }

    pub async fn track(&self, id: i64) -> Result<Track> {
        self.get(&format!("library/tracks/{id}"), &[]).await
    }

    /// Newest plays first, across all zones.
    pub async fn history(&self, limit: u32) -> Result<Vec<pm_library::PlayEntry>> {
        Ok(self
            .get::<History>("library/history", &[("limit", limit.to_string())])
            .await?
            .plays)
    }

    /// Report a play made by this client itself (a local zone).
    pub async fn record_play(&self, track_id: i64, zone: Option<&str>) -> Result<()> {
        let req = RecordPlay {
            track_id,
            zone: zone.map(str::to_string),
        };
        self.send::<History, _>(reqwest::Method::POST, "library/history", Some(&req))
            .await?;
        Ok(())
    }

    /// The Home screen: recently added and played albums, most played, history.
    pub async fn home(&self, limit: u32) -> Result<HomePage> {
        self.get("library/home", &[("limit", limit.to_string())]).await
    }

    /// Matching tracks only (see [`Self::search_all`] for albums, artists and genres too).
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<Track>> {
        Ok(self.search_all(q, limit).await?.tracks)
    }

    pub async fn search_all(&self, q: &str, limit: u32) -> Result<SearchResults> {
        self.get(
            "library/search",
            &[("q", q.to_string()), ("limit", limit.to_string())],
        )
        .await
    }

    pub async fn genres(&self) -> Result<Vec<pm_library::GenreSummary>> {
        self.get("library/genres", &[]).await
    }

    pub async fn genre(&self, id: i64) -> Result<GenreDetail> {
        self.get(&format!("library/genres/{id}"), &[]).await
    }

    pub async fn artists(&self) -> Result<Vec<pm_library::ArtistSummary>> {
        self.get("library/artists", &[]).await
    }

    pub async fn artist(&self, id: i64) -> Result<ArtistDetail> {
        self.get(&format!("library/artists/{id}"), &[]).await
    }

    pub async fn artist_tracks(&self, id: i64) -> Result<Vec<Track>> {
        Ok(self
            .get::<Tracks>(&format!("library/artists/{id}/tracks"), &[])
            .await?
            .tracks)
    }

    pub async fn albums(&self, after: Option<i64>, limit: u32) -> Result<Vec<Album>> {
        let mut q = vec![("limit", limit.to_string())];
        if let Some(a) = after {
            q.push(("after", a.to_string()));
        }
        self.get("library/albums", &q).await
    }

    pub async fn album_tracks(&self, album_id: i64) -> Result<Vec<Track>> {
        Ok(self
            .get::<Tracks>(&format!("library/albums/{album_id}/tracks"), &[])
            .await?
            .tracks)
    }

    /// Download a track's audio file as stored on the node (for local playback).
    pub async fn download_track(&self, id: i64, to: &std::path::Path) -> Result<u64> {
        let resp = self
            .authed(self.http.get(self.url(&format!("library/tracks/{id}/file"))))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(ClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
        let bytes = resp.bytes().await?;
        let tmp = to.with_extension("part");
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(|e| ClientError::Decode(format!("write {}: {e}", tmp.display())))?;
        tokio::fs::rename(&tmp, to)
            .await
            .map_err(|e| ClientError::Decode(format!("rename {}: {e}", to.display())))?;
        Ok(bytes.len() as u64)
    }

    /// The album's cover bytes and MIME type; `None` when the album has none.
    pub async fn album_cover(&self, album_id: i64) -> Result<Option<(String, Vec<u8>)>> {
        let resp = self
            .authed(
                self.http
                    .get(self.url(&format!("library/albums/{album_id}/cover"))),
            )
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(ClientError::Api {
                status: status.as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        Ok(Some((mime, resp.bytes().await?.to_vec())))
    }

    pub async fn folders(&self) -> Result<Vec<(String, bool)>> {
        self.get("library/folders", &[]).await
    }

    /// Add a library folder on the node; it is watched and scanned.
    pub async fn add_folder(&self, path: &str) -> Result<FoldersChanged> {
        self.send(
            reqwest::Method::POST,
            "library/folders",
            Some(&FolderRequest {
                path: path.to_string(),
            }),
        )
        .await
    }

    /// Forget a library folder and everything scanned from it.
    pub async fn remove_folder(&self, path: &str) -> Result<FoldersChanged> {
        let mut url = self.url("library/folders");
        url.query_pairs_mut().append_pair("path", path);
        let resp = self.authed(self.http.delete(url)).send().await?;
        Self::finish(resp).await
    }

    pub async fn scan(&self, folders: Vec<String>) -> Result<ScanStarted> {
        self.send(
            reqwest::Method::POST,
            "library/scan",
            Some(&ScanRequest { folders }),
        )
        .await
    }

    // ---- playlists ----

    pub async fn playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.get("playlists", &[]).await
    }

    pub async fn playlist(&self, id: &str) -> Result<PlaylistDetail> {
        self.get(&format!("playlists/{id}"), &[]).await
    }

    pub async fn create_playlist(&self, req: &CreatePlaylist) -> Result<PlaylistDetail> {
        self.send(reqwest::Method::POST, "playlists", Some(req)).await
    }

    pub async fn update_playlist(&self, id: &str, req: &UpdatePlaylist) -> Result<PlaylistDetail> {
        self.send(reqwest::Method::PUT, &format!("playlists/{id}"), Some(req))
            .await
    }

    pub async fn add_playlist_items(&self, id: &str, req: &AddPlaylistItems) -> Result<PlaylistDetail> {
        self.send(reqwest::Method::POST, &format!("playlists/{id}/items"), Some(req))
            .await
    }

    pub async fn remove_playlist_item(&self, id: &str, index: usize) -> Result<PlaylistDetail> {
        self.send::<PlaylistDetail, ()>(
            reqwest::Method::DELETE,
            &format!("playlists/{id}/items/{index}"),
            None,
        )
        .await
    }

    // ---- internet radio ----

    /// Search the radio-browser directory by name, tag or country.
    pub async fn radio_search(&self, q: &RadioSearch) -> Result<Vec<Station>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(v) = &q.q {
            params.push(("q", v.clone()));
        }
        if let Some(v) = &q.tag {
            params.push(("tag", v.clone()));
        }
        if let Some(v) = &q.country {
            params.push(("country", v.clone()));
        }
        if let Some(v) = q.limit {
            params.push(("limit", v.to_string()));
        }
        Ok(self.get::<Stations>("radio/search", &params).await?.stations)
    }

    pub async fn radio_stations(&self) -> Result<Vec<Station>> {
        Ok(self.get::<Stations>("radio/stations", &[]).await?.stations)
    }

    pub async fn save_station(&self, st: &Station) -> Result<Vec<Station>> {
        Ok(self
            .send::<Stations, _>(reqwest::Method::PUT, "radio/stations", Some(st))
            .await?
            .stations)
    }

    pub async fn delete_station(&self, id: &str) -> Result<Vec<Station>> {
        Ok(self
            .send::<Stations, ()>(reqwest::Method::DELETE, &format!("radio/stations/{id}"), None)
            .await?
            .stations)
    }

    /// Replace the zone's queue with the station and start it.
    pub async fn play_station(&self, zone: &str, st: &Station) -> Result<Enqueued> {
        let req = PlayStation {
            zone: zone.to_string(),
            station: st.clone(),
        };
        self.send(reqwest::Method::POST, "radio/play", Some(&req)).await
    }

    // ---- tags ----

    /// My tags on one object (`Some(r)`) or all of them (`None`).
    pub async fn tags(&self, r: Option<&FavouriteRef>) -> Result<Tags> {
        let mut q: Vec<(&str, String)> = Vec::new();
        if let Some(r) = r {
            if let Some(u) = &r.uri {
                q.push(("uri", u.clone()));
            }
            if let Some(i) = r.track_id {
                q.push(("track_id", i.to_string()));
            }
            if let Some(i) = r.album_id {
                q.push(("album_id", i.to_string()));
            }
            if let Some(i) = r.artist_id {
                q.push(("artist_id", i.to_string()));
            }
        }
        self.get("tags", &q).await
    }

    /// Add or remove one tag label on an object.
    pub async fn set_tag(&self, r: &FavouriteRef, label: &str, on: bool) -> Result<TagChanged> {
        let req = TagRef {
            uri: r.uri.clone(),
            track_id: r.track_id,
            album_id: r.album_id,
            artist_id: r.artist_id,
            label: Some(label.to_string()),
        };
        if on {
            self.send(reqwest::Method::PUT, "tags", Some(&req)).await
        } else {
            let mut url = self.url("tags");
            {
                let mut q = url.query_pairs_mut();
                if let Some(u) = &req.uri {
                    q.append_pair("uri", u);
                }
                if let Some(i) = req.track_id {
                    q.append_pair("track_id", &i.to_string());
                }
                if let Some(i) = req.album_id {
                    q.append_pair("album_id", &i.to_string());
                }
                if let Some(i) = req.artist_id {
                    q.append_pair("artist_id", &i.to_string());
                }
                q.append_pair("label", label);
            }
            let resp = self.authed(self.http.delete(url)).send().await?;
            Self::finish(resp).await
        }
    }

    // ---- favourites ----

    pub async fn favourites(&self) -> Result<Favourites> {
        self.get("favourites", &[]).await
    }

    /// Heart or un-heart a track, album, artist or canonical URL.
    pub async fn set_favourite(&self, r: &FavouriteRef, on: bool) -> Result<FavouriteChanged> {
        if on {
            self.send(reqwest::Method::PUT, "favourites", Some(r)).await
        } else {
            let mut url = self.url("favourites");
            {
                let mut q = url.query_pairs_mut();
                if let Some(u) = &r.uri {
                    q.append_pair("uri", u);
                }
                if let Some(i) = r.track_id {
                    q.append_pair("track_id", &i.to_string());
                }
                if let Some(i) = r.album_id {
                    q.append_pair("album_id", &i.to_string());
                }
                if let Some(i) = r.artist_id {
                    q.append_pair("artist_id", &i.to_string());
                }
            }
            let resp = self.authed(self.http.delete(url)).send().await?;
            Self::finish(resp).await
        }
    }

    /// Import described tracks as a new private playlist.
    pub async fn import_playlist(&self, req: &ImportPlaylist) -> Result<ImportedPlaylist> {
        self.send(reqwest::Method::POST, "playlists/import", Some(req))
            .await
    }

    /// Import a playlist file (M3U/M3U8, CSV, Roon Excel export) by its bytes.
    pub async fn import_playlist_file(
        &self,
        file_name: &str,
        name: Option<&str>,
        bytes: Vec<u8>,
    ) -> Result<ImportedPlaylist> {
        let mut url = self.url("playlists/import/file");
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("file", file_name);
            if let Some(n) = name {
                q.append_pair("name", n);
            }
        }
        let resp = self
            .authed(self.http.post(url))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await?;
        Self::finish(resp).await
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<()> {
        self.send::<(), ()>(reqwest::Method::DELETE, &format!("playlists/{id}"), None)
            .await
    }

    pub async fn play_playlist(&self, id: &str, zone: &str, replace: bool) -> Result<PlayedPlaylist> {
        self.send(
            reqwest::Method::POST,
            &format!("playlists/{id}/play"),
            Some(&PlayPlaylist {
                zone: zone.into(),
                replace: Some(replace),
            }),
        )
        .await
    }

    pub async fn discover_trends(&self, window: &str, limit: usize) -> Result<DiscoverPage> {
        self.get(
            "discover/trends",
            &[("window", window.to_string()), ("limit", limit.to_string())],
        )
        .await
    }

    pub async fn discover_tag(&self, label: &str, limit: usize) -> Result<DiscoverPage> {
        self.get(&format!("discover/tag/{label}"), &[("limit", limit.to_string())])
            .await
    }

    pub async fn discover_friends(&self, viewer_z32: &str, limit: usize) -> Result<DiscoverPage> {
        self.get(
            &format!("discover/friends/{viewer_z32}"),
            &[("limit", limit.to_string())],
        )
        .await
    }

    pub async fn pubky_status(&self) -> Result<PubkyStatus> {
        self.get("pubky", &[]).await
    }

    pub async fn pubky_files(&self) -> Result<Vec<pm_pubky::homeserver::RemoteFile>> {
        self.get("pubky/files", &[]).await
    }

    pub async fn pubky_upload(&self, req: &UploadFile) -> Result<UploadedFile> {
        self.send(reqwest::Method::POST, "pubky/files", Some(req)).await
    }

    pub async fn pubky_mirror(&self) -> Result<MirrorReport> {
        self.send::<MirrorReport, ()>(reqwest::Method::POST, "pubky/files/mirror", None)
            .await
    }

    /// Start a Pubky Ring sign-in on the node; returns the pubkyauth URL.
    pub async fn pubky_login_start(&self) -> Result<LoginStarted> {
        self.send::<LoginStarted, ()>(reqwest::Method::POST, "pubky/login", None)
            .await
    }

    pub async fn pubky_login_poll(&self) -> Result<LoginStatus> {
        self.get("pubky/login", &[]).await
    }

    pub async fn pubky_sync(&self) -> Result<SyncReport> {
        self.send::<SyncReport, ()>(reqwest::Method::POST, "pubky/sync", None)
            .await
    }

    // ---- events ----

    /// Open `/v1/events`. The first item is the snapshot.
    pub async fn events(&self) -> Result<EventStream> {
        let mut req = self.events_url().as_str().into_client_request()?;
        if let Some(t) = &self.token {
            req.headers_mut().insert(
                header::AUTHORIZATION,
                format!("Bearer {t}").parse().expect("header"),
            );
        }
        let (ws, _) = tokio_tungstenite::connect_async(req).await?;
        Ok(EventStream { ws })
    }
}

/// Find the user's node through their homeserver (ADR 0008): restore the
/// session from an exported secret, read the private node record, and
/// return a client for it.
pub async fn connect_via_pubky(
    network: pm_pubky::Network,
    session_secret: &str,
) -> Result<(NodeClient, pm_pubky::homeserver::NodeRecord)> {
    let hs = pm_pubky::homeserver::NodeHomeserver::new(network)
        .map_err(|e| ClientError::Url(format!("pubky client: {e}")))?;
    let session = hs
        .restore_session(session_secret.trim())
        .await
        .map_err(|e| ClientError::Api {
            status: 401,
            message: format!("pubky session: {e}"),
        })?;
    let record = hs
        .read_node_record(&session)
        .await
        .map_err(|e| ClientError::Api {
            status: 502,
            message: format!("homeserver: {e}"),
        })?
        .ok_or_else(|| ClientError::Api {
            status: 404,
            message:
                "no node has published its address for this pubky yet (start `pm-node serve` with a session)"
                    .into(),
        })?;
    let client = NodeClient::new(
        &record.api_url,
        (!record.token.is_empty()).then(|| record.token.clone()),
    )?;
    Ok((client, record))
}

/// Re-export so callers need not depend on pm-library for one struct.
pub mod pm_library_stats {
    pub use pm_library::LibraryStats;
}

pub struct EventStream {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

impl EventStream {
    /// Next event; `None` when the node closed the socket.
    pub async fn next(&mut self) -> Option<Result<ApiEvent>> {
        loop {
            match self.ws.next().await? {
                Ok(Message::Text(t)) => {
                    return Some(serde_json::from_str(&t).map_err(|e| ClientError::Decode(e.to_string())));
                }
                Ok(Message::Close(_)) => return None,
                Ok(_) => continue,
                Err(e) => return Some(Err(e.into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let c = NodeClient::new("http://node.local:7788", Some("abc".into())).unwrap();
        assert_eq!(
            c.url("zones/x/queue").as_str(),
            "http://node.local:7788/v1/zones/x/queue"
        );
        assert_eq!(
            c.events_url().as_str(),
            "ws://node.local:7788/v1/events?token=abc"
        );
        let c = NodeClient::new("https://node.example/base", None).unwrap();
        assert_eq!(c.url("info").as_str(), "https://node.example/base/v1/info");
        assert_eq!(c.events_url().as_str(), "wss://node.example/base/v1/events");
        assert!(NodeClient::new("ftp://x", None).is_err());
        assert!(NodeClient::new("not a url", None).is_err());
    }
}
