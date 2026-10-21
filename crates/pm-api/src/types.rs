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
    /// Stable node id (also announced over mDNS and SSDP).
    #[serde(default)]
    pub id: String,
    /// The node's iroh key when remote access is on (ADR 0013).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
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

/// Sleep timer: pause the zone after `seconds`; `null` cancels.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SetSleep {
    #[serde(default)]
    pub seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SleepView {
    /// Unix milliseconds when the zone pauses; absent when no timer is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_ms: Option<u64>,
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
    /// Albums only: keep albums with a file of this quality (`hi_res`,
    /// `dsd`, `cd`, `lossless`, `lossy`).
    #[serde(default)]
    pub focus: Option<pm_library::Focus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    /// `sources=0` keeps the search to the library.
    #[serde(default)]
    pub sources: Option<u8>,
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
    /// Hits from streaming services that take part in search (TIDAL when configured).
    #[serde(default)]
    pub sources: Vec<SourceResults>,
}

/// An artist on a streaming service with their albums.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceArtist {
    pub source: String,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub url: String,
    pub albums: Vec<SourceHit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_artist_id: Option<i64>,
}

/// Where else a library album or track can be streamed from (Roon's "Versions").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub source: String,
    /// Source item id (`album:<id>` / `track:<id>`), for `/v1/sources/{source}/items/{id}`.
    pub id: String,
    pub label: String,
    pub url: String,
    /// Queue URI when the version is a single track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
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

/// `POST /v1/tags/bulk`: one label on every track in scope.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BulkTag {
    pub label: String,
    /// Scope: one album, one artist, or the whole library when both are absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BulkTagged {
    pub label: String,
    /// Tracks that now carry the tag (new ones; already-tagged ones count under `already`).
    pub tagged: u64,
    pub already: u64,
    /// Tracks without a publishable identity (no MusicBrainz id or ISRC); Pinakes cannot match them.
    pub skipped_no_identity: u64,
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
    /// The zone Subsonic clients drive through `jukeboxControl` (none = first by name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jukebox_zone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSettingsUpdate {
    #[serde(default)]
    pub contact: Option<String>,
    /// Zone id for Subsonic jukebox control; an empty string clears the choice.
    #[serde(default)]
    pub jukebox_zone: Option<String>,
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

/// What a network scan found, to explain why a device is (not) an output.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoveryReport {
    /// Every UPnP device that answered SSDP, renderer or not.
    pub upnp: Vec<DiscoveredUpnp>,
    /// Chromecast / Google Cast devices seen over mDNS.
    pub cast: Vec<DiscoveredCast>,
    /// The address devices would fetch audio from (`public_url`).
    pub public_url: String,
    /// The node's own route-facing address, as a hint for `public_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_address: Option<String>,
    /// Plain-language findings (no answers at all, public URL unusable, …).
    pub notes: Vec<String>,
    pub upnp_enabled: bool,
    pub cast_enabled: bool,
    /// Renderer addresses added by hand (`POST /v1/outputs/upnp`).
    #[serde(default)]
    pub saved_addresses: Vec<String>,
    /// Endpoint addresses added by hand (`POST /v1/outputs/endpoints`).
    #[serde(default)]
    pub saved_endpoints: Vec<String>,
    /// Which node scanned (people run more than one).
    #[serde(default)]
    pub node: String,
}

/// `POST /v1/outputs/upnp`: a renderer by IP, `ip:port` or description URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRenderer {
    pub address: String,
}

/// `POST /v1/outputs/endpoints`: a `pm-endpoint` (or the desktop app's
/// player) by `host:port`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddEndpoint {
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredUpnp {
    pub from: String,
    pub location: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub friendly_name: Option<String>,
    pub renderer: bool,
    /// Already an output of this node.
    pub output: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredCast {
    pub id: String,
    pub friendly_name: String,
    pub addr: String,
}

/// A release group MusicBrainz lists for one of the library's artists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRelease {
    pub artist_id: i64,
    pub artist: String,
    pub release_group_mbid: String,
    pub title: String,
    /// `YYYY-MM-DD` (MusicBrainz may give only a year or a month).
    pub first_release_date: String,
    /// `Album`, `EP` or `Single`.
    pub kind: String,
    pub url: String,
    /// The library album with this release group, when you already have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_library: Option<Album>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NewReleasesPage {
    pub items: Vec<NewRelease>,
    /// Library artists with a MusicBrainz id (only those can be checked).
    pub artists_total: usize,
    pub artists_checked: usize,
    pub refreshing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<i64>,
}

/// A track's lyrics, from its tags or a sidecar `.lrc` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lyrics {
    pub track_id: i64,
    /// `lrc` (sidecar file) or `tag` (embedded).
    pub source: String,
    /// True when the lines carry timestamps.
    pub synced: bool,
    pub text: String,
    pub lines: Vec<pm_library::LyricLine>,
}

/// Save a play queue as a playlist: either a zone's queue or an explicit
/// list of queue URIs (`track:<id>`, local paths, canonical music URLs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlaylistFromQueue {
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default)]
    pub uris: Vec<String>,
    /// Defaults to `Queue · <zone name>` or `Queue`.
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
    /// Pins for `uris`, by position (ADR 0015); missing or `null` means Auto.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Option<pm_pubky::ItemSource>>,
}

/// `?keep_source=true` on a provider playlist import pins every song to its track on that
/// provider; otherwise the songs follow the playlist's order (Auto).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportOptions {
    #[serde(default)]
    pub keep_source: Option<bool>,
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
    /// Things worth knowing that are not errors (playlists kept in the old format).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubkyStatus {
    /// Signed-in user's raw z32 key, when the node has a session.
    pub user: Option<String>,
    pub unsynced: usize,
    /// The session must be approved again in Pubky Ring (it lacks folders the node uses, or the
    /// homeserver refused one).
    #[serde(default)]
    pub needs_resign: bool,
    /// The folders the session lacks (`/pub/social/v1/:rw`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// The sentence to show when `needs_resign`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
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
    /// Charts and search: `tracks`, `albums` or `artists`.
    #[serde(default, rename = "type")]
    pub type_: Option<String>,
    /// Search text (`/discover/search`).
    #[serde(default)]
    pub q: Option<String>,
    /// One card (`/discover/item`): its canonical or service URL.
    #[serde(default)]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    /// `track`, `album`, `artist` or `playlist` (from the card's type).
    #[serde(default)]
    pub kind: String,
    /// The service the card is from ("TIDAL", "Bandcamp", …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Pinakes resource id, for `/v1/discover/item`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinakes_id: Option<String>,
    /// How many services carry the same thing (the Pinakes group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_size: Option<i64>,
    /// Queue URI when the node can stream the card's own page (TIDAL, Deezer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playable: Option<String>,
    /// "Play on…" / "Open on…": the same thing on other services, tagged first.
    #[serde(default)]
    pub also_on: Vec<Version>,
    /// The card's own page (a service URL), when it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// Where Play will take this from, resolved through the node's connected
    /// services: the library, else the best service link. `None` with
    /// `connect` naming the services that have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plays_from: Option<PlaysFrom>,
    /// Services that have this item but are not connected to this node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<String>,
    /// Other listings of the same song merged into this one (two Pinakes groups, a remaster).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub versions: u32,
}

/// `POST /v1/discover/describe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeRequest {
    pub uris: Vec<String>,
}

/// A link from an item page to another one (artist, album).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageLink {
    pub name: String,
    /// Page id (`lib-<n>`, `mb-…`, `isrc-…`, `u-…`); absent when there is no page yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// One fact in an item page's About section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fact {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
}

/// `GET /v1/item/{kind}/{id}`: one item page (song, album, artist) from any
/// source, with the same sections everywhere (docs/ux/08-item-page.md).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ItemPage {
    /// `song`, `album` or `artist`.
    pub kind: String,
    /// This page's id (`lib-<n>`, `mb-…`, `isrc-…`, `u-…`).
    pub id: String,
    /// The canonical URL or the service link the page is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// What tags, ratings and favourites attach to (`{track_id}`, `{album_id}`,
    /// `{artist_id}` or `{uri}`), for the existing `/v1/tags`, `/v1/reviews`
    /// and `/v1/favourites` calls.
    pub target: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<PageLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<PageLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Where the item comes from: "Your library", "TIDAL", "Deezer", "Pubky".
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plays_from: Option<PlaysFrom>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<String>,
    /// Pinakes is still reading this link.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reading: bool,
    #[serde(default)]
    pub songs: Vec<ItemTrack>,
    #[serde(default)]
    pub total_ms: u64,
    #[serde(default)]
    pub albums: Vec<ItemAlbum>,
    #[serde(default)]
    pub versions: Vec<Version>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_pubky: Option<OnPubky>,
    #[serde(default)]
    pub about: Vec<Fact>,
    /// A short description (Wikipedia for library artists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blurb: Option<String>,
    /// The "Open on" links (service pages), the item's own first.
    #[serde(default)]
    pub open_on: Vec<Version>,
    /// "People who tagged this also tagged" (once Pinakes ships `related`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<DiscoverItem>,
    /// An artist's upcoming concerts (once Pinakes keys event performers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concerts: Vec<DiscoverItem>,
    /// Sidebar section the page belongs to (`albums`, `artists`, `discover`).
    pub section: String,
}

/// One song on an item page (an album's tracks, an artist's top songs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ItemTrack {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no: Option<u32>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Queue URI (`tidal:track:…`, `deezer:track:…`, `spotify:track:…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// The library track, when it plays from the library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<i64>,
    /// The service's page for this song (opens its own item page).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The song's own page id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
}

/// An album tile on an artist's item page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemAlbum {
    /// The album's page id.
    #[serde(default)]
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub page: String,
}

/// `GET /v1/discover/contents`: what is inside an item, from the service
/// it plays from (an album's songs; an artist's albums and top songs).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ItemContents {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plays_from: Option<PlaysFrom>,
    /// Services that have it but are not connected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<String>,
    #[serde(default)]
    pub tracks: Vec<ItemTrack>,
    /// Sum of the songs' durations.
    #[serde(default)]
    pub total_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub albums: Vec<ItemAlbum>,
}

/// Where an item plays from (the "Plays from" chip).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaysFrom {
    /// `library`, `tidal`, `deezer`, `spotify`.
    pub service: String,
    /// `track` or `album`.
    pub kind: String,
    /// Source item id (`track:<id>` / `album:<id>`); the library track or album id.
    pub id: String,
    /// Short text for the chip ("TIDAL · tagged on Pubky", "Your library · FLAC").
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `preview` when the service only gives excerpts, `connect` when the
    /// service plays on its own device (Spotify Connect), else `full`.
    pub mode: String,
}

/// `GET /v1/discover/play`: what to queue for an item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayPlan {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plays_from: Option<PlaysFrom>,
    /// Library track ids to queue (when `plays_from.service == "library"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub track_ids: Vec<i64>,
    /// Queue items to send to a zone (streamed by the node).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<pm_zones::QueueItem>,
    /// Spotify URIs for `POST /v1/sources/spotify/play` (mode `connect`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spotify_uris: Vec<String>,
    /// Services that have it but are not connected ("Connect TIDAL").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<String>,
    /// A 30 s preview the node can play when nothing full is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<PlaysFrom>,
}

/// `GET /v1/discover/item`: one card with everything the node knows around it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoverDetail {
    pub item: DiscoverItem,
    /// Exact identities from Pinakes (`details.facts`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    #[serde(default)]
    pub isrcs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_type: Option<String>,
    /// Who tagged it on Pubky (labels with counts, distinct taggers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_pubky: Option<OnPubky>,
    /// The library album with the same identity, when owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_library_album: Option<i64>,
    /// MusicBrainz / Wikipedia facts for artists, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_info: Option<ArtistInfo>,
    /// Pinakes is still reading this link: the page shows the provider's own
    /// data and asks again shortly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reading: bool,
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
    /// Track hits: length, so a queue item knows it before the stream does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Artist hits: the library artist with the same name, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_artist_id: Option<i64>,
    /// Identity the service exposes, for matching against the library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upc: Option<String>,
    /// The library track or album with the same identity, when you already have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_track_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_album_id: Option<i64>,
    /// Queue URI of a track hit (`tidal:track:…`, `deezer:track:…`, a stream URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
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
    /// The library album with the same UPC, when you already have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_album_id: Option<i64>,
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
    /// The same as a QR code (SVG markup), for screens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qr_svg: Option<String>,
}

/// `POST /v1/auth/pubky/start`: a controller or browser sign-in with Pubky Ring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerLoginStarted {
    pub id: String,
    /// `pubkyauth://…` to scan or open in Pubky Ring.
    pub url: String,
    /// Who must approve: the node's owner (z32 public key).
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qr_svg: Option<String>,
}

/// `GET /v1/auth/pubky/poll?id=`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerLoginStatus {
    /// `pending`, `ok`, `denied` (another identity approved), `expired`.
    pub status: String,
    /// The API token, once the owner approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStatus {
    /// `none`, `pending` or `signed_in`.
    pub status: String,
    pub user: Option<String>,
    pub url: Option<String>,
}

// ---- where playlist items play from (play_from.rs) ----

/// One source an item has on this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaySource {
    /// `library`, `tidal`, `spotify`, `deezer`, `archive`.
    pub service: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub label: String,
    /// `full`, `connect` (plays on a Spotify device), `preview` or `none` (not connected).
    pub mode: String,
    pub available: bool,
    /// The source the item plays from now.
    pub chosen: bool,
}

/// A playlist item, resolved: the chip, the pin and every source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItemPlay {
    pub index: usize,
    pub uri: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub image: Option<String>,
    pub page: Option<String>,
    pub site: Option<String>,
    pub duration_ms: Option<u64>,
    /// The library track, when the library is the chosen source.
    pub track_id: Option<i64>,
    /// The queue URI, when a streamed service is the chosen source.
    pub stream_uri: Option<String>,
    pub plays_from: Option<PlaysFrom>,
    /// The owner's pin, if any.
    pub pin: Option<pm_pubky::ItemSource>,
    /// Pinned, but that source is not usable here (the order decided instead).
    pub pin_unavailable: bool,
    pub sources: Vec<PlaySource>,
    /// Why nothing plays ("Not on your connected services").
    pub reason: Option<String>,
    /// Services that have it but are not connected.
    pub connect: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistPlayFrom {
    /// The order in use (the playlist's, else the node's).
    pub order: PlayOrder,
    /// Whether the playlist has its own order.
    pub own_order: bool,
    pub items: Vec<PlaylistItemPlay>,
}

/// A play order and what each service can do on this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayOrder {
    pub order: Vec<String>,
    pub services: Vec<PlayService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayService {
    pub id: String,
    pub name: String,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetPlayOrder {
    pub order: Vec<String>,
}

/// `PUT /v1/playlists/{id}/order`: `null` follows the node's order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetPlaylistOrder {
    #[serde(default)]
    pub order: Option<Vec<String>>,
}

/// `PUT /v1/playlists/{id}/items/{index}/source`: no `service` means Auto (follow the playlist).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PinItemSource {
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
}

/// `PUT /v1/playlists/{id}/visibility`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetVisibility {
    pub public: bool,
}

// ---- playlists other people share (shared.rs) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedLink {
    pub link: String,
}

/// Who a pasted link names, and which playlist if it names one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedTarget {
    pub user: String,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedPlaylistSummary {
    /// The author's key (z32).
    pub user: String,
    pub id: String,
    /// `pubky://<user>/pub/social/v1/posts/<id>`.
    pub link: String,
    pub name: String,
    pub description: Option<String>,
    pub items: usize,
    /// Microseconds.
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedPlaylist {
    pub summary: SharedPlaylistSummary,
    /// Where each song plays from on this node.
    pub play_from: PlaylistPlayFrom,
}

/// `GET /v1/sources/jamendo/status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JamendoStatus {
    pub configured: bool,
    pub signed_in: bool,
    pub name: Option<String>,
    /// What the Jamendo app's redirect URL must allow (same domain).
    pub redirect_uri: String,
}

// ---- your own Subsonic server as a source (opensubsonic.rs) ----

/// The connected server as the UI sees it: never the password.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubsonicServerView {
    pub connected: bool,
    pub url: Option<String>,
    pub user: Option<String>,
    /// "Navidrome", "Gonic", … from the server's own answer.
    pub name: Option<String>,
    pub version: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SubsonicServerForm {
    pub url: String,
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubsonicPlaylist {
    pub id: String,
    pub name: String,
    pub songs: usize,
}

// ---- Jellyfin (jellyfin.rs) ----

/// The signed-in Jellyfin as the UI sees it: never the token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JellyfinView {
    pub connected: bool,
    pub url: Option<String>,
    pub user: Option<String>,
    pub server_name: Option<String>,
    pub version: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct JellyfinSignIn {
    pub url: String,
    pub user: String,
    #[serde(default)]
    pub password: String,
}

// ---- ListenBrainz (listenbrainz.rs) ----

/// The connection as the UI sees it: never the token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenBrainzView {
    pub connected: bool,
    pub user: Option<String>,
    /// Whether plays are sent.
    pub submit: bool,
    /// Listens waiting to be sent.
    pub pending: usize,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ListenBrainzForm {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub submit: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListenImport {
    pub imported: usize,
    /// Listens ListenBrainz could not name by a MusicBrainz recording.
    pub skipped: usize,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

// ---- bulk upload to the homeserver's private storage (files.rs) ----

/// `POST /v1/pubky/files/upload`: what to copy. One of `album_id`, `track_ids` or `all`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UploadRequest {
    #[serde(default)]
    pub album_id: Option<i64>,
    #[serde(default)]
    pub track_ids: Vec<i64>,
    /// The whole library.
    #[serde(default)]
    pub all: bool,
    /// Only say how many files and bytes it would be.
    #[serde(default)]
    pub dry_run: bool,
}

/// A bulk upload: planned (`dry_run`), running or finished.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UploadProgress {
    pub id: String,
    pub total: usize,
    pub bytes_total: u64,
    /// Uploaded now.
    pub done: usize,
    pub bytes_done: u64,
    /// Already on the homeserver (same content), not sent again.
    pub skipped: usize,
    pub failed: usize,
    /// The file being sent.
    pub current: Option<String>,
    pub finished: bool,
    /// The last few problems, plainly.
    pub errors: Vec<String>,
}
