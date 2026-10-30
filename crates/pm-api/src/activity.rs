//! Listening activity and playlist import for integrations such as the Roon
//! bridge sidecar: callers send what they know (title, artist, album, or a
//! canonical URL) and the node resolves it against the library so that
//! only canonical identities enter social data (constraint 4). A play is a
//! music `listen` post, private or public as the listen history setting says,
//! or nothing when it is off ([`crate::listens`]).

use crate::playlists::store;
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::Library;
use pm_model::{MusicUri, Track};
use pm_pubky::schemas::post::TYPE_ACTIVITY;
use pm_pubky::{ActivitySource, MusicContent, MusicPost, Playlist, PlaylistItem, Visibility};
use std::sync::Arc;

/// Find a library track for a described item: by canonical URL when given,
/// else by a title + artist search (first hit whose title matches).
pub(crate) fn resolve_described(lib: &Library, d: &DescribedTrack) -> Result<Option<Track>, ApiError> {
    if let Some(uri) = d.uri.as_deref().filter(|u| !u.trim().is_empty()) {
        let m = MusicUri::parse(uri).map_err(|e| ApiError::bad_request(format!("{uri}: {e}")))?;
        if let Some(t) = lib.tracks_by_canonical(&m)?.into_iter().next() {
            return Ok(Some(t));
        }
    }
    Ok(lib.resolve_import(&pm_library::ImportedTrack {
        title: d.title.clone(),
        artist: d.artist.clone(),
        album: d.album.clone(),
        path: d.path.clone(),
        duration_ms: None,
    })?)
}

fn source_of(s: &str) -> ActivitySource {
    match s.trim().to_lowercase().as_str() {
        "" | "local" => ActivitySource::Local,
        "roon" => ActivitySource::Roon,
        other => ActivitySource::Service {
            name: other.to_string(),
        },
    }
}

/// Record a play. The item must resolve to a canonical identity: a
/// canonical URL in the request, or a library track that has one. With the
/// history off nothing is written (`recorded: false`).
pub(crate) async fn record(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<RecordActivity>,
) -> ApiResult<RecordedActivity> {
    let lib = s.library.lock().expect("library poisoned");
    let track = resolve_described(&lib, &body.track)?;
    let uri = match (&track, body.track.uri.as_deref()) {
        (Some(t), _) if t.publishable_uri().is_some() => t.publishable_uri().cloned(),
        (_, Some(u)) if !u.trim().is_empty() => {
            Some(MusicUri::parse(u).map_err(|e| ApiError::bad_request(e.to_string()))?)
        }
        _ => None,
    };
    let Some(uri) = uri else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "“{}” has no canonical identity in this library; nothing recorded",
                body.track.title
            ),
        ));
    };
    // Where the listen history setting says; nothing when it is off
    let id = crate::listens::record(&lib, &uri, source_of(&body.source), body.duration_ms, None)?;
    Ok(Json(RecordedActivity {
        recorded: id.is_some(),
        id: id.unwrap_or_default(),
        uri: uri.to_url(),
        track_id: track.and_then(|t| t.id.map(|i| i.0)),
    }))
}

pub(crate) async fn list(
    State(s): State<Arc<NodeState>>,
    Query(p): Query<Page>,
) -> ApiResult<Vec<ActivityEntry>> {
    let lib = s.library.lock().expect("library poisoned");
    let limit = p.limit.unwrap_or(100).min(1000) as usize;
    let mut out = Vec::new();
    for row in lib.list_music_posts(Some(TYPE_ACTIVITY))?.into_iter().take(limit) {
        let Ok(post) = serde_json::from_str::<pubky_app_specs::PubkyAppPost>(&row.json) else {
            continue;
        };
        let Ok(Some(music)) = MusicPost::from_post(post, Some(&row.id)) else {
            continue;
        };
        if let Some(a) = music.as_activity() {
            let track = MusicUri::parse(&a.uri)
                .ok()
                .and_then(|m| lib.tracks_by_canonical(&m).ok())
                .and_then(|v| v.into_iter().next());
            out.push(ActivityEntry {
                id: row.id.clone(),
                uri: a.uri.clone(),
                played_at: a.played_at,
                source: a.source_kind.clone(),
                duration_ms: a.duration_ms,
                track,
                synced: row.synced_at.is_some(),
            });
        }
    }
    Ok(Json(out))
}

/// Create a playlist from described tracks (a Roon playlist, a CSV, …):
/// every track that resolves to a canonical identity becomes an item,
/// the rest are reported back so the caller can show what was lost.
pub(crate) async fn import_playlist(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<ImportPlaylist>,
) -> ApiResult<ImportedPlaylist> {
    import_described(&s, body.name, body.description, body.tracks).map(Json)
}

/// Import a playlist file (M3U/M3U8, PLS, CSV, Roon's Excel export) sent as the
/// raw body. Entries resolve by path, file name, then title and artist.
pub(crate) async fn import_playlist_file(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<ImportFileQuery>,
    body: axum::body::Bytes,
) -> ApiResult<ImportedPlaylist> {
    let format = match q.format.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("m3u") | Some("m3u8") => Some(pm_library::ImportFormat::M3u),
        Some("pls") => Some(pm_library::ImportFormat::Pls),
        Some("csv") | Some("tsv") => Some(pm_library::ImportFormat::Csv),
        Some("xlsx") => Some(pm_library::ImportFormat::Xlsx),
        Some(other) => return Err(ApiError::bad_request(format!("unknown format {other}"))),
        None => pm_library::import::detect_format(q.file.as_deref(), &body),
    };
    let format = format
        .ok_or_else(|| ApiError::bad_request(pm_library::import::ImportError::UnknownFormat.to_string()))?;
    let list = pm_library::import::parse(&body, format, None)
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let name = q
        .name
        .filter(|n| !n.trim().is_empty())
        .or(list.name)
        .or_else(|| {
            q.file
                .as_deref()
                .map(|f| pm_library::import::name_from_file(std::path::Path::new(f)))
        })
        .unwrap_or_else(|| "Imported playlist".into());
    let tracks: Vec<DescribedTrack> = list.tracks.into_iter().map(Into::into).collect();
    import_described(&s, name, None, tracks).map(Json)
}

/// A playlist from described tracks (the Roon import uses it too).
pub(crate) fn import_tracks(
    s: &Arc<NodeState>,
    name: String,
    description: Option<String>,
    tracks: Vec<DescribedTrack>,
) -> Result<ImportedPlaylist, ApiError> {
    import_described(s, name, description, tracks)
}

fn import_described(
    s: &Arc<NodeState>,
    name: String,
    description: Option<String>,
    tracks: Vec<DescribedTrack>,
) -> Result<ImportedPlaylist, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let parsed = tracks.len();
    let mut items = Vec::new();
    let mut unresolved = Vec::new();
    let mut kept = 0;
    for d in &tracks {
        match bridge(&lib, d)? {
            Bridged::Identified(u) => items.push(PlaylistItem::of(u)),
            Bridged::Kept(u) => {
                items.push(PlaylistItem::of(u));
                kept += 1;
                unresolved.push(d.clone());
            }
            Bridged::Empty => unresolved.push(d.clone()),
        }
    }
    if items.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "none of the lines names a song",
        ));
    }
    let playlist = Playlist::new(name.clone(), description, None, items);
    let post = MusicPost::new(MusicContent::Playlist(playlist));
    let id = post.create_id();
    post.validate(Some(&id))
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    store(&lib, &post, &id, Visibility::Private)?;
    // Matched songs only, as every importer counts them; the kept links are `kept`
    let imported = post.as_playlist().map(|p| p.items.len()).unwrap_or(0) - kept;
    drop(lib);
    s.notify_library();
    Ok(ImportedPlaylist {
        id,
        name,
        parsed,
        imported,
        unresolved,
        kept,
    })
}

/// What an import line became (TASK-pinakes-social-ux.md B4).
enum Bridged {
    /// Its canonical identity: the library song's, a MusicBrainz recording, an ISRC, a service link.
    Identified(String),
    /// Unmatched, kept as a link: the web address the line named, else a MusicBrainz search for
    /// its words, so the line is never lost and can be matched by hand later.
    Kept(String),
    /// Nothing to keep (no words, no address).
    Empty,
}

/// One import line to a collection item, the same matching for every format (M3U, PLS, CSV, the
/// Roon export, a service playlist): the library first, then the MusicBrainz id, the ISRC, a
/// music link, then the line kept as a link.
fn bridge(lib: &pm_library::Library, d: &DescribedTrack) -> Result<Bridged, ApiError> {
    // A line's address may sit in `uri` or, in a playlist file, in its path
    let addresses: Vec<&str> = [d.uri.as_deref(), d.path.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect();
    let music = addresses.iter().find_map(|a| MusicUri::parse(a).ok());
    let lookup = DescribedTrack {
        uri: music.as_ref().map(MusicUri::to_url),
        ..d.clone()
    };
    let found = resolve_described(lib, &lookup)?;
    if let Some(u) = found.as_ref().and_then(|t| t.publishable_uri()) {
        return Ok(Bridged::Identified(u.to_url()));
    }
    let named = d
        .mbid
        .as_deref()
        .and_then(|m| pm_model::ids::Mbid::parse(m.trim()).ok())
        .map(MusicUri::recording)
        .or_else(|| {
            d.isrc
                .as_deref()
                .and_then(|i| pm_model::ids::Isrc::parse(i.trim()).ok())
                .map(|isrc| MusicUri::Isrc { isrc })
        })
        .or(music);
    if let Some(u) = named {
        return Ok(Bridged::Identified(u.to_url()));
    }
    // A song of the library not identified yet: unresolved, so importing again after
    // identification finds it (a search link would stand in for it for good)
    if found.is_some() {
        return Ok(Bridged::Empty);
    }
    if let Some(link) = addresses.iter().find_map(|a| kept_link(a)) {
        return Ok(Bridged::Kept(link));
    }
    let stem = d
        .path
        .as_deref()
        .map(pm_library::import::title_from_path)
        .filter(|t| !t.is_empty());
    let title = Some(d.title.trim())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or(stem);
    Ok(search_link(d.artist.as_deref(), title.as_deref()).map_or(Bridged::Empty, Bridged::Kept))
}

/// The longest link a collection keeps: a social v1 reference's limit, so the collection still
/// syncs.
const LINK_MAX: usize = pubky_social_specs::VALIDATION_LIMITS.reference_uri_max_length;

/// Host names that only mean something on one network (or one person's devices).
const LAN_SUFFIXES: [&str; 16] = [
    ".local",
    ".lan",
    ".home",
    ".internal",
    ".intern",
    ".localhost",
    ".localdomain",
    ".arpa",
    ".intranet",
    ".corp",
    ".private",
    ".box",
    ".router",
    ".ts.net",
    ".plex.direct",
    ".myfritz.net",
];

/// Path parts of an address that plays or hands out a file rather than shows a page: a share
/// link carries its secret in the path (Nextcloud's `/s/<token>`, Navidrome's `/share/<id>`), and
/// a stream or download is someone's server, never a page to show.
const FILE_PARTS: [&str; 11] = [
    "s",
    "share",
    "shares",
    "stream",
    "streams",
    "download",
    "downloads",
    "rest",
    "audio",
    "media",
    "items",
];

/// Audio and playlist files by their extension.
const AUDIO_EXTENSIONS: [&str; 20] = [
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "m4b", "aac", "wav", "aif", "aiff", "alac", "wma", "ape",
    "wv", "dsf", "dff", "m3u", "m3u8", "pls",
];

/// The address of a file or stream rather than a page (see [`FILE_PARTS`]).
fn plays_a_file(u: &url::Url) -> bool {
    let segments: Vec<String> = u
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).map(str::to_ascii_lowercase).collect())
        .unwrap_or_default();
    let audio_ext = segments
        .last()
        .and_then(|l| l.rsplit_once('.'))
        .is_some_and(|(_, ext)| AUDIO_EXTENSIONS.contains(&ext));
    // An Icecast mount (`/mp3-192`, `/live`, `/listen`) or a named stream
    let stream = segments.iter().any(|p| {
        FILE_PARTS.contains(&p.as_str())
            || p.starts_with("stream")
            || matches!(p.as_str(), "live" | "listen" | "radio" | "icecast")
            || ["mp3", "aac", "ogg", "opus", "flac"].iter().any(|c| {
                p.strip_prefix(c)
                    .is_some_and(|r| r.is_empty() || r.starts_with(['-', '_', '.']))
            })
    });
    audio_ext || stream
}

/// The start of every MusicBrainz search link [`search_link`] makes.
const MB_SEARCH: &str = "https://musicbrainz.org/search?type=recording&query=";

/// Whether `uri` is a search link the node made for an unmatched line: its words are a private
/// playlist's, so it is never shown through Pinakes (`GET /v1/cards` answers it without asking).
pub(crate) fn is_search_link(uri: &str) -> bool {
    uri.starts_with(MB_SEARCH)
}

/// A line's web address, kept as a link only when it is safe to keep and to share. Playlist
/// files from a media server name streams with their sign-in in them (Jellyfin's `api_key`,
/// Subsonic's `t` and `s`, Plex's `X-Plex-Token`, `user:pass@`), and a kept link is shown
/// through Pinakes and published with the collection. So: no userinfo, query or fragment, a
/// public host (a name, not an IP address, `localhost` or a LAN name) on the usual port, no file,
/// share or stream address ([`plays_a_file`]: the secret may be in the path, and the line's own
/// words say more), and no longer than a v1 reference. Anything else falls back to the words.
fn kept_link(raw: &str) -> Option<String> {
    let link = pm_pubky::schemas::web_link(raw)?;
    let u = url::Url::parse(&link).ok()?;
    let public_host = match u.host()? {
        url::Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d.contains('.') && !LAN_SUFFIXES.iter().any(|s| d.ends_with(s) || d == s[1..])
        }
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => false,
    };
    let bare = u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none()
        && u.fragment().is_none()
        && u.port().is_none()
        && !plays_a_file(&u);
    (public_host && bare && link.len() <= LINK_MAX).then_some(link)
}

/// A MusicBrainz recording search for an unmatched song's words, kept as a collection link so
/// the line isn't lost; `None` without any words. An address among the words loses its sign-in
/// first (`without_secrets`), and long words are cut so the link stays within a v1 reference.
pub(crate) fn search_link(artist: Option<&str>, title: Option<&str>) -> Option<String> {
    const SEARCH: &str = MB_SEARCH;
    let words = [artist, title]
        .into_iter()
        .flatten()
        .map(|w| pm_library::import::without_secrets(w).trim().to_string())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut query = String::new();
    for c in words.chars() {
        let mut buf = [0; 4];
        let part: String = url::form_urlencoded::byte_serialize(c.encode_utf8(&mut buf).as_bytes()).collect();
        if SEARCH.len() + query.len() + part.len() > LINK_MAX {
            break;
        }
        query.push_str(&part);
    }
    let query = query.trim_end_matches('+');
    (!query.is_empty()).then(|| format!("{SEARCH}{query}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unmatched_song_is_kept_as_a_musicbrainz_search() {
        assert_eq!(
            super::search_link(Some(" Nobody "), Some("Mystery & Song")).as_deref(),
            Some("https://musicbrainz.org/search?type=recording&query=Nobody+Mystery+%26+Song")
        );
        assert_eq!(
            super::search_link(None, Some("Only a title")).as_deref(),
            Some("https://musicbrainz.org/search?type=recording&query=Only+a+title")
        );
        assert_eq!(super::search_link(Some("  "), None), None);
        // Words that are an address lose its sign-in
        assert_eq!(
            super::search_link(
                None,
                Some("https://a:SECRET@radio.example.com/live?token=SECRET#SECRET")
            )
            .as_deref(),
            Some(
                "https://musicbrainz.org/search?type=recording&query=https%3A%2F%2Fradio.example.com%2Flive"
            )
        );
        // Percent-encoding takes 9 bytes a CJK character: cut to fit a v1 reference
        let long = super::search_link(Some("Artist"), Some(&"曲".repeat(500))).unwrap();
        assert!(long.len() <= super::LINK_MAX, "{}", long.len());
        assert!(long.starts_with("https://musicbrainz.org/search?type=recording&query=Artist+%E6%9B%B2"));
    }

    #[test]
    fn a_link_with_a_sign_in_or_on_the_lan_is_not_kept() {
        for ok in [
            "https://example.com/mixtape-notes",
            "https://www.discogs.com/release/249504-Daft-Punk-Discovery",
        ] {
            assert_eq!(super::kept_link(&format!(" {ok} ")).as_deref(), Some(ok), "{ok}");
        }
        assert!(super::is_search_link(
            &super::search_link(Some("A"), Some("B")).unwrap()
        ));
        assert!(!super::is_search_link("https://musicbrainz.org/recording/x"));
        for bad in [
            "http://jellyfin.example.com/Audio/x/stream.mp3?api_key=secret",
            "https://music.example.com/rest/stream.view?u=admin&t=abc&s=def",
            "https://plex.example.com/library/parts/1/file.mp3?X-Plex-Token=secret",
            "https://user:pass@stream.example.com/live.mp3",
            "https://token@stream.example.com/live.mp3",
            "https://example.com/s.mp3#access_token=secret",
            "http://nas:8096/Audio/x/stream.mp3",
            "http://192.168.1.20/music/a.mp3",
            "http://[fd00::1]/a.mp3",
            "http://localhost:4533/a.mp3",
            "http://nas.local/a.mp3",
            "http://box.home.arpa/a.mp3",
            "https://pc.tail1234.ts.net/a.mp3",
            // A share's secret in the path, a stream, a file, another port, a home router's name
            "https://cloud.example.com/s/Ab12Token/download",
            "https://music.example.com/share/xyz",
            "https://jellyfin.example.com/Items/abc/Download",
            "https://stream.radioparadise.com/mp3-192",
            "https://radio.example.com/live",
            "https://cdn.example.com/album/song.FLAC",
            "https://music.example.com:4533/x",
            "http://nas.fritz.box/x",
            "https://1-2-3-4.abc.plex.direct/x",
        ] {
            assert_eq!(super::kept_link(bad), None, "{bad}");
        }
        let long = format!("https://cdn.example.com/{}", "a".repeat(super::LINK_MAX));
        assert_eq!(super::kept_link(&long), None, "longer than a v1 reference");
    }
}
