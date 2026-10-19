//! Request and response bodies of `/v1`. Plain serde types so a client crate
//! can reuse them without pulling in the server.

use pm_model::{Track, Volume};
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
