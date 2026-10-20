//! Request and response bodies of `/v1`. Plain serde types so a client crate
//! can reuse them without pulling in the server.

use pm_model::{Track, Volume};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateZone {
    pub name: String,
    /// Output id from `GET /v1/outputs`.
    pub output: String,
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
