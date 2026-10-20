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
    /// A library scan is running right now.
    #[serde(default)]
    pub scanning: bool,
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

/// Move playback from one zone to another (Roon's "Transfer"): the queue,
/// current item and position, repeat and radio go with it; the source stops
/// and empties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRequest {
    /// Target zone id.
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRadio {
    pub on: bool,
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

/// Everything a query matched: tracks by full text, albums by title,
/// artists and genres by name.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub albums: Vec<Album>,
    #[serde(default)]
    pub artists: Vec<ArtistSummary>,
    #[serde(default)]
    pub genres: Vec<pm_library::GenreSummary>,
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

/// A review target (one of the id fields) with the rating and/or text to store.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ReviewRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewEntry {
    /// The post id (timestamp id).
    pub id: String,
    pub uri: String,
    pub rating: Option<u8>,
    pub text: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub synced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Reviews {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    pub reviews: Vec<ReviewEntry>,
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

/// Runtime-changeable node settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSettingsView {
    /// Contact (email or URL) sent in outbound User-Agent strings; required by Pinakes, MusicBrainz and friends.
    #[serde(default)]
    pub contact: String,
    /// Pinakes/Nexus discovery is available.
    #[serde(default)]
    pub discovery: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSettingsUpdate {
    #[serde(default)]
    pub contact: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldersChanged {
    /// `[path, present]` for every library folder.
    pub folders: Vec<(String, bool)>,
    /// A scan of the new folder started (false when one was already running).
    #[serde(default)]
    pub scan_started: bool,
    #[serde(default)]
    pub removed_files: u64,
    #[serde(default)]
    pub removed_tracks: u64,
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

/// Save a play queue as a playlist: either a zone's queue or an explicit
/// list of queue URIs (`track:<id>`, local paths, canonical music URLs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlaylistFromQueue {
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default)]
    pub uris: Vec<String>,
    /// Defaults to "Queue · <zone>" or "Queue".
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistFromQueueResult {
    pub playlist: PlaylistDetail,
    /// Labels of queue entries without a canonical identity (untagged files,
    /// radio streams); they are not in the playlist.
    pub skipped: Vec<String>,
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

/// What open sources know about an artist (see `artist_info`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ArtistInfo {
    /// Opening paragraph of the English Wikipedia article.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    /// A portrait is cached; fetch it from `/v1/library/artists/{id}/image`.
    #[serde(default)]
    pub image: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wikipedia: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wikidata: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub musicbrainz: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Why there is nothing (no id, lookup running, nothing found).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenreDetail {
    pub genre: pm_library::GenreSummary,
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
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

// ---- streaming sources ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `ready`, or a reason it is not.
    pub status: String,
}

/// One search result of a streaming source: an album-like item or a single track.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceHit {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `item` (several tracks) or `track`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// The source's own page for it.
    pub url: String,
}

fn default_kind() -> String {
    "item".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourceResults {
    pub source: String,
    pub items: Vec<SourceHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceTrack {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_no: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The archive's format label (`Flac`, `VBR MP3`, …).
    pub format: String,
    /// Direct stream URL.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceItem {
    pub source: String,
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub url: String,
    pub tracks: Vec<SourceTrack>,
}

/// What Pubky (via Nexus) knows about one canonical URL: who tagged it and how.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OnPubky {
    pub uri: String,
    /// The indexer knows this URL.
    pub indexed: bool,
    /// Labels with how many people used them, most used first.
    pub tags: Vec<(String, u64)>,
    /// Distinct people across all labels (from the paged tagger lists).
    pub taggers: u64,
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
