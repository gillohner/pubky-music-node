//! Request and response bodies of `/v1`. Plain serde types so a client crate
//! can reuse them without pulling in the server.

use pm_library::ArtistSummary;
use pm_model::{Album, Track, Volume};
use pm_pubky::{ActivitySource, Playlist};
use pm_zones::{QueueItem, Repeat, ZoneEvent, ZoneId, ZoneState};
use serde::{Deserialize, Serialize};

pub const API_VERSION: &str = "v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub name: String,
    pub version: String,
    pub api: String,
    pub library: pm_library::LibraryStats,
    pub zones: usize,
    pub outputs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateZone {
    pub name: String,
    /// Output id from `GET /v1/outputs`.
    #[serde(default)]
    pub output: Option<String>,
    /// Two or more output ids to play as one group (the first is the leader).
    #[serde(default)]
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QueueAt {
    #[default]
    End,
    Next,
}

/// Items to enqueue: library tracks by id, or raw URIs (local paths,
/// `file://`, later stream URLs). Either list may be empty.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Enqueue {
    #[serde(default)]
    pub track_ids: Vec<i64>,
    #[serde(default)]
    pub items: Vec<QueueItem>,
    #[serde(default)]
    pub at: QueueAt,
    /// Clear the queue first.
    #[serde(default)]
    pub replace: bool,
    /// Start playing (from the first enqueued item when `replace`).
    #[serde(default)]
    pub play: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enqueued {
    pub ids: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TransportAction {
    Play {
        /// Queue index to start from; omitted = resume or current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek {
        position_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetVolume {
    pub volume: Volume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRepeat {
    pub repeat: Repeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveItem {
    pub to_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Page {
    pub after: Option<i64>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tracks {
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limit {
    pub limit: Option<u32>,
}

/// A play reported by a client that plays by itself (the native app's local zone).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordPlay {
    pub track_id: i64,
    #[serde(default)]
    pub zone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    pub plays: Vec<pm_library::PlayEntry>,
}

/// A favourite by canonical URL or by library id (one of them).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct FavouriteRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<i64>,
}

impl FavouriteRef {
    pub fn uri(uri: impl Into<String>) -> Self {
        Self {
            uri: Some(uri.into()),
            ..Default::default()
        }
    }
    pub fn track(id: i64) -> Self {
        Self {
            track_id: Some(id),
            ..Default::default()
        }
    }
    pub fn album(id: i64) -> Self {
        Self {
            album_id: Some(id),
            ..Default::default()
        }
    }
    pub fn artist(id: i64) -> Self {
        Self {
            artist_id: Some(id),
            ..Default::default()
        }
    }
}

/// Favourites joined to the library; `other` are canonical URLs the library
/// has no object for (favourited elsewhere, or not scanned here).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Favourites {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<ArtistSummary>,
    #[serde(default)]
    pub other: Vec<String>,
    /// Every favourited canonical URL, for quick "is this a favourite" checks.
    #[serde(default)]
    pub uris: Vec<String>,
}

/// A tag on a canonical URL or a library object (one target field), with its label.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TagRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TagEntry {
    pub uri: String,
    pub label: String,
    /// Microseconds.
    pub created_at: i64,
    /// Pushed to the homeserver.
    pub synced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Tags {
    /// The canonical URL the list is for; `None` for "all of mine".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    pub tags: Vec<TagEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagChanged {
    pub uri: String,
    pub label: String,
    pub tagged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FavouriteChanged {
    pub uri: String,
    pub favourite: bool,
}

/// What the Home screen shows: the Roon-style landing page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomePage {
    #[serde(default)]
    pub favourite_albums: Vec<Album>,
    pub recently_added: Vec<Album>,
    pub recently_played: Vec<Album>,
    pub most_played_albums: Vec<Album>,
    pub most_played: Vec<pm_library::PlayedTrack>,
    pub history: Vec<pm_library::PlayEntry>,
    pub stats: pm_library::LibraryStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRequest {
    /// Folders to scan; the configured ones when empty.
    #[serde(default)]
    pub folders: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanStarted {
    pub folders: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// What the WebSocket at `/v1/events` sends. The first message is a
/// `snapshot`; after that every zone state change and zone event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiEvent {
    Snapshot { zones: Vec<ZoneState> },
    Zone { state: ZoneState },
    ZoneEvent { zone: ZoneId, event: ZoneEvent },
    ZoneRemoved { zone: ZoneId },
    LibraryChanged,
}

// ---- playlists (ADR 0006) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub items: usize,
    pub created_at: i64,
    pub updated_at: i64,
    /// `private` or `public`.
    pub visibility: String,
    /// Pushed to the homeserver since the last edit.
    pub synced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistDetail {
    pub summary: PlaylistSummary,
    pub playlist: Playlist,
    /// One entry per item: the library track that matches its canonical URL, if any.
    pub tracks: Vec<Option<Track>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreatePlaylist {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// Canonical music URLs.
    #[serde(default)]
    pub uris: Vec<String>,
    /// Library tracks; each must have a canonical identity.
    #[serde(default)]
    pub track_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdatePlaylist {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// Replace the item list (order matters).
    #[serde(default)]
    pub uris: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AddPlaylistItems {
    #[serde(default)]
    pub uris: Vec<String>,
    #[serde(default)]
    pub track_ids: Vec<i64>,
    /// Insert position; end when omitted.
    #[serde(default)]
    pub at: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayPlaylist {
    pub zone: String,
    /// Replace the zone's queue and start (default true); false appends.
    #[serde(default)]
    pub replace: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayedPlaylist {
    pub queued: usize,
    /// Items with no matching library track.
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncReport {
    pub pushed: usize,
    pub pulled: usize,
    pub deleted: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubkyStatus {
    /// Signed-in user's raw z32 key, when the node has a session.
    pub user: Option<String>,
    pub unsynced: usize,
}

// ---- activity and import (integrations) ----

/// A track as an integration knows it; `uri` is a canonical music URL when available.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DescribedTrack {
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    /// A file path as another player knew it; matched exactly, then by file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl From<pm_library::ImportedTrack> for DescribedTrack {
    fn from(t: pm_library::ImportedTrack) -> Self {
        Self {
            title: t.title,
            artist: t.artist,
            album: t.album,
            uri: None,
            path: t.path,
        }
    }
}

/// Query for `POST /v1/playlists/import/file`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportFileQuery {
    /// Playlist name; defaults to the file's own name or its stem.
    #[serde(default)]
    pub name: Option<String>,
    /// The uploaded file's name, used to pick the format and the default name.
    #[serde(default)]
    pub file: Option<String>,
    /// `m3u`, `csv` or `xlsx` when the name does not say.
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordActivity {
    pub track: DescribedTrack,
    /// `local`, `roon`, or a streaming service name.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedActivity {
    pub id: String,
    pub uri: String,
    pub track_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: String,
    pub uri: String,
    pub played_at: i64,
    pub source: ActivitySource,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    pub track: Option<Track>,
    pub synced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportPlaylist {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub tracks: Vec<DescribedTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedPlaylist {
    pub id: String,
    pub name: String,
    /// Entries found in the file (or sent).
    pub parsed: usize,
    pub imported: usize,
    pub unresolved: Vec<DescribedTrack>,
}

// ---- private files on the homeserver ----

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UploadFile {
    /// A library track to upload.
    #[serde(default)]
    pub track_id: Option<i64>,
    /// Or a path on the node.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadedFile {
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MirrorReport {
    pub fetched: usize,
    pub present: usize,
    pub bytes: u64,
    #[serde(default)]
    pub errors: Vec<String>,
}

// ---- artists ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistDetail {
    pub artist: ArtistSummary,
    pub albums: Vec<Album>,
}

// ---- discovery ----

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoverQuery {
    pub limit: Option<usize>,
    /// Trending window, `1h`..`90d`.
    pub window: Option<String>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverItem {
    /// Canonical music URL.
    pub uri: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub taggers: Option<u32>,
    /// The matching library track, when the user owns it.
    pub in_library: Option<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoverPage {
    /// Tag facets `(label, count)` when the upstream returned them.
    pub facets: Vec<(String, i64)>,
    pub items: Vec<DiscoverItem>,
}

// ---- pull outputs ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePull {
    /// URL segment: `GET /v1/outputs/pull/{name}/stream.wav`.
    pub name: String,
}

// ---- Pubky Ring sign-in ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStarted {
    /// `pubkyauth://…` to scan or paste into Pubky Ring.
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStatus {
    /// `none`, `pending` or `signed_in`.
    pub status: String,
    pub user: Option<String>,
    pub url: Option<String>,
}
