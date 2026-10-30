//! The node's native API (handoff §9): HTTP + WebSocket under `/v1`.
//!
//! Resources: `info`, `outputs`, `zones` (queue, transport, volume, repeat),
//! `library` (stats, tracks, albums, search, scan) and the event stream at
//! `/v1/events`. Bodies are in [`types`]; the same JSON shapes drive the
//! controller. OpenSubsonic (M4) will be a second router next to this one.
//!
//! Auth: an optional bearer token (`NodeState::token`). Without one the API
//! trusts whoever can reach the socket, so bind to localhost. Remote access
//! by pubkey (M5) wraps this API rather than replacing it.

pub mod about;
pub mod access_token;
pub mod activity;
pub mod archive;
pub mod art;
pub mod artist_info;
pub mod audius;
pub mod renderer;

/// "1 album", "2 albums".
pub(crate) fn plural(n: u64, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

pub mod accounts;
pub mod add_music;
pub mod announce;
pub mod apps;
pub mod audiobookshelf;
pub mod backup;
pub mod bandcamp;
pub mod bookmarks;
pub mod bundled;
pub mod dash;
pub mod deezer;
pub mod devices;
pub mod discover;
pub mod downloads;
pub mod drives;
pub mod events;
pub mod files;
pub mod first_run;
pub mod first_steps;
pub mod gpodder;
pub mod groups;
pub mod health;
pub mod hls;
pub mod home;
pub mod identify;
pub mod imports;
pub mod item;
pub mod jamendo;
pub mod jellyfin;
pub mod lastfm;
pub mod library_match;
pub mod librespot;
pub mod listenbrainz;
pub mod listening_share;
pub mod listens;
pub mod loudness;
pub mod lrclib;
pub mod mbproxy;
pub mod new_releases;
pub mod opensubsonic;
pub mod party;
pub mod peer;
pub mod people;
pub mod pinakes_reader;
pub mod play_from;
pub mod playlists;
pub mod plex;
pub mod podcasts;
pub mod prefs;
pub mod profiles;
pub mod project_apps;
pub mod qobuz;
pub mod quality;
pub mod radio;
pub mod radio_sources;
pub mod remote;
pub mod reviews;
pub mod roon_import;
pub mod search;
pub mod shared;
pub mod signin;
pub mod smart;
pub mod soloist;
pub mod soundcloud;
pub mod sources;
pub mod spotify;
pub mod state;
pub mod storage;
pub mod streaming;
pub mod subsonic;
pub mod sync;
pub mod tag_fixes;
pub mod tags;
pub mod tidal;
pub mod tidal_unofficial;
pub mod tunnel;
pub mod types;
pub mod vault;
pub mod vault_cache;
pub mod watch;
mod web;
pub mod webdav;
pub mod youtube;
pub mod ytdlp;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{middleware, Json, Router};
use futures_util::StreamExt;
use pm_model::TrackId;
use pm_zones::{Insert, Output, OutputId, OutputInfo, QueueItem, Zone, ZoneId, ZoneState};
use sources::NodeSources;
pub use state::{NodeSettings, NodeState};
use std::sync::Arc;
use types::*;

// ---- errors ----

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub(crate) fn not_found(what: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, what)
    }
    pub(crate) fn bad_request(what: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, what)
    }
}

impl From<pm_library::LibraryError> for ApiError {
    fn from(e: pm_library::LibraryError) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl From<pm_zones::SinkError> for ApiError {
    fn from(e: pm_zones::SinkError) -> Self {
        Self::new(StatusCode::BAD_REQUEST, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorBody { error: self.message })).into_response()
    }
}

pub(crate) type ApiResult<T> = Result<Json<T>, ApiError>;

// ---- router ----

/// The framework's own refusals (a JSON body that doesn't fit, a wrong method) come as plain
/// text such as "Failed to deserialize the JSON body…". Pages show `error` to people, so those
/// become the node's error shape with a plain sentence; the detail goes to the log and `detail`
/// (STEWARD-FIX-same-artist.md: no raw errors in the UI).
async fn plain_errors(resp: Response) -> Response {
    let st = resp.status();
    let text = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("text/plain"));
    if !(st.is_client_error() || st.is_server_error()) || !text || st == StatusCode::UNAUTHORIZED {
        return resp;
    }
    let (_, body) = resp.into_parts();
    let raw = axum::body::to_bytes(body, 64 * 1024).await.unwrap_or_default();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    tracing::warn!(status = %st, "request refused: {raw}");
    let error = match st.as_u16() {
        400 | 415 | 422 => "The node didn't understand that request. Reload the page and try again.",
        404 => "That isn't here (any more).",
        405 => "The node can't do that here.",
        413 => "That is too big for the node to take.",
        _ => "Something went wrong on the node. Try again.",
    };
    (st, Json(serde_json::json!({ "error": error, "detail": raw }))).into_response()
}

/// Marks a response from a deprecated path (RFC 9745 `Deprecation`, and the path to use).
async fn deprecated(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert("deprecation", axum::http::HeaderValue::from_static("true"));
    h.insert(
        axum::http::header::LINK,
        axum::http::HeaderValue::from_static("</v1/bookmarks>; rel=\"successor-version\""),
    );
    resp
}

pub fn router(state: Arc<NodeState>) -> Router {
    let v1 = Router::new()
        .route("/info", get(info))
        .route("/health", get(health::health))
        .route("/node", get(groups::index))
        .route("/node/apps", get(apps::get))
        .route("/node/apps/{id}", put(apps::put))
        .route("/node/settings", get(node_settings).put(update_node_settings))
        .route("/node/token", post(access_token::replace))
        .route("/node/settings/backup", post(settings_backup_now))
        .route("/node/settings/restore", post(settings_restore_now))
        .route("/outputs", get(outputs))
        .route("/outputs/rescan", post(rescan_outputs))
        .route("/outputs/discovery", get(discovery_report))
        .route("/outputs/upnp", post(add_upnp_output))
        .route("/outputs/upnp/{address}", delete(remove_upnp_address))
        .route("/outputs/endpoints", post(add_endpoint))
        .route("/outputs/endpoints/{address}", delete(remove_endpoint))
        .route("/outputs/pull", post(create_pull))
        .route("/outputs/pull/{name}/url", get(pull_url))
        .route("/zones", get(zones).post(create_zone))
        .route("/zones/{id}", get(zone).delete(delete_zone))
        .route("/zones/{id}/queue", get(queue).post(enqueue).delete(clear_queue))
        .route(
            "/zones/{id}/queue/{item}",
            axum::routing::delete(remove_item).put(move_item),
        )
        .route("/zones/{id}/transport", post(transport))
        .route("/zones/{id}/announce", post(announce::announce))
        .route("/zones/{id}/volume", post(volume))
        .route("/zones/{id}/repeat", axum::routing::put(repeat))
        .route("/zones/{id}/radio", axum::routing::put(radio))
        .route("/zones/{id}/crossfade", axum::routing::put(crossfade))
        .route("/zones/{id}/shuffle", post(shuffle_queue))
        .route("/zones/{id}/transfer", post(transfer))
        .route("/zones/{id}/load", post(load))
        .route("/zones/{id}/sleep", get(sleep_get).put(sleep_set))
        .route("/zones/{id}/dsp", get(dsp_get).put(dsp_put).delete(dsp_reset))
        .route("/library/stats", get(library_stats))
        .route("/library/tracks", get(tracks))
        .route("/library/tracks/{id}", get(track))
        .route("/library/tracks/{id}/lyrics", get(lyrics))
        .route("/lyrics", get(lrclib::get))
        .route("/library/tracks/{id}/versions", get(track_versions))
        .route("/library/albums/{id}/versions", get(album_versions))
        .route("/backup", get(backup::get_backup).post(backup::post_backup))
        .route(
            "/restore",
            post(backup::post_restore).layer(axum::extract::DefaultBodyLimit::max(4 << 30)),
        )
        .route(
            "/backup/schedule",
            get(backup::get_schedule).put(backup::put_schedule),
        )
        .route("/backup/schedule/run", post(backup::run_schedule))
        .route("/library/folders/move", post(move_folder))
        .route("/library/tracks/{id}/file", get(track_file))
        .route("/library/tracks/{id}/download", get(downloads::download))
        .route("/library/search", get(search))
        .route("/library/loudness", get(loudness::get))
        .route("/library/loudness/measure", post(loudness::measure))
        .route("/library/matches", get(library_match::status))
        .route("/library/artists", get(artists))
        .route("/library/artists/{id}", get(artist))
        .route("/library/artists/{id}/tracks", get(artist_tracks))
        .route("/library/artists/{id}/info", get(artist_info_handler))
        .route("/library/artists/{id}/image", get(artist_image))
        .route("/library/genres", get(genres))
        .route("/library/genres/{id}", get(genre))
        .route("/library/albums", get(albums))
        .route("/library/albums/formats", get(album_formats))
        .route("/library/new-releases", get(new_releases))
        .route("/library/new-releases/refresh", post(new_releases_refresh))
        .route("/library/albums/{id}/tracks", get(album_tracks))
        .route("/library/albums/{id}/cover", get(album_cover))
        .route("/library/history", get(history).post(record_play))
        .route("/library/home", get(home))
        .route(
            "/library/folders",
            get(folders).post(add_folder).delete(remove_folder),
        )
        .route("/library/folders/suggest", get(first_run::suggest_folders))
        .route("/library/scan", post(scan))
        // Collections: the same resource as /playlists (playlists.rs)
        .route("/collections", get(playlists::list).post(playlists::create))
        .route(
            "/collections/{id}",
            get(playlists::get)
                .put(playlists::update)
                .delete(playlists::delete),
        )
        .route("/collections/{id}/items", post(playlists::add_items))
        .route(
            "/collections/{id}/items/{index}",
            axum::routing::delete(playlists::remove_item),
        )
        .route(
            "/collections/{id}/local/{index}",
            axum::routing::delete(playlists::remove_local),
        )
        .route("/collections/{id}/play", post(playlists::play))
        .route("/collections/{id}/play-from", get(play_from::playlist_play_from))
        .route("/collections/{id}/order", put(playlists::set_order))
        .route("/collections/{id}/visibility", put(playlists::set_visibility))
        .route(
            "/collections/{id}/items/{index}/source",
            put(playlists::pin_source),
        )
        .route("/playlists", get(playlists::list).post(playlists::create))
        .route("/playlists/from-queue", post(playlists::from_queue))
        .route("/playlists/import", post(activity::import_playlist))
        .route("/playlists/import/file", post(activity::import_playlist_file))
        .route("/import/roon", post(roon_import::import))
        .route("/activity", get(activity::list).post(activity::record))
        .route(
            "/bookmarks",
            get(bookmarks::list).put(bookmarks::put).delete(bookmarks::delete),
        )
        // The name before social v1 bookmarks, for one release (docs/api.md, Deprecated)
        .route(
            "/favourites",
            get(bookmarks::list)
                .put(bookmarks::put)
                .delete(bookmarks::delete)
                .layer(middleware::map_response(deprecated)),
        )
        .route("/tags", get(tags::list).put(tags::put).delete(tags::delete))
        .route("/tags/bulk", post(tags::bulk))
        .route(
            "/reviews",
            get(reviews::list).put(reviews::put).delete(reviews::delete),
        )
        .route(
            "/playlists/{id}",
            get(playlists::get)
                .put(playlists::update)
                .delete(playlists::delete),
        )
        .route("/playlists/{id}/items", post(playlists::add_items))
        .route(
            "/playlists/{id}/local/{index}",
            axum::routing::delete(playlists::remove_local),
        )
        .route(
            "/playlists/{id}/items/{index}",
            axum::routing::delete(playlists::remove_item),
        )
        .route("/playlists/{id}/play", post(playlists::play))
        .route("/playlists/{id}/play-from", get(play_from::playlist_play_from))
        .route("/playlists/{id}/order", put(playlists::set_order))
        .route("/playlists/{id}/visibility", put(playlists::set_visibility))
        .route("/playlists/{id}/items/{index}/source", put(playlists::pin_source))
        .route("/shared/resolve", get(shared::resolve))
        .route("/shared/{user}/playlists", get(shared::list))
        .route("/shared/{user}/playlists/{id}", get(shared::get))
        .route("/shared/{user}/playlists/{id}/copy", post(shared::duplicate))
        .route("/shared/{user}/collections", get(shared::list_collections))
        .route("/shared/{user}/collections/{id}", get(shared::get_collection))
        .route(
            "/shared/{user}/collections/{id}/duplicate",
            post(shared::duplicate),
        )
        .route("/play-order", get(play_from::get_order).put(play_from::set_order))
        .route("/discover/trends", get(discover::trends))
        .route("/discover/charts", get(discover::charts))
        .route("/discover/search", get(discover::search))
        .route("/discover/item", get(discover::item_detail))
        .route("/discover/play", get(discover::play_plan))
        .route("/discover/describe", post(discover::describe))
        .route("/discover/contents", get(discover::contents))
        .route("/search", get(search::search))
        .route("/search/tags", get(search::tag_suggestions))
        .route("/home/location", get(home::get_location).put(home::set_location))
        .route("/home/layout", get(home::get_layout).put(home::set_layout))
        .route("/smart", get(smart::list).post(smart::create))
        .route("/smart/preview", post(smart::preview))
        .route(
            "/smart/{id}",
            axum::routing::put(smart::update).delete(smart::delete),
        )
        .route("/smart/{id}/tracks", get(smart::tracks))
        .route("/home/{section}", get(home::section))
        .route("/item/{kind}/{id}", get(item::page))
        .route("/item/{kind}/{id}/extras", get(item::page_extras))
        .route(
            "/library/identify",
            get(identify::get_status).post(identify::start),
        )
        .route("/library/identify/settings", put(identify::put_settings))
        .route("/library/identify/write", post(identify::write_now))
        .route("/library/tag-fixes", get(tag_fixes::list).post(tag_fixes::apply))
        .route("/library/suggestions", get(identify::suggestions))
        .route("/library/tracks/{id}/candidates", get(identify::track_candidates))
        .route("/library/suggestions/{id}/accept", post(identify::accept))
        .route("/library/suggestions/{id}", delete(identify::reject))
        .route("/discover/uri", get(discover::on_pubky))
        .route("/discover/tag/{label}", get(discover::by_tag))
        .route("/discover/friends/{viewer}", get(discover::friends))
        .route("/sources", get(streaming::list))
        .route("/sources/{source}/search", get(streaming::search))
        .route(
            "/sources/{source}/enabled",
            axum::routing::put(streaming::set_enabled),
        )
        .route(
            "/sources/{source}/signin",
            get(signin::get).put(signin::put).delete(signin::sign_out),
        )
        .route("/sources/{source}/quality", get(quality::get).put(quality::put))
        .route("/sources/{source}/signin/import", post(signin::import))
        .route("/sources/{source}/items/{id}", get(streaming::item))
        .route("/sources/{source}/artists/{id}", get(streaming::artist))
        .route("/sources/tidal/login", get(tidal::login))
        .route("/sources/tidal/status", get(tidal::status))
        .route(
            "/sources/tidal/unofficial",
            get(tidal_unofficial::get_status).put(tidal_unofficial::put),
        )
        .route(
            "/sources/tidal/unofficial/login",
            post(tidal_unofficial::login).delete(tidal_unofficial::logout),
        )
        .route("/sources/tidal/session", axum::routing::delete(tidal::logout))
        .route("/sources/tidal/collection", get(tidal::collection))
        .route("/sources/tidal/callback", post(tidal::pasted_callback))
        .route("/sources/spotify/callback", post(spotify::pasted_callback))
        .route("/sources/tidal/stream/{id}", get(tidal::stream))
        .route("/sources/deezer/stream/{id}", get(deezer::stream))
        .route("/sources/jamendo/stream/{id}", get(jamendo::stream))
        .route("/sources/qobuz/stream/{id}", get(qobuz::stream))
        .route(
            "/sources/qobuz/unofficial",
            get(qobuz::get_status).put(qobuz::put),
        )
        .route(
            "/sources/qobuz/unofficial/login",
            post(qobuz::login).delete(qobuz::logout),
        )
        .route(
            "/sources/youtube/key",
            get(youtube::get_key).put(youtube::put_key),
        )
        .route(
            "/sources/youtube/unofficial",
            get(ytdlp::get_status).put(ytdlp::put),
        )
        .route(
            "/sources/youtube/unofficial/ytdlp",
            post(ytdlp::install).delete(ytdlp::uninstall),
        )
        .route("/sources/bandcamp/stream/{key}", get(bandcamp::stream))
        .route(
            "/sources/bandcamp/unofficial",
            get(bandcamp::get_status).put(bandcamp::put),
        )
        .route("/sources/bandcamp/collection", get(bandcamp::collection))
        .route(
            "/sources/bandcamp/collection/{id}/download",
            post(bandcamp::download),
        )
        .route("/sources/bandcamp/downloads", get(bandcamp::downloads))
        .route(
            "/sources/spotify/soloist",
            get(soloist::get_status).put(soloist::put),
        )
        .route(
            "/sources/spotify/soloist/binary",
            post(soloist::install).delete(soloist::uninstall),
        )
        .route("/sources/spotify/soloist/control", post(soloist::control))
        .route(
            "/sources/spotify/librespot",
            get(librespot::get_status).put(librespot::put),
        )
        .route("/sources/soundcloud/stream/{id}", get(soundcloud::stream))
        .route(
            "/sources/soundcloud/unofficial",
            get(soundcloud::get_status).put(soundcloud::put),
        )
        .route("/sources/soundcloud/likes", get(soundcloud::likes))
        .route("/sources/soundcloud/likes/import", post(soundcloud::import_likes))
        .route("/sources/soundcloud/playlists", get(soundcloud::playlists))
        .route(
            "/sources/soundcloud/playlists/{id}/import",
            post(soundcloud::import_playlist),
        )
        .route(
            "/sources/subsonic/server",
            get(opensubsonic::get_server)
                .put(opensubsonic::set_server)
                .delete(opensubsonic::forget_server),
        )
        .route("/sources/subsonic/stream/{id}", get(opensubsonic::stream))
        .route(
            "/sources/jellyfin/server",
            get(jellyfin::get_server)
                .put(jellyfin::sign_in)
                .delete(jellyfin::sign_out),
        )
        .route("/sources/jellyfin/stream/{id}", get(jellyfin::stream))
        .route(
            "/listenbrainz",
            get(listenbrainz::status)
                .put(listenbrainz::connect)
                .delete(listenbrainz::disconnect),
        )
        .route("/listenbrainz/import", post(listenbrainz::import_listens))
        .route(
            "/lastfm",
            get(lastfm::status).put(lastfm::update).delete(lastfm::disconnect),
        )
        .route("/lastfm/login", get(lastfm::login))
        .route("/library/artists/{id}/similar", get(lastfm::similar))
        .route(
            "/listenbrainz/recommendations",
            get(listenbrainz::recommendations),
        )
        .route("/listenbrainz/playlists", get(listenbrainz::playlists))
        .route(
            "/listenbrainz/playlists/{id}/import",
            post(listenbrainz::import_playlist),
        )
        .route("/sources/jellyfin/cover/{id}", get(jellyfin::cover))
        .route(
            "/sources/plex/server",
            get(plex::get_server).put(plex::connect).delete(plex::disconnect),
        )
        .route("/sources/plex/pin", post(plex::start_pin))
        .route("/sources/plex/pin/{id}", get(plex::poll_pin))
        .route("/sources/plex/stream/{id}", get(plex::stream))
        .route("/sources/plex/cover/{id}/{ver}", get(plex::cover))
        .route(
            "/sources/audiobookshelf/server",
            get(audiobookshelf::get_server)
                .put(audiobookshelf::connect)
                .delete(audiobookshelf::disconnect),
        )
        .route("/sources/audiobookshelf/play", post(audiobookshelf::play))
        .route(
            "/sources/audiobookshelf/continue",
            get(audiobookshelf::in_progress),
        )
        .route(
            "/sources/audiobookshelf/stream/{item}/{ino}",
            get(audiobookshelf::stream),
        )
        .route("/sources/audiobookshelf/cover/{item}", get(audiobookshelf::cover))
        .route("/sources/jellyfin/playlists", get(jellyfin::playlists))
        .route(
            "/sources/jellyfin/playlists/{id}/import",
            post(jellyfin::import_playlist),
        )
        .route("/sources/subsonic/cover/{id}", get(opensubsonic::cover))
        .route("/sources/subsonic/playlists", get(opensubsonic::playlists))
        .route(
            "/sources/subsonic/playlists/{id}/import",
            post(opensubsonic::import_playlist),
        )
        .route("/sources/jamendo/status", get(jamendo::status))
        .route("/sources/jamendo/login", post(jamendo::login))
        .route("/sources/jamendo/logout", post(jamendo::logout))
        .route(
            "/sources/jamendo/collection/import",
            post(jamendo::import_collection),
        )
        .route("/sources/spotify/login", get(spotify::login))
        .route("/sources/spotify/status", get(spotify::status))
        .route("/sources/spotify/app", put(spotify::set_app))
        .route("/sources/spotify/session", delete(spotify::logout))
        .route("/sources/spotify/collection", get(spotify::collection))
        .route(
            "/sources/spotify/collection/import",
            post(spotify::import_collection),
        )
        .route("/sources/spotify/playlists", get(spotify::playlists))
        .route(
            "/sources/spotify/playlists/{id}/import",
            post(spotify::import_playlist),
        )
        .route("/sources/spotify/devices", get(spotify::devices))
        .route("/sources/spotify/play", post(spotify::play))
        .route("/sources/tidal/collection/import", post(tidal::import_collection))
        .route("/sources/tidal/playlists", get(tidal::playlists))
        .route(
            "/sources/tidal/playlists/{id}/import",
            post(tidal::import_playlist),
        )
        .route("/radio/search", get(radio::search))
        .route("/radio/stations", get(radio::saved).put(radio::save))
        .route("/radio/stations/{id}", axum::routing::delete(radio::remove))
        .route("/radio/play", post(radio::play))
        .route("/podcasts/search", get(podcasts::search))
        .route(
            "/podcasts/subscriptions",
            get(podcasts::subscriptions).post(podcasts::subscribe),
        )
        .route(
            "/podcasts/subscriptions/{id}",
            get(podcasts::subscription).delete(podcasts::unsubscribe),
        )
        .route("/podcasts/new", get(podcasts::newest))
        .route("/podcasts/refresh", post(podcasts::refresh_route))
        .route(
            "/podcasts/gpodder",
            get(gpodder::get).put(gpodder::put).delete(gpodder::delete),
        )
        .route("/podcasts/gpodder/sync", post(gpodder::sync_now))
        .route("/podcasts/play", post(podcasts::play))
        .route(
            "/podcasts/progress/{id}",
            axum::routing::put(podcasts::set_progress),
        )
        .route("/radio/curated", get(radio_sources::curated_route))
        .route("/radio/countries", get(radio_sources::countries))
        .route("/radio/tags", get(radio_sources::tags))
        .route("/homeserver/bundled", get(bundled::status))
        // Everything about the bundled homeserver lives under it ("This homeserver" in Settings);
        // `/v1/remote/tunnel` stays as an alias for one release
        .route(
            "/homeserver/bundled/tunnel",
            get(tunnel::status).put(tunnel::setup).delete(tunnel::remove),
        )
        .route("/homeserver/bundled/owner-signup", post(bundled::owner_signup))
        .route(
            "/remote/tunnel",
            get(tunnel::status).put(tunnel::setup).delete(tunnel::remove),
        )
        .route("/storage", get(storage::status))
        .route("/storage/save", post(storage::save))
        .route("/library/uploads", get(add_music::list).post(add_music::start))
        .route(
            "/library/uploads/{id}",
            get(add_music::get)
                .put(add_music::put_chunk)
                .delete(add_music::cancel)
                .layer(axum::extract::DefaultBodyLimit::max(add_music::CHUNK_LIMIT)),
        )
        .route("/library/imports", post(add_music::import))
        .route("/profiles", get(profiles::list))
        .route("/profiles/{id}", get(profiles::page))
        .route("/profiles/{id}/avatar", get(profiles::avatar))
        .route(
            "/profiles/{id}/follow",
            axum::routing::put(profiles::follow).delete(profiles::unfollow),
        )
        .route("/library/webdav", get(webdav::list).post(webdav::add))
        .route("/library/drives", get(drives::list).post(drives::add))
        .route("/library/drives/{provider}/login", post(drives::login))
        .route("/library/drives/{provider}/folders", get(drives::folders))
        .route(
            "/library/drives/{provider}/account",
            axum::routing::delete(drives::sign_out),
        )
        .route("/library/drives/roots/{id}/scan", post(drives::rescan))
        .route(
            "/library/drives/roots/{id}",
            axum::routing::delete(drives::remove),
        )
        .route("/library/webdav/{id}", axum::routing::delete(webdav::remove))
        .route("/library/webdav/{id}/scan", post(webdav::rescan))
        .route("/me", get(accounts::me))
        .route("/me/prefs", get(prefs::get).put(prefs::put))
        .route("/me/first-steps", get(first_steps::get))
        .route("/me/token", axum::routing::delete(accounts::sign_out))
        .route(
            "/me/listening",
            get(listening_share::get).put(listening_share::put),
        )
        .route("/me/listens", get(listens::list).delete(listens::delete_all))
        .route("/me/listens/stats", get(listens::get_stats))
        .route(
            "/me/listens/settings",
            get(listens::get_settings).put(listens::put_settings),
        )
        .route("/me/listens/visibility", post(listens::move_all))
        .route("/people/zones", get(accounts::get_zones).put(accounts::set_zones))
        .route("/people", get(people::list))
        .route("/devices", get(devices::list))
        .route(
            "/devices/{id}",
            axum::routing::put(devices::register).delete(devices::forget),
        )
        .route(
            "/devices/{id}/downloads",
            get(devices::get_downloads).put(devices::put_downloads).layer(
                axum::extract::DefaultBodyLimit::max(devices::DOWNLOADS_BODY_LIMIT),
            ),
        )
        .route("/vault", get(vault::get))
        .route("/vault/register", post(vault::register))
        .route("/vault/sync", post(vault::sync_now))
        .route("/vault/devices/{key}/sign-out", post(vault::sign_out_device))
        .route("/vault/sign-out-everywhere", post(vault::sign_out_everywhere))
        .route("/vault/new-key", post(vault::new_key))
        .route("/people/invites", post(people::invite))
        .route(
            "/people/invites/{id}",
            axum::routing::delete(people::forget_invite),
        )
        .route("/people/{key}", axum::routing::patch(people::change))
        .route("/pubky", get(pubky_status))
        .route("/pubky/sync", post(pubky_sync))
        .route("/pubky/login", post(pubky_login_start).get(pubky_login_poll))
        .route(
            "/pubky/files",
            get(files::list).post(files::upload).delete(files::remove),
        )
        .route("/pubky/files/mirror", post(files::mirror))
        .route("/pubky/files/upload", post(files::upload_many))
        .route("/pubky/files/upload/{id}", get(files::upload_progress))
        .route("/pubky/files/jobs", get(files::current_job))
        .route("/pubky/files/jobs/{id}", get(files::upload_progress))
        .route("/pubky/files/jobs/{id}/cancel", post(files::cancel_job))
        .route("/events", get(events::events_ws))
        // Party mode: the owner's parties, and what a guest token opens (party.rs)
        .route("/party", get(party::list).post(party::create))
        .route("/party/{id}", delete(party::end))
        .route("/party/guest", get(party::guest))
        .route("/party/guest/search", get(party::search))
        .route("/party/guest/queue", post(party::add))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(middleware::map_response(plain_errors))
        .with_state(state.clone());
    // The web UI's paths (navigation brief): a deep link or reload lands on
    // the same view, so every page path serves index.html.
    let mut web = Router::new()
        .route("/", get(web_ui))
        .route("/app", get(web_ui))
        .route("/app/", get(web_ui))
        .route("/song/{id}", get(web_ui))
        .route("/album/{id}", get(web_ui))
        .route("/artist/{id}", get(web_ui))
        .route("/playlist/{id}", get(web_ui))
        .route("/person/{id}", get(web_ui))
        .route("/smart/{id}", get(web_ui))
        .route("/now-playing", get(web_ui))
        .route("/shared/{user}", get(web_ui))
        .route("/shared/{user}/{id}", get(web_ui))
        .route("/genre/{id}", get(web_ui))
        .route("/source/{id}", get(web_ui))
        .route("/sources/{id}", get(web_ui))
        .route("/settings/{tab}", get(web_ui))
        .route("/home/{section}", get(web_ui))
        .route("/tag/{labels}", get(web_ui))
        .route("/discover", get(web_ui))
        .route("/discover/{*rest}", get(web_ui))
        .route("/search", get(web_ui));
    for view in WEB_VIEWS {
        web = web.route(&format!("/{view}"), get(web_ui));
    }
    web.route("/app.css", get(web_css))
        .route("/ux/", get(ux_index))
        .route("/ux/{*path}", get(ux_file))
        .route("/app.js", get(web_js))
        .route("/manifest.webmanifest", get(web_manifest))
        .route("/icon.svg", get(web_icon))
        .nest("/v1", v1)
        // Reached without a bearer token: the OAuth redirect target and the
        // Pubky Ring sign-in that hands the token out.
        .merge(
            Router::new()
                .route("/tidal/callback", get(tidal::callback))
                .route("/spotify/callback", get(spotify::callback))
                .route("/lastfm/callback", get(lastfm::callback))
                .route("/drives/{provider}/callback", get(drives::callback))
                .route("/jamendo/callback", get(jamendo::callback))
                .route("/v1/auth/pubky/start", post(controller_login_start))
                .route("/v1/auth/pubky/poll", get(controller_login_poll))
                // A party guest's link: the page, and joining it (party.rs)
                .route("/party/{code}", get(party_page))
                .route("/v1/party/join", post(party::join))
                // LAN discovery: the SSDP LOCATION (pm-discovery); names the node, nothing more.
                .route(pm_discovery::DESCRIPTION_PATH, get(upnp_description))
                // Renderers fetch pull streams with the stream's own key (`?k=`), or the token.
                .route("/v1/outputs/pull/{name}/{file}", get(pull_stream))
                // Album covers for TV screens, with that cover's key.
                .route("/v1/art/{id}", get(keyed_cover))
                // MusicBrainz caching proxy for Pinakes and the owner's tools.
                .route("/mb/ws/2/{*path}", get(mbproxy::forward))
                .with_state(state.clone()),
        )
        .merge(subsonic::router(state.clone()))
        // The zones as UPnP/DLNA renderers (no token, like any renderer; `[renderer] enabled`),
        // to LAN peers only (peer.rs)
        .merge(
            state
                .renderer
                .get()
                .map(|r| r.router().layer(middleware::from_fn(peer::lan_only)))
                .unwrap_or_default(),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            peer::tokenless_direct_only,
        ))
}

/// Bind and serve until the task is cancelled.
pub async fn serve(state: Arc<NodeState>, listen: &str) -> anyhow::Result<()> {
    let listener = bind(listen).await?;
    serve_listener(state, listener).await
}

/// Bind the API's address. A port in use is the one fatal port: the error names it and the
/// likely cause, so a restart loop says what to fix.
pub async fn bind(listen: &str) -> anyhow::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => {
            tracing::info!(%listen, "API port bound; serving once the node has started");
            Ok(l)
        }
        Err(e) => Err(anyhow::anyhow!(bind_error(listen, &e))),
    }
}

/// What to say when the API cannot listen on `listen`.
pub fn bind_error(listen: &str, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::AddrInUse => format!(
            "cannot listen on {listen}: the port is already in use, most likely by another Pubky Music Node \
             on this machine (an older install, or a second container on the host network). Stop it, or set \
             [api] listen (PM_LISTEN) to a free port"
        ),
        std::io::ErrorKind::PermissionDenied => format!(
            "cannot listen on {listen}: permission denied (ports below 1024 need root); pick a port above 1024 \
             in [api] listen (PM_LISTEN)"
        ),
        _ => format!("cannot listen on {listen}: {e}"),
    }
}

/// Serve on an already bound listener (tests bind port 0).
pub async fn serve_listener(state: Arc<NodeState>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    // Peer addresses: the renderer answers LAN peers only (peer.rs)
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn auth(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
    mut req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    // The owner's API token, or a listener's personal one (accounts.rs)
    let Some(caller) = accounts::caller(&state, bearer(&headers, &query).as_deref()) else {
        return ApiError::new(StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
    };
    let refusal = match caller.role {
        accounts::Role::Owner => None,
        accounts::Role::Listener => accounts::listener_refusal(&state, req.method(), req.uri().path()),
        accounts::Role::Guest => party::guest_refusal(req.method(), req.uri().path()),
    };
    if let Some(why) = refusal {
        return accounts::refused(why).into_response();
    }
    // Who changed a zone last decides whether its plays are the owner's listens (listens.rs)
    if !matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD) {
        if let Some(zone) = listens::zone_of_path(req.uri().path()) {
            listens::zone_changed_by(&state, zone, caller.role == accounts::Role::Owner);
        }
    }
    // Listeners (and, to be safe, party guests) see file names, not where the owner keeps files
    let not_owner = caller.role != accounts::Role::Owner;
    req.extensions_mut().insert(caller);
    let resp = next.run(req).await;
    if not_owner {
        accounts::listener_response(resp).await
    } else {
        resp
    }
}

/// The request's bearer token (or `?token=`, for WebSocket upgrades).
pub(crate) fn bearer(
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| query.get("token").cloned())
}

/// The request carries the API token (or the node has none).
fn token_ok(
    state: &NodeState,
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
) -> bool {
    let Some(token) = state.token() else { return true };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        // Browsers cannot set headers on WebSocket upgrades: allow `?token=`.
        .or_else(|| query.get("token").cloned());
    bearer.as_deref() == Some(token.as_str())
}

// ---- web UI ----

/// The built-in browser controller: one static page over `/v1`. It asks for
/// the token itself, so the page is served without auth.
/// Top-level views of the web UI that have their own path.
const WEB_VIEWS: [&str; 12] = [
    "favourites",
    "albums",
    "artists",
    "genres",
    "tracks",
    "playlists",
    "radio",
    "sources",
    "queue",
    "zones",
    "settings",
    "identify",
];

/// `/ux/`: the UX wireframes in `docs/ux/` (steward brief 2026-10-20), read
/// from the source tree on debug builds only; release builds answer 404.
async fn ux_index() -> Response {
    ux_file(Path("index.html".to_string())).await
}

async fn ux_file(Path(path): Path<String>) -> Response {
    if !cfg!(debug_assertions) || path.contains("..") || path.starts_with('/') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/ux");
    let file = root.join(&path);
    let mime = match file.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("png") => "image/png",
        Some("md") => "text/markdown; charset=utf-8",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache")],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn web_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/app.css"),
    )
        .into_response()
}

async fn web_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/app.js"),
    )
        .into_response()
}

async fn web_manifest() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/manifest.webmanifest"),
    )
        .into_response()
}

async fn web_icon() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../static/icon.svg"),
    )
        .into_response()
}

/// `GET /party/{code}`: the page a party guest opens (no token; it joins with the code).
async fn party_page() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONTENT_SECURITY_POLICY, web::PAGE_CSP.as_str()),
        ],
        include_str!("../static/party.html"),
    )
        .into_response()
}

async fn web_ui() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONTENT_SECURITY_POLICY, web::PAGE_CSP.as_str()),
        ],
        include_str!("../static/index.html"),
    )
        .into_response()
}

// ---- handlers: node ----

async fn info(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<accounts::Caller>,
) -> ApiResult<Info> {
    let library = s.library.lock().expect("library poisoned").stats()?;
    let mut last_scan = s.last_scan.lock().expect("last scan lock").clone();
    // The unreadable files' paths are where the owner keeps music: a listener gets the count
    if c.role != accounts::Role::Owner {
        if let Some(l) = last_scan.as_mut() {
            l.errors.clear();
        }
    }
    Ok(Json(Info {
        id: s.node_id.clone(),
        remote: s.remote_id.read().expect("remote id poisoned").clone(),
        name: s.name.clone(),
        version: s.version.clone(),
        api: API_VERSION.into(),
        library,
        zones: s.zones.list().len(),
        outputs: s.outputs.len(),
        scanning: s.is_scanning(),
        last_scan,
    }))
}

async fn upnp_description(State(s): State<Arc<NodeState>>) -> Response {
    let xml = pm_discovery::description_xml(&s.announcement(0));
    ([(header::CONTENT_TYPE, "text/xml; charset=utf-8")], xml).into_response()
}

/// An output as `GET /v1/outputs` lists it.
#[derive(Debug, serde::Serialize)]
struct OutputView {
    #[serde(flatten)]
    info: OutputInfo,
    /// The speaker system that groups it with others of its kind itself (`sonos`, `heos`,
    /// `bluos`): a zone of only those is that system's own group, in sync (`POST /v1/zones`).
    #[serde(skip_serializing_if = "Option::is_none")]
    group_by: Option<&'static str>,
}

async fn outputs(State(s): State<Arc<NodeState>>) -> Json<Vec<OutputView>> {
    // One lock at a time
    fn ids<T>(m: &std::collections::HashMap<OutputId, T>) -> std::collections::HashSet<OutputId> {
        m.keys().cloned().collect()
    }
    let sonos = ids(&s.sonos.lock().expect("sonos lock"));
    let heos = ids(&s.heos.lock().expect("heos lock"));
    let bluos = ids(&s.bluos_players.lock().expect("bluos lock"));
    Json(
        s.outputs
            .list()
            .into_iter()
            .map(|info| {
                let group_by = if sonos.contains(&info.id) {
                    Some("sonos")
                } else if heos.contains(&info.id) {
                    Some("heos")
                } else if bluos.contains(&info.id) {
                    Some("bluos")
                } else {
                    None
                };
                OutputView { info, group_by }
            })
            .collect(),
    )
}

/// Create a pull output: a zone that plays to it streams WAV at
/// `/v1/outputs/pull/{name}/stream.wav` for any renderer or browser to fetch.
async fn create_pull(State(s): State<Arc<NodeState>>, Json(body): Json<CreatePull>) -> ApiResult<OutputInfo> {
    let name = body.name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::bad_request("name must be letters, digits, '-' or '_'"));
    }
    let out = Arc::new(pm_zones::outputs::PullOutput::new(name));
    if s.outputs.get(&out.id()).is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("pull output {name} exists"),
        ));
    }
    let info = out.info();
    s.pulls
        .lock()
        .expect("pulls lock")
        .insert(name.to_string(), out.clone());
    s.outputs.register(out.clone());
    s.create_zone(&format!("Stream {name}"), out);
    Ok(Json(info))
}

/// `GET /v1/outputs/pull/{name}/url`: the keyed URL to give a renderer or player by hand.
async fn pull_url(State(s): State<Arc<NodeState>>, Path(name): Path<String>) -> ApiResult<serde_json::Value> {
    if !s.pulls.lock().expect("pulls lock").contains_key(&name) {
        return Err(ApiError::not_found(format!("no pull output {name}")));
    }
    Ok(Json(serde_json::json!({ "url": s.pull_url(&name) })))
}

/// The live stream of a pull output. Waits up to 15 s for a zone to play into it; a listener that
/// leaves gives it back for the next one (see `pm_zones::outputs::pull`).
async fn pull_stream(
    State(s): State<Arc<NodeState>>,
    Path((name, file)): Path<(String, String)>,
    method: axum::http::Method,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let keyed = q.get("k").is_some_and(|k| *k == s.stream_key(&name));
    if !keyed && !token_ok(&s, &headers, &q) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing or wrong stream key",
        ));
    }
    // WAV for everyone; DLNA LPCM (raw big-endian 16-bit) for TVs that take nothing else.
    let kind = match file.as_str() {
        "stream.wav" => pm_out_upnp::dlna::StreamKind::Wav {
            mime: "audio/wav".into(),
        },
        // OpenHome renderers (Linn): the same WAV, with the RIFF size 0 their pipeline reads as
        // "continuous stream" (see `wav_header_continuous`).
        "live.wav" => pm_out_upnp::dlna::StreamKind::Wav {
            mime: "audio/wav".into(),
        },
        "stream.l16" => pm_out_upnp::dlna::StreamKind::L16,
        // Slimproto players: the PCM without a header (squeezelite would play the header).
        "stream.pcm" => pm_out_upnp::dlna::StreamKind::Wav {
            mime: "application/octet-stream".into(),
        },
        _ => return Err(ApiError::not_found(format!("no stream {file}"))),
    };
    let out = s
        .pulls
        .lock()
        .expect("pulls lock")
        .get(&name)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("no pull output {name}")))?;
    // DLNA devices check these on the answer (some ask with HEAD first).
    let dlna = [
        ("transferMode.dlna.org", "Streaming".to_string()),
        ("contentFeatures.dlna.org", kind.content_features()),
    ];
    let with_headers = |mime: String, body: axum::body::Body| {
        let mut r = (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "no-store".to_string()),
                (header::ACCEPT_RANGES, "none".to_string()),
                // The body ends when the connection closes: say so, so a keep-alive client
                // (and hyper) treat this as the last answer on the connection.
                (header::CONNECTION, "close".to_string()),
            ],
            body,
        )
            .into_response();
        for (k, v) in &dlna {
            if let Ok(v) = axum::http::HeaderValue::from_str(v) {
                r.headers_mut().insert(*k, v);
            }
        }
        // An HTTP/1.0 answer: the body runs until the connection closes. HTTP/1.1 would send a
        // live stream chunked, and a Linn DS stops at once on a chunked stream (seen on the
        // owner's Klimax: it plays the same stream unchunked).
        *r.version_mut() = axum::http::Version::HTTP_10;
        r
    };
    // A HEAD must not take the live stream: the GET that follows needs it.
    if method == axum::http::Method::HEAD {
        let mime = match &kind {
            pm_out_upnp::dlna::StreamKind::L16 => "audio/L16".to_string(),
            k => k.mime(0, 0),
        };
        return Ok(with_headers(mime, axum::body::Body::empty()));
    }
    let Some(live) = out.take_stream(pm_zones::outputs::LISTENER_TIMEOUT).await else {
        return Err(ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "no zone is playing into this stream; start playback to the Stream zone first",
        ));
    };
    let fmt = live.format;
    if kind == pm_out_upnp::dlna::StreamKind::L16 && fmt.bits != 16 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("LPCM is 16-bit; the zone plays {fmt}"),
        ));
    }
    let mime = kind.mime(fmt.rate_hz, fmt.channels);
    // Each listener gets its own header: one that leaves and comes back (OpenHome renderers do,
    // after reading the start) decodes the stream from the top again.
    let header = match file.as_str() {
        "stream.wav" => Some(pm_zones::outputs::wav_header(fmt)),
        "live.wav" => Some(pm_zones::outputs::wav_header_continuous(fmt)),
        _ => None,
    };
    let to_l16 = kind == pm_out_upnp::dlna::StreamKind::L16;
    // The live stream moves into the body; when the listener goes, it is given back
    let chunks = futures_util::stream::unfold(live, |mut live| async move {
        live.next_chunk().await.map(|c| (c, live))
    });
    let mut l16 = L16::default();
    let body = futures_util::stream::iter(header)
        .chain(chunks.map(move |c| if to_l16 { l16.convert(&c) } else { c }))
        .map(Ok::<_, std::convert::Infallible>);
    Ok(with_headers(mime, axum::body::Body::from_stream(body)))
}

/// Little-endian 16-bit PCM to DLNA LPCM (big-endian), across chunk boundaries.
#[derive(Default)]
struct L16 {
    /// A sample's first byte when a chunk ended between its two bytes.
    carry: Option<u8>,
}

impl L16 {
    fn convert(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut data = chunk;
        let mut out = Vec::with_capacity(data.len() + 1);
        if let Some(lo) = self.carry.take() {
            match data.split_first() {
                Some((hi, rest)) => {
                    out.extend_from_slice(&[*hi, lo]);
                    data = rest;
                }
                None => {
                    self.carry = Some(lo);
                    return out;
                }
            }
        }
        let (pairs, rest) = data.as_chunks::<2>();
        if let [lo] = rest {
            self.carry = Some(*lo);
        }
        for [lo, hi] in pairs {
            out.extend_from_slice(&[*hi, *lo]);
        }
        out
    }
}

/// Discover outputs again (local devices, endpoints over mDNS and configured
/// addresses); new ones get a zone. Returns the full list.
/// `GET /v1/outputs/discovery`: scan the network and explain what was found,
/// for setups where a renderer does not show up (Docker bridge, VLANs).
async fn discovery_report(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<DiscoveryReport> {
    let st = s.clone();
    let quick = q.get("quick").is_some_and(|v| v == "1");
    let (upnp, cast) = tokio::task::spawn_blocking(move || {
        if quick {
            return (Vec::new(), Vec::new());
        }
        let upnp = if st.upnp {
            pm_out_upnp::ssdp_probe(std::time::Duration::from_secs(3)).unwrap_or_default()
        } else {
            Vec::new()
        };
        let cast = if st.cast {
            pm_out_cast::discover(std::time::Duration::from_millis(1500))
        } else {
            Vec::new()
        };
        (upnp, cast)
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let outputs = s.outputs.list();
    let have = |loc: &str| {
        outputs
            .iter()
            .any(|o| o.id.0.contains(loc) || o.name.contains(loc))
    };
    let local_address = local_address();
    let mut notes = Vec::new();
    if !s.upnp && !s.cast {
        notes.push("UPnP and Cast discovery are both off in the node config.".into());
    }
    if quick {
        // No scan ran; only the configuration notes below apply.
    } else if s.upnp && upnp.is_empty() {
        notes.push("No UPnP device answered at all: multicast is not reaching this node. In Docker, run the container on the host network; on a VLAN, SSDP must be relayed.".into());
    } else if s.upnp && !upnp.iter().any(|a| a.renderer) {
        notes.push("Devices answered, but none describes itself as a MediaRenderer with AVTransport. OpenHome-only players (some Linn models) need the UPnP AV service enabled, or are reached through another renderer front-end.".into());
    }
    if s.public_url.contains("0.0.0.0")
        || s.public_url.contains("127.0.0.1")
        || s.public_url.contains("localhost")
    {
        notes.push(format!(
            "public_url is {}, which renderers cannot reach; set [api] public_url (PM_PUBLIC_URL) to http://{}:7788.",
            s.public_url,
            local_address.as_deref().unwrap_or("<node ip>")
        ));
    }
    if s.public_url_guessed() {
        if let Some(why) = doubtful_guess(&s.public_url) {
            notes.push(format!(
                "public_url is not set, so the node picked {}, which {why}. If speakers stay silent, set [api] public_url (PM_PUBLIC_URL) to the address they reach.",
                s.public_url
            ));
        }
    }
    Ok(Json(DiscoveryReport {
        upnp: upnp
            .into_iter()
            .map(|a| DiscoveredUpnp {
                output: have(&a.location),
                from: a.from,
                location: a.location,
                server: a.server,
                friendly_name: a.friendly_name,
                renderer: a.renderer,
            })
            .collect(),
        cast: cast
            .into_iter()
            .map(|d| DiscoveredCast {
                id: d.id,
                friendly_name: d.friendly_name,
                addr: d.addr.to_string(),
            })
            .collect(),
        public_url: s.public_url.clone(),
        local_address,
        notes,
        upnp_enabled: s.upnp,
        cast_enabled: s.cast,
        saved_addresses: s.upnp_addresses(),
        saved_endpoints: s.endpoint_addresses(),
        node: s.name.clone(),
    }))
}

/// Why an address the node picked for itself may not be the one speakers reach: a VPN's or
/// another non-home address, or Docker's default bridge (the container is not on the host
/// network). `None` when it looks like a home network address.
fn doubtful_guess(public_url: &str) -> Option<&'static str> {
    let host = url::Url::parse(public_url).ok()?.host_str()?.to_string();
    let std::net::IpAddr::V4(ip) = host.parse().ok()? else {
        return None;
    };
    if ip.octets()[..2] == [172, 17] {
        Some("looks like Docker's bridge network (run the container on the host network)")
    } else if !ip.is_private() && !ip.is_loopback() && !ip.is_unspecified() {
        Some("is not a home network address (a VPN?)")
    } else {
        None
    }
}

/// The address this node talks to the network from.
fn local_address() -> Option<String> {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|u| u.connect("1.1.1.1:80").map(|_| u))
        .and_then(|u| u.local_addr())
        .ok()
        .map(|a| a.ip().to_string())
}

/// `POST /v1/outputs/upnp {address}`: add a renderer multicast cannot find
/// (another subnet or VLAN); the address is kept for the next start.
async fn add_upnp_output(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<AddRenderer>,
) -> ApiResult<Vec<OutputInfo>> {
    let st = s.clone();
    let found = tokio::task::spawn_blocking(move || st.add_upnp_address(&body.address))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| {
            // Say which node tried: people run several (a cloud workspace has no
            // route to a renderer at home).
            let from = local_address().unwrap_or_else(|| "this node".into());
            ApiError::bad_request(format!("{e} (asked from {} at {from})", s.name))
        })?;
    let names: Vec<&str> = found.iter().map(|r| r.friendly_name.as_str()).collect();
    tracing::info!(?names, "renderers added by address");
    Ok(Json(s.outputs.list()))
}

/// `DELETE /v1/outputs/upnp/{address}`: stop re-adding it at start.
async fn remove_upnp_address(
    State(s): State<Arc<NodeState>>,
    Path(address): Path<String>,
) -> ApiResult<Vec<String>> {
    if !s.remove_upnp_address(&address) {
        return Err(ApiError::not_found(format!("{address} is not a saved address")));
    }
    Ok(Json(s.upnp_addresses()))
}

/// `POST /v1/outputs/endpoints {address, key}`: the desktop app registers its player at
/// `host:port` with the key it made; the outputs become zones, and the address and key are kept
/// for the next start (a new key for the same address replaces the old).
async fn add_endpoint(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<AddEndpoint>,
) -> ApiResult<Vec<OutputInfo>> {
    let key = pm_stream::Key::parse(body.key.expose()).map_err(|_| {
        ApiError::bad_request(if body.key.is_empty() {
            "the player's key is missing: update the Pubky Music app".to_string()
        } else {
            "the player's key is not 32 to 256 visible ASCII characters".to_string()
        })
    })?;
    let st = s.clone();
    let address = body.address.clone();
    let outs = tokio::task::spawn_blocking(move || st.add_endpoint_address(&address, Some(&key)))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| {
            let from = local_address().unwrap_or_else(|| "this node".into());
            ApiError::bad_request(format!(
                "cannot reach the endpoint at {}: {e} (asked from {} at {from})",
                body.address, s.name
            ))
        })?;
    tracing::info!(address = %body.address, outputs = outs.len(), "endpoint added by address");
    Ok(Json(outs))
}

/// `DELETE /v1/outputs/endpoints/{address}`: forget it; its zones go too.
async fn remove_endpoint(
    State(s): State<Arc<NodeState>>,
    Path(address): Path<String>,
) -> ApiResult<Vec<String>> {
    if !s.remove_endpoint_address(&address) {
        return Err(ApiError::not_found(format!("{address} is not a saved endpoint")));
    }
    Ok(Json(s.endpoint_addresses()))
}

async fn rescan_outputs(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<OutputInfo>> {
    let st = s.clone();
    let new = tokio::task::spawn_blocking(move || st.rescan_outputs())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !new.is_empty() {
        tracing::info!(?new, "new outputs");
    }
    Ok(Json(s.outputs.list()))
}

// ---- handlers: zones ----

fn zone_of(s: &NodeState, id: &str) -> Result<Arc<Zone>, ApiError> {
    s.zones
        .get(&ZoneId::new(id))
        .ok_or_else(|| ApiError::not_found(format!("no zone {id}")))
}

async fn zones(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<accounts::Caller>,
) -> Json<Vec<ZoneState>> {
    // A listener sees only the zones open to listeners
    Json(events::visible(&s, &c, s.zones.list()))
}

async fn create_zone(State(s): State<Arc<NodeState>>, Json(body): Json<CreateZone>) -> ApiResult<ZoneState> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("zone name is empty"));
    }
    let mut ids: Vec<String> = body.outputs.clone();
    if let Some(o) = body.output.clone() {
        if !ids.contains(&o) {
            ids.insert(0, o);
        }
    }
    if ids.is_empty() {
        return Err(ApiError::bad_request("give `output` or `outputs`"));
    }
    let mut members = Vec::with_capacity(ids.len());
    for id in &ids {
        let out = s
            .outputs
            .get(&OutputId(id.clone()))
            .ok_or_else(|| ApiError::not_found(format!("no output {id}")))?;
        members.push(out);
    }
    // Sonos players only: Sonos groups them itself, in tight sync (pm_out_upnp::sonos).
    let sonos: Vec<Arc<pm_out_upnp::UpnpOutput>> = {
        let map = s.sonos.lock().expect("sonos lock");
        ids.iter()
            .filter_map(|id| map.get(&OutputId(id.clone())).cloned())
            .collect()
    };
    // HEOS players only (Denon, Marantz) whose CLI knows them all: HEOS groups them itself
    // (pm_out_upnp::heos).
    let mut heos: Vec<Arc<pm_out_upnp::UpnpOutput>> = {
        let map = s.heos.lock().expect("heos lock");
        ids.iter()
            .filter_map(|id| map.get(&OutputId(id.clone())).cloned())
            .collect()
    };
    // BluOS players only (Bluesound, NAD): BluOS groups them itself (pm_out_bluos)
    let bluos: Vec<Arc<pm_out_bluos::BluosOutput>> = {
        let map = s.bluos_players.lock().expect("bluos lock");
        ids.iter()
            .filter_map(|id| map.get(&OutputId(id.clone())).cloned())
            .collect()
    };
    if members.len() > 1 && heos.len() == ids.len() {
        let renderers: Vec<pm_out_upnp::Renderer> = heos.iter().map(|o| o.renderer().clone()).collect();
        let ok = tokio::task::spawn_blocking(move || {
            pm_out_upnp::heos::can_group(&renderers, pm_out_upnp::heos::CLI_PORT)
        })
        .await
        .unwrap_or(false);
        if !ok {
            heos.clear();
        }
    }
    let output: Arc<dyn pm_zones::Output> = if members.len() == 1 {
        members.remove(0)
    } else if sonos.len() == ids.len() {
        let mut sonos = sonos;
        let lead = sonos.remove(0);
        Arc::new(pm_out_upnp::sonos::SonosGroup::new(body.name.trim(), lead, sonos))
    } else if bluos.len() == ids.len() {
        let mut bluos = bluos;
        let lead = bluos.remove(0);
        Arc::new(pm_out_bluos::BluosGroup::new(body.name.trim(), lead, bluos))
    } else if heos.len() == ids.len() {
        let mut heos = heos;
        let lead = heos.remove(0);
        Arc::new(pm_out_upnp::heos::HeosGroup::new(body.name.trim(), lead, heos))
    } else {
        Arc::new(
            pm_zones::outputs::GroupOutput::new(body.name.trim(), members)
                .map_err(|e| ApiError::bad_request(e.to_string()))?,
        )
    };
    let zone = s.create_zone(body.name.trim(), output);
    Ok(Json(zone.state()))
}

async fn zone(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<ZoneState> {
    Ok(Json(zone_of(&s, &id)?.state()))
}

async fn delete_zone(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.stop();
    s.remove_zone(zone.id());
    Ok(StatusCode::NO_CONTENT)
}

async fn queue(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<Vec<QueueItem>> {
    Ok(Json(zone_of(&s, &id)?.queue()))
}

async fn enqueue(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<Enqueue>,
) -> ApiResult<Enqueued> {
    let zone = zone_of(&s, &id)?;
    let mut items = Vec::with_capacity(body.track_ids.len() + body.items.len());
    {
        let lib = s.library.lock().expect("library poisoned");
        for tid in &body.track_ids {
            let track = lib
                .get_track(TrackId(*tid))?
                .ok_or_else(|| ApiError::not_found(format!("no track {tid}")))?;
            if let Some(item) = NodeSources::item_for(&track) {
                items.push(item);
            }
        }
    }
    for it in body.items {
        if it.uri.trim().is_empty() {
            return Err(ApiError::bad_request("item uri is empty"));
        }
        items.push(it);
    }
    if items.is_empty() {
        return Err(ApiError::bad_request("nothing to enqueue"));
    }
    if body.replace {
        zone.stop();
        zone.clear();
    }
    let at = match body.at {
        QueueAt::End => Insert::End,
        QueueAt::Next => Insert::Next,
    };
    let ids = zone.enqueue(items, at);
    if body.play {
        if body.replace {
            zone.play_index(0);
        } else {
            zone.play();
        }
    }
    settle(&zone).await;
    Ok(Json(Enqueued { ids }))
}

async fn clear_queue(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.stop();
    zone.clear();
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Wait (bounded) until the engine has applied every command sent so far,
/// so that a read right after a write sees the new state.
async fn settle(zone: &Zone) {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), zone.wait_acked()).await;
}

async fn remove_item(
    State(s): State<Arc<NodeState>>,
    Path((id, item)): Path<(String, u64)>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    if zone.remove(item) {
        settle(&zone).await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no queue item {item}")))
    }
}

async fn move_item(
    State(s): State<Arc<NodeState>>,
    Path((id, item)): Path<(String, u64)>,
    Json(body): Json<MoveItem>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    if zone.move_item(item, body.to_index) {
        settle(&zone).await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no queue item {item}")))
    }
}

async fn transport(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<TransportAction>,
) -> ApiResult<ZoneState> {
    let zone = zone_of(&s, &id)?;
    match body {
        TransportAction::Play { index: Some(i) } => zone.play_index(i),
        TransportAction::Play { index: None } => zone.play(),
        TransportAction::Pause => zone.pause(),
        TransportAction::Toggle => zone.toggle_pause(),
        TransportAction::Stop => zone.stop(),
        TransportAction::Next => zone.next(),
        TransportAction::Previous => zone.previous(),
        TransportAction::Seek { position_ms } => zone.seek_ms(position_ms),
    }
    // Reply with the state after the engine applied the command (bounded wait).
    Ok(Json(
        tokio::time::timeout(std::time::Duration::from_secs(2), zone.wait_acked())
            .await
            .unwrap_or_else(|_| zone.state()),
    ))
}

async fn volume(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetVolume>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    // A zone's volume limit (DSP settings) caps absolute requests.
    let limit = s.sources.dsp_for(&zone.output().id()).max_volume_db;
    zone.set_volume(cap_volume(body.volume, limit)).await?;
    announce::note_volume(zone.id(), &body.volume);
    Ok(StatusCode::NO_CONTENT)
}

/// Apply a zone's volume limit to an absolute dB request.
fn cap_volume(v: pm_model::Volume, limit: Option<f32>) -> pm_model::Volume {
    match (v, limit) {
        (pm_model::Volume::Db(db), Some(max)) if db > max => pm_model::Volume::Db(max),
        (v, _) => v,
    }
}

async fn repeat(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetRepeat>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.set_repeat(body.repeat);
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Library radio: keep the zone playing with similar music after the last item.
async fn radio(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetRadio>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.set_radio(body.on);
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Deserialize)]
struct SetCrossfade {
    /// Seconds, 0 (off) to 12.
    seconds: f64,
}

/// `PUT /v1/zones/{id}/crossfade {seconds}`: fade each track into the next (0 = off). Songs of
/// the same album following each other stay gapless.
async fn crossfade(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetCrossfade>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    if !(0.0..=12.0).contains(&body.seconds) {
        return Err(ApiError::bad_request("crossfade is 0 to 12 seconds"));
    }
    let ms = (body.seconds * 1000.0).round() as u64;
    let key = state::crossfade_key(zone.id());
    {
        let lib = s.library.lock().expect("library poisoned");
        if ms == 0 {
            lib.delete_setting(&key)?;
        } else {
            lib.set_setting(&key, &ms.to_string())?;
        }
    }
    zone.set_crossfade_ms(ms);
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Shuffle the upcoming part of the queue; the playing item is untouched.
async fn shuffle_queue(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.shuffle_upcoming();
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

fn sleep_view(s: &NodeState, zone: &ZoneId) -> SleepView {
    let ends_at_ms = s.sleep_ends_at(zone);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    SleepView {
        ends_at_ms,
        remaining_ms: ends_at_ms.map(|e| e.saturating_sub(now)),
    }
}

/// The zone's sleep timer, if any.
async fn sleep_get(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<SleepView> {
    let zone = zone_of(&s, &id)?;
    Ok(Json(sleep_view(&s, zone.id())))
}

/// Pause the zone after `seconds` (`null` cancels any timer).
async fn sleep_set(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetSleep>,
) -> ApiResult<SleepView> {
    let zone = zone_of(&s, &id)?;
    if body.seconds == Some(0) {
        return Err(ApiError::bad_request(
            "seconds must be positive, or null to cancel",
        ));
    }
    s.set_sleep(zone.clone(), body.seconds);
    Ok(Json(sleep_view(&s, zone.id())))
}

/// Move the queue and playback position to another zone. The target stops
/// whatever it was doing; the source is left stopped and empty.
async fn transfer(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<accounts::Caller>,
    Path(id): Path<String>,
    Json(body): Json<TransferRequest>,
) -> ApiResult<ZoneState> {
    // The middleware checked the zone in the path; the target needs the same right
    if !accounts::may_use_zone(&s, &c, &body.to) {
        return Err(accounts::refused(
            "the owner has not opened that zone to listeners",
        ));
    }
    if body.to == id {
        return Err(ApiError::bad_request("cannot transfer a zone to itself"));
    }
    let from = zone_of(&s, &id)?;
    let to = zone_of(&s, &body.to)?;
    let st = from.state();
    if st.queue.is_empty() {
        return Err(ApiError::bad_request("nothing to transfer: the queue is empty"));
    }
    let index = st.now_playing.as_ref().map(|n| n.index).or(st.queue_index);
    let position_ms = st.now_playing.as_ref().map(|n| n.position_ms).unwrap_or(0);
    from.stop();
    from.clear();
    let moved = load_zone(
        &to,
        st.queue,
        index,
        position_ms,
        st.transport,
        st.repeat,
        st.radio,
    )
    .await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), from.wait_acked()).await;
    Ok(Json(moved))
}

/// Fills `to` the way a move leaves it: stopped and emptied, then `queue`, `repeat` and `radio`;
/// `index` plays from `position_ms` (paused when the source was paused; nothing starts when it
/// was stopped). Seeking needs the track open, so it waits for that (bounded) first.
async fn load_zone(
    to: &Zone,
    queue: Vec<QueueItem>,
    index: Option<usize>,
    position_ms: u64,
    transport: pm_zones::Transport,
    repeat: pm_zones::Repeat,
    radio: bool,
) -> ZoneState {
    to.stop();
    to.clear();
    to.set_repeat(repeat);
    to.set_radio(radio);
    let mut events = to.subscribe();
    let len = queue.len();
    to.enqueue(queue, Insert::End);
    let index = index.filter(|i| *i < len);
    if let (Some(i), true) = (index, transport != pm_zones::Transport::Stopped) {
        to.play_index(i);
        let started = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Ok(pm_zones::ZoneEvent::TrackStarted { .. }) => break true,
                    Ok(pm_zones::ZoneEvent::Error { .. }) | Err(_) => break false,
                    Ok(_) => {}
                }
            }
        })
        .await
        .unwrap_or(false);
        if started {
            if position_ms > 1_000 {
                to.seek_ms(position_ms);
            }
            if transport == pm_zones::Transport::Paused {
                to.pause();
            }
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), to.wait_acked())
        .await
        .unwrap_or_else(|_| to.state())
}

/// `POST /v1/zones/{id}/load`: a player the node does not run (This browser) moves its queue
/// here. The zone stops and empties, takes the items, repeat and radio, and plays `index` from
/// `position_ms`; paused stays paused. A listener needs the zone open to listeners (the
/// middleware checks the path).
async fn load(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<LoadRequest>,
) -> ApiResult<ZoneState> {
    if body.items.is_empty() {
        return Err(ApiError::bad_request("nothing to move: the queue is empty"));
    }
    if body.items.iter().any(|it| it.uri.trim().is_empty()) {
        return Err(ApiError::bad_request("item uri is empty"));
    }
    let to = zone_of(&s, &id)?;
    Ok(Json(
        load_zone(
            &to,
            body.items,
            body.index,
            body.position_ms,
            body.transport,
            body.repeat,
            body.radio,
        )
        .await,
    ))
}

// ---- handlers: DSP per zone ----

/// The pipeline settings for the zone's output (EQ, headroom, software
/// volume, resampling policy). Changes apply at the next track start.
async fn dsp_get(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<pm_audio::PipelineConfig> {
    let zone = zone_of(&s, &id)?;
    Ok(Json(s.sources.dsp_for(&zone.output().id())))
}

async fn dsp_put(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(cfg): Json<pm_audio::PipelineConfig>,
) -> ApiResult<pm_audio::PipelineConfig> {
    let zone = zone_of(&s, &id)?;
    for b in &cfg.eq {
        // Validate against a common rate; the real plan validates again per track.
        b.validate(48_000).map_err(ApiError::bad_request)?;
    }
    if cfg.headroom_db.is_some_and(|d| !(-30.0..=0.0).contains(&d)) {
        return Err(ApiError::bad_request("headroom_db must be between -30 and 0"));
    }
    if cfg.volume_db.is_some_and(|d| !(-90.0..=0.0).contains(&d)) {
        return Err(ApiError::bad_request("volume_db must be between -90 and 0"));
    }
    if cfg.max_volume_db.is_some_and(|d| !(-90.0..=0.0).contains(&d)) {
        return Err(ApiError::bad_request("max_volume_db must be between -90 and 0"));
    }
    let mut cfg = cfg;
    if let (Some(v), Some(max)) = (cfg.volume_db, cfg.max_volume_db) {
        if v > max {
            cfg.volume_db = Some(max);
        }
    }
    s.sources.set_dsp(&zone.output().id(), Some(cfg.clone()))?;
    Ok(Json(cfg))
}

async fn dsp_reset(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    s.sources.set_dsp(&zone.output().id(), None)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- handlers: library ----

async fn library_stats(State(s): State<Arc<NodeState>>) -> ApiResult<pm_library::LibraryStats> {
    Ok(Json(s.library.lock().expect("library poisoned").stats()?))
}

async fn tracks(State(s): State<Arc<NodeState>>, Query(p): Query<Page>) -> ApiResult<Tracks> {
    let limit = p.limit.unwrap_or(100).min(1000);
    let tracks = s
        .library
        .lock()
        .expect("library poisoned")
        .list_tracks(p.after.map(TrackId), limit)?;
    Ok(Json(Tracks { tracks }))
}

async fn track(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<pm_model::Track> {
    let t = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    Ok(Json(t))
}

/// Add Pinakes's links after the direct lookups, skipping pages already
/// listed (same URL) and services the node already answered for.
fn merge_versions(out: &mut Vec<Version>, extra: Vec<Version>) {
    for v in extra {
        let dup = out.iter().any(|o| {
            o.url.trim_end_matches('/') == v.url.trim_end_matches('/')
                || (o.source == v.source && matches!(o.source.as_str(), "tidal" | "deezer" | "spotify"))
        });
        if !dup {
            out.push(v);
        }
    }
}

/// Where else a library track streams from (TIDAL and Deezer by ISRC, then
/// every service Pinakes lists for it); empty when nothing is known.
pub(crate) async fn track_versions(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
) -> ApiResult<Vec<Version>> {
    let track = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    // The canonical identity first (tags, fingerprint or the owner's
    // confirmation), then the raw tag ids.
    let canonical = track.canonical.as_ref().map(|(u, _)| u.clone());
    let hub = canonical
        .as_ref()
        .filter(|u| {
            matches!(
                u,
                pm_model::MusicUri::Recording { .. } | pm_model::MusicUri::Isrc { .. }
            )
        })
        .map(|u| u.to_url());
    let pinakes_key = match &canonical {
        Some(pm_model::MusicUri::Recording { mbid }) => Some(format!("mbid:{mbid}")),
        Some(pm_model::MusicUri::Isrc { isrc }) => Some(format!("isrc:{isrc}")),
        _ => None,
    }
    .or_else(|| track.recording_mbid.as_ref().map(|m| format!("mbid:{m}")))
    .or_else(|| track.isrc.as_ref().map(|i| format!("isrc:{i}")));
    let key = format!("pinakes.version.track.{id}");
    let (t, d, sp, p) = tokio::join!(
        tidal::track_versions(&s, &track),
        deezer::track_versions(&s, &track),
        spotify::track_versions(&s, &track),
        async {
            // The MusicBrainz URL's own card (a hub), then everything tagged with the key.
            let mut v = match &hub {
                Some(h) => discover::hub_versions(&s, h).await,
                None => Vec::new(),
            };
            if let Some(k) = pinakes_key {
                let by_key = discover::also_on_versions(&s, &k, &key).await;
                merge_versions(&mut v, by_key);
            }
            v
        }
    );
    let mut out = t.unwrap_or_default();
    out.extend(d.unwrap_or_default());
    out.extend(sp.unwrap_or_default());
    merge_versions(&mut out, p);
    Ok(Json(out))
}

/// Where else a library album streams from (TIDAL and Deezer by UPC).
pub(crate) async fn album_versions(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
) -> ApiResult<Vec<Version>> {
    let album = s
        .library
        .lock()
        .expect("library poisoned")
        .get_album(id)?
        .ok_or_else(|| ApiError::not_found(format!("no album {id}")))?;
    let pinakes_key = album
        .release_mbid
        .as_ref()
        .or(album.release_group_mbid.as_ref())
        .map(|m| format!("mbid:{m}"))
        .or_else(|| album.upc.as_ref().map(|u| format!("upc:{u}")));
    let key = format!("pinakes.version.album.{id}");
    let (t, d, sp, p) = tokio::join!(
        tidal::album_versions(&s, &album),
        deezer::album_versions(&s, &album),
        spotify::album_versions(&s, &album),
        async {
            match pinakes_key {
                Some(k) => discover::also_on_versions(&s, &k, &key).await,
                None => Vec::new(),
            }
        }
    );
    let mut out = t.unwrap_or_default();
    out.extend(d.unwrap_or_default());
    out.extend(sp.unwrap_or_default());
    merge_versions(&mut out, p);
    Ok(Json(out))
}

/// Lyrics from the file's tags or a sidecar `.lrc`; 404 when the track has none.
async fn lyrics(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Lyrics> {
    let (found, track) = {
        let lib = s.library.lock().expect("library poisoned");
        (lib.lyrics(TrackId(id))?, lib.get_track(TrackId(id))?)
    };
    // None in the file or beside it: LRCLIB, by title, artist, album and length
    let found = match (found, track) {
        (Some(f), _) => Some(f),
        (None, Some(t)) => {
            let artist = t.artists.first().map(|a| a.name.clone()).unwrap_or_default();
            let album = t.album.as_ref().map(|a| a.title.clone());
            lrclib::find(&s, &t.title, &artist, album.as_deref(), t.duration_ms)
                .await
                .map(|(text, _)| (text, "lrclib".to_string()))
        }
        _ => None,
    };
    let Some((text, source)) = found else {
        return Err(ApiError::not_found(format!("no lyrics for track {id}")));
    };
    let (lines, synced) = pm_library::lyrics::parse(&text);
    Ok(Json(Lyrics {
        track_id: id,
        source,
        synced,
        text,
        lines,
    }))
}

/// The track's audio file as stored, for a controller that plays locally
/// (the web UI's browser zone). Served only to authenticated callers of this node; never a
/// public URL (constraint 2).
/// A whole body or the byte range a browser asked for (seeking, resuming),
/// with `Accept-Ranges` so the browser knows seeking is possible.
pub(crate) fn ranged_response(headers: &HeaderMap, bytes: Vec<u8>, mime: &str) -> Response {
    let total = bytes.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| {
            let (a, b) = v.split_once('-')?;
            let start: usize = a.parse().ok()?;
            let end: usize = if b.is_empty() {
                total.saturating_sub(1)
            } else {
                b.parse().ok()?
            };
            (start < total && start <= end).then_some((start, end.min(total - 1)))
        });
    let mut resp = match range {
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}")),
            ],
            axum::body::Body::from(bytes[start..=end].to_vec()),
        )
            .into_response(),
        None => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime.to_string())],
            axum::body::Body::from(bytes),
        )
            .into_response(),
    };
    resp.headers_mut().insert(
        header::ACCEPT_RANGES,
        axum::http::HeaderValue::from_static("bytes"),
    );
    resp
}

async fn track_file(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let t = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    let mime = match t.file.container.as_str() {
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "aiff" => "audio/aiff",
        "isomp4" | "mp4" | "m4a" => "audio/mp4",
        "dsf" => "audio/x-dsf",
        "dff" => "audio/x-dff",
        "mp3" => "audio/mpeg",
        "aac" => "audio/aac",
        "vorbis" => "audio/ogg",
        _ => "application/octet-stream",
    };
    let name = t
        .file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("track-{id}"));
    downloads::stream_file(&s, id, &t.file, &headers, mime, Some(("inline", &name))).await
}

async fn search(State(s): State<Arc<NodeState>>, Query(q): Query<SearchQuery>) -> ApiResult<SearchResults> {
    let limit = q.limit.unwrap_or(50).min(500);
    let needle = q.q.trim().to_lowercase();
    // Streaming services answer alongside the library (their part is bounded in time).
    let sources = if q.sources.unwrap_or(1) != 0 && !needle.is_empty() {
        streaming::unified_search(&s, q.q.trim(), 12).await
    } else {
        Vec::new()
    };
    let lib = s.library.lock().expect("library poisoned");
    let tracks = lib.search_tracks(&q.q, limit)?;
    let albums = if needle.is_empty() {
        Vec::new()
    } else {
        lib.search_albums(&q.q, 24)?
    };
    let contains = |name: &str| !needle.is_empty() && name.to_lowercase().contains(&needle);
    let artists = lib
        .list_artists()?
        .into_iter()
        .filter(|a| contains(&a.name))
        .take(24)
        .collect();
    let genres = lib
        .list_genres()?
        .into_iter()
        .filter(|g| contains(&g.name))
        .take(12)
        .collect();
    Ok(Json(SearchResults {
        tracks,
        albums,
        artists,
        genres,
        sources,
    }))
}

async fn artists(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<pm_library::ArtistSummary>> {
    let lib = s.library.lock().expect("library poisoned");
    let mut all = lib.list_artists()?;
    // Songs without any artist, as "Unknown artist" at the end (its page is `/artist/lib-0`)
    all.extend(lib.unknown_artist()?);
    Ok(Json(all))
}

async fn artist(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<ArtistDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let artist = lib
        .get_artist(id)?
        .ok_or_else(|| ApiError::not_found(format!("no artist {id}")))?;
    let albums = lib.artist_albums(id)?;
    Ok(Json(ArtistDetail { artist, albums }))
}

/// Biography, links and whether a portrait is cached (fetched on first request).
async fn artist_info_handler(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
    Query(q): Query<item::LangQuery>,
    headers: HeaderMap,
) -> ApiResult<ArtistInfo> {
    if s.library
        .lock()
        .expect("library poisoned")
        .get_artist(id)?
        .is_none()
    {
        return Err(ApiError::not_found(format!("no artist {id}")));
    }
    let lang = about::language(q.lang.as_deref(), &headers);
    Ok(Json(artist_info::artist_info(&s, id, &lang).await?))
}

async fn artist_image(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let path = artist_info::image_path(&s, id);
    if !path.is_file() {
        // The first request may have to fetch it.
        let info = artist_info::artist_info(&s, id, "en").await?;
        if !info.image {
            // Else the picture of the service artist the owner matched it to
            let matched = s
                .library
                .lock()
                .expect("library poisoned")
                .get_setting(&format!("artist.picture.{id}"))
                .ok()
                .flatten();
            let got = match matched {
                Some(url) => match s.http_client().get(&url).send().await {
                    Ok(r) if r.status().is_success() => r.bytes().await.ok(),
                    _ => None,
                },
                None => None,
            };
            let Some(bytes) = got.filter(|b| b.len() > 100) else {
                return Err(ApiError::not_found(format!("no portrait for artist {id}")));
            };
            let _ = tokio::fs::create_dir_all(&s.art_dir).await;
            tokio::fs::write(&path, &bytes)
                .await
                .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ApiError::not_found(format!("portrait for artist {id}: {e}")))?;
    let mime = match bytes.get(..4) {
        Some([0x89, b'P', b'N', b'G']) => "image/png",
        Some([b'R', b'I', b'F', b'F']) => "image/webp",
        Some([b'G', b'I', b'F', _]) => "image/gif",
        _ => "image/jpeg",
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response())
}

async fn artist_tracks(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Tracks> {
    let tracks = s.library.lock().expect("library poisoned").artist_tracks(id)?;
    Ok(Json(Tracks { tracks }))
}

async fn albums(State(s): State<Arc<NodeState>>, Query(p): Query<Page>) -> ApiResult<Vec<pm_model::Album>> {
    let limit = p.limit.unwrap_or(100).min(1000);
    Ok(Json(
        s.library
            .lock()
            .expect("library poisoned")
            .list_albums_focused(p.focus, p.after, limit)?,
    ))
}

/// Release groups MusicBrainz lists for the library's artists from the last
/// year; a background refresh starts when any artist's list is stale.
async fn new_releases(State(s): State<Arc<NodeState>>) -> ApiResult<NewReleasesPage> {
    Ok(Json(new_releases::page(&s)?))
}

/// Force a refresh of stale artists now (one MusicBrainz request per second).
async fn new_releases_refresh(State(s): State<Arc<NodeState>>) -> ApiResult<NewReleasesPage> {
    if s.contact().is_empty() {
        return Err(ApiError::bad_request(
            "set a contact first (Settings): MusicBrainz requires one in the User-Agent",
        ));
    }
    new_releases::kick_refresh(s.clone());
    Ok(Json(new_releases::page(&s)?))
}

/// The best file format per album, for badges and Focus in clients.
async fn album_formats(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<pm_library::AlbumFormat>> {
    Ok(Json(s.library.lock().expect("library poisoned").album_formats()?))
}

async fn genres(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<pm_library::GenreSummary>> {
    Ok(Json(s.library.lock().expect("library poisoned").list_genres()?))
}

async fn genre(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<GenreDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let genre = lib
        .get_genre(id)?
        .ok_or_else(|| ApiError::not_found(format!("no genre {id}")))?;
    Ok(Json(GenreDetail {
        albums: lib.genre_albums(id)?,
        tracks: lib.genre_tracks(id, 500)?,
        genre,
    }))
}

async fn album_tracks(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Tracks> {
    let tracks = s.library.lock().expect("library poisoned").album_tracks(id)?;
    Ok(Json(Tracks { tracks }))
}

async fn history(State(s): State<Arc<NodeState>>, Query(p): Query<Limit>) -> ApiResult<History> {
    let limit = p.limit.unwrap_or(100).min(1000);
    let plays = s.library.lock().expect("library poisoned").history(limit)?;
    Ok(Json(History { plays }))
}

/// Clients that play by themselves (the native app's local zone, a browser)
/// report their plays here; node zones are recorded automatically.
async fn record_play(State(s): State<Arc<NodeState>>, Json(req): Json<RecordPlay>) -> ApiResult<History> {
    let lib = s.library.lock().expect("library poisoned");
    if lib.get_track(TrackId(req.track_id))?.is_none() {
        return Err(ApiError::not_found(format!("track {}", req.track_id)));
    }
    match req.played_at {
        Some(at) => {
            lib.record_play_at(TrackId(req.track_id), req.zone.as_deref(), at)?;
        }
        None => {
            lib.record_play(TrackId(req.track_id), req.zone.as_deref())?;
        }
    }
    let plays = lib.history(1)?;
    Ok(Json(History { plays }))
}

async fn home(State(s): State<Arc<NodeState>>, Query(p): Query<Limit>) -> ApiResult<HomePage> {
    let limit = p.limit.unwrap_or(12).min(100);
    let lib = s.library.lock().expect("library poisoned");
    let favs = bookmarks::collect(&lib)?;
    let bookmarked_albums: Vec<_> = favs.albums.into_iter().take(limit as usize).collect();
    // Forgotten bookmarks: bookmarked, then not played for two months
    let cutoff = pm_library::db::now_unix() - 60 * 86_400;
    let mut forgotten: Vec<(i64, pm_model::Track)> = favs
        .tracks
        .into_iter()
        .filter_map(|t| {
            let last =
                t.id.and_then(|id| lib.last_played(id).ok().flatten())
                    .unwrap_or(0);
            (last < cutoff).then_some((last, t))
        })
        .collect();
    forgotten.sort_by_key(|(last, _)| *last);
    let forgotten = forgotten
        .into_iter()
        .map(|(_, t)| t)
        .take(limit as usize)
        .collect();
    Ok(Json(HomePage {
        forgotten,
        favourite_albums: bookmarked_albums.clone(),
        bookmarked_albums,
        recently_added: lib.recent_albums(0, limit)?,
        recently_played: lib.recently_played_albums(limit)?,
        most_played_albums: lib.most_played_albums(limit)?,
        most_played: lib.most_played_tracks(limit)?,
        history: lib.history(limit)?,
        stats: lib.stats()?,
    }))
}

async fn folders(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<(String, bool)>> {
    Ok(Json(folder_list(&s).await?))
}

fn settings_view(s: &NodeState) -> NodeSettingsView {
    let copy_rate = files::copy_rate(s);
    // One lock, taken and released here: a guard in a struct literal lives until the end of the
    // whole expression, so a second `lock()` in another field would wait for ever
    let (jukebox_zone, dev_setting, genre_tags, prefer_full, first_steps_hidden) = {
        let lib = s.library.lock().expect("library poisoned");
        let on = |k: &str| lib.get_setting(k).ok().flatten().is_some_and(|v| v == "1");
        (
            lib.get_setting(subsonic::JUKEBOX_ZONE_KEY)
                .ok()
                .flatten()
                .filter(|z| !z.is_empty()),
            on(DEV_OUTPUTS_KEY),
            tags::genre_tags_on(&lib),
            on(item::PREFER_FULL_KEY),
            on(first_steps::HIDDEN_KEY),
        )
    };
    NodeSettingsView {
        contact: s.contact(),
        discovery: s.discovery().is_some(),
        jukebox_zone,
        copy_rate,
        dev_outputs: std::env::var("PM_DEV_OUTPUTS").is_ok_and(|v| v == "1") || dev_setting,
        genre_tags,
        prefer_full,
        first_steps_hidden,
        config_folders: config_folders(s),
        token_replaceable: s.token_file().is_some(),
    }
}

/// Show developer outputs (the Null output) in the zone lists (STEWARD-UX-INSTANCES item 5).
const DEV_OUTPUTS_KEY: &str = "ui.dev_outputs";

async fn node_settings(State(s): State<Arc<NodeState>>) -> ApiResult<NodeSettingsView> {
    Ok(Json(settings_view(&s)))
}

/// `POST /v1/node/settings/backup`: write the owner's settings to
/// `/priv/pubky.music/settings.json` now (also done after every sync).
async fn settings_backup_now(State(s): State<Arc<NodeState>>) -> ApiResult<serde_json::Value> {
    let sync = s.sync_handle().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "no Pubky session: sign in with Pubky Ring first",
        )
    })?;
    sync.backup_settings(&s).await;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// `POST /v1/node/settings/restore`: bring the owner's settings from the
/// homeserver onto this node (missing values only; existing ones stay).
async fn settings_restore_now(State(s): State<Arc<NodeState>>) -> ApiResult<sync::SettingsRestored> {
    let sync = s.sync_handle().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "no Pubky session: sign in with Pubky Ring first",
        )
    })?;
    let r = sync
        .restore_settings(&s, false)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?
        .ok_or_else(|| ApiError::not_found("no settings backup on the homeserver yet"))?;
    s.notify_library();
    Ok(Json(r))
}

/// `PUT /v1/node/settings {contact}`: set the contact without touching the
/// config file; discovery comes alive at once.
async fn update_node_settings(
    State(s): State<Arc<NodeState>>,
    Json(req): Json<NodeSettingsUpdate>,
) -> ApiResult<NodeSettingsView> {
    if let Some(c) = req.contact {
        s.set_contact(&c).map_err(ApiError::bad_request)?;
    }
    if let Some(z) = req.jukebox_zone {
        if !z.is_empty() {
            zone_of(&s, &z)?;
        }
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(subsonic::JUKEBOX_ZONE_KEY, &z)?;
    }
    if let Some(f) = req.prefer_full {
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(item::PREFER_FULL_KEY, if f { "1" } else { "0" })?;
    }
    if let Some(g) = req.genre_tags {
        let lib = s.library.lock().expect("library poisoned");
        lib.set_setting(tags::GENRE_TAGS_KEY, if g { "1" } else { "0" })?;
        // Switched on: the tags now (they go out with the next sync); off keeps the ones made
        if g {
            let n = tags::publish_genres(&lib)?;
            tracing::info!(tags = n, "files' genres published as tags");
        }
    }
    if let Some(h) = req.first_steps_hidden {
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(first_steps::HIDDEN_KEY, if h { "1" } else { "0" })?;
    }
    if let Some(d) = req.dev_outputs {
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(DEV_OUTPUTS_KEY, if d { "1" } else { "0" })?;
    }
    if let Some(r) = req.copy_rate {
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(files::COPY_RATE_KEY, &r.to_string())?;
    }
    Ok(Json(settings_view(&s)))
}

/// `[path, present]` for the config's folders, then the ones added through the API; present: the
/// folder is there now. Off the async threads: a stale network mount can hang the check.
async fn folder_list(s: &Arc<NodeState>) -> Result<Vec<(String, bool)>, ApiError> {
    let st = s.clone();
    tokio::task::spawn_blocking(move || folder_list_now(&st))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

fn folder_list_now(s: &NodeState) -> Result<Vec<(String, bool)>, ApiError> {
    let added = s.library.lock().expect("library poisoned").list_folders()?;
    let mut out: Vec<(std::path::PathBuf, bool)> = s.scan_folders.iter().map(|p| (p.clone(), true)).collect();
    for (p, enabled) in added {
        if !out.iter().any(|(q, _)| *q == p) {
            out.push((p, enabled));
        }
    }
    Ok(out
        .into_iter()
        .map(|(p, enabled)| {
            let present = enabled && p.is_dir();
            (p.to_string_lossy().into_owned(), present)
        })
        .collect())
}

fn config_folders(s: &NodeState) -> Vec<String> {
    s.scan_folders
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// `POST /v1/library/folders {path}`: add a folder, watch it, scan it.
async fn add_folder(
    State(s): State<Arc<NodeState>>,
    Json(req): Json<FolderRequest>,
) -> ApiResult<FoldersChanged> {
    let raw = req.path.trim();
    if raw.is_empty() {
        return Err(ApiError::bad_request("path is required"));
    }
    let path = std::path::PathBuf::from(raw);
    let path = tokio::fs::canonicalize(&path)
        .await
        .map_err(|e| ApiError::bad_request(format!("{}: {e}", path.display())))?;
    if !path.is_dir() {
        return Err(ApiError::bad_request(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    s.library.lock().expect("library poisoned").add_folder(&path)?;
    s.watch_more(&path);
    let scan_started = matches!(s.start_scan(vec![path]), Ok(true));
    Ok(Json(FoldersChanged {
        folders: folder_list(&s).await?,
        config_folders: config_folders(&s),
        scan_started,
        removed_files: 0,
        removed_tracks: 0,
        moved_files: 0,
    }))
}

/// `DELETE /v1/library/folders?path=`: forget a folder and everything scanned from it.
async fn remove_folder(
    State(s): State<Arc<NodeState>>,
    Query(req): Query<FolderRequest>,
) -> ApiResult<FoldersChanged> {
    let path = std::path::PathBuf::from(req.path.trim());
    if s.is_config_folder(&path) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "{} is in the node's config ([library] folders, or PM_FOLDERS in Docker), which reads it at every start: take it out there and restart the node, then remove it here to forget its songs",
                path.display()
            ),
        ));
    }
    if s.is_scanning() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a scan is running; try again when it is done",
        ));
    }
    s.unwatch(&path);
    let (removed_files, removed_tracks) = s.library.lock().expect("library poisoned").remove_folder(&path)?;
    s.notify_library();
    Ok(Json(FoldersChanged {
        folders: folder_list(&s).await?,
        config_folders: config_folders(&s),
        scan_started: false,
        removed_files,
        removed_tracks,
        moved_files: 0,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct FolderMove {
    from: String,
    to: String,
}

/// `POST /v1/library/folders/move {from, to}`: the music under `from` now lives under `to` (a
/// restore on another machine, a new disk). Tracks keep their matches, playlists and history;
/// a rescan follows.
async fn move_folder(
    State(s): State<Arc<NodeState>>,
    Json(m): Json<FolderMove>,
) -> ApiResult<FoldersChanged> {
    if s.is_config_folder(std::path::Path::new(m.from.trim())) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "{} is in the node's config ([library] folders, or PM_FOLDERS in Docker): put the new place there and restart the node",
                m.from.trim()
            ),
        ));
    }
    let to = tokio::fs::canonicalize(m.to.trim())
        .await
        .map_err(|e| ApiError::bad_request(format!("{}: {e}", m.to.trim())))?;
    if !to.is_dir() {
        return Err(ApiError::bad_request(format!("{} is not a folder", to.display())));
    }
    if !s.begin_scan() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a scan is running; try again when it is done",
        ));
    }
    let moved = s
        .library
        .lock()
        .expect("library poisoned")
        .move_folder(std::path::Path::new(m.from.trim()), &to);
    s.end_scan();
    let moved = moved?;
    s.unwatch(std::path::Path::new(m.from.trim()));
    s.watch_more(&to);
    let scan_started = matches!(s.start_scan(vec![to]), Ok(true));
    s.notify_library();
    Ok(Json(FoldersChanged {
        folders: folder_list(&s).await?,
        config_folders: config_folders(&s),
        scan_started,
        removed_files: 0,
        removed_tracks: 0,
        moved_files: moved,
    }))
}

/// Start a scan in the background on its own SQLite connection; one at a time.
async fn scan(State(s): State<Arc<NodeState>>, Json(body): Json<ScanRequest>) -> ApiResult<ScanStarted> {
    let folders: Vec<std::path::PathBuf> = if body.folders.is_empty() {
        s.all_folders()
    } else {
        body.folders.iter().map(std::path::PathBuf::from).collect()
    };
    match s.start_scan(folders.clone()) {
        Ok(true) => Ok(Json(ScanStarted {
            folders: folders.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        })),
        Ok(false) => Err(ApiError::new(StatusCode::CONFLICT, "a scan is already running")),
        Err(e) => Err(ApiError::bad_request(e)),
    }
}

/// `GET /v1/art/{album_id}?k=`: the cover for a renderer's screen (`NodeState::art_url`).
async fn keyed_cover(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, ApiError> {
    if q.get("k") != Some(&s.stream_key(&format!("art:{id}"))) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "missing or wrong key"));
    }
    album_cover(State(s), Path(id)).await
}

/// The album's cover image (extracted at scan time or found in the folder).
async fn album_cover(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let known = s.library.lock().expect("library poisoned").album_art(id)?;
    let art = match known {
        Some(a) => a,
        None => art::fetch_missing_cover(&s, id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("no cover for album {id}")))?,
    };
    let bytes = tokio::fs::read(&art.file)
        .await
        .map_err(|e| ApiError::not_found(format!("cover file for album {id}: {e}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, art.mime),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response())
}

// ---- pubky ----

async fn pubky_status(State(s): State<Arc<NodeState>>) -> ApiResult<PubkyStatus> {
    let unsynced = s
        .library
        .lock()
        .expect("library poisoned")
        .unsynced_music_posts()?
        .len();
    let sync = s.sync_handle();
    let needs_resign = sync.as_ref().is_some_and(|h| h.needs_resign());
    Ok(Json(PubkyStatus {
        user: sync.as_ref().map(|h| h.z32().to_string()),
        unsynced,
        needs_resign,
        missing: sync
            .as_ref()
            .map(|h| h.missing_capabilities())
            .unwrap_or_default(),
        notice: needs_resign.then(|| sync::RESIGN_NOTICE.to_string()),
        sync: s.sync_activity.lock().expect("sync activity").clone(),
    }))
}

/// Start a Pubky Ring sign-in: returns the `pubkyauth://` URL to scan or
/// paste into Pubky Ring. Poll with GET until approved.
async fn pubky_login_start(State(s): State<Arc<NodeState>>) -> ApiResult<LoginStarted> {
    let hs = s
        .homeserver()
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let flow = hs
        .start_login("pubky-music-node")
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let url = flow.url().to_string();
    *s.login.lock().await = Some(flow);
    let qr_svg = qr_svg(&url);
    Ok(Json(LoginStarted { url, qr_svg }))
}

const CONTROLLER_LOGIN_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A QR code of `text` as an SVG string (dark modules on a light tile, a
/// quiet zone of four modules), for Pubky Ring sign-ins.
pub(crate) fn qr_svg(text: &str) -> Option<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let w = code.width();
    let quiet = 4;
    let size = w + 2 * quiet;
    let mut path = String::new();
    for y in 0..w {
        for x in 0..w {
            if code[(x, y)] == qrcode::Color::Dark {
                path.push_str(&format!("M{} {}h1v1h-1z", x + quiet, y + quiet));
            }
        }
    }
    Some(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" shape-rendering=\"crispEdges\"><rect width=\"{size}\" height=\"{size}\" fill=\"#fff\"/><path d=\"{path}\" fill=\"#111\"/></svg>"
    ))
}

/// `POST /v1/auth/pubky/start`: a controller or browser asks to sign in with
/// Pubky Ring. Only the node's owner (the identity the node itself is signed
/// in as) is accepted; approval hands out the API token, so nothing is typed
/// or pasted. 412 until the node has a Pubky session.
async fn controller_login_start(State(s): State<Arc<NodeState>>) -> ApiResult<ControllerLoginStarted> {
    let Some(owner) = s.sync_handle().map(|h| h.z32().to_string()) else {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "the node is not signed in to Pubky yet: sign it in once (Settings → Pubky, or `pm-node login`)",
        ));
    };
    let hs = s
        .homeserver()
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let flow = hs
        .start_login_with(
            "pubky-music-controller",
            pm_pubky::homeserver::CONTROLLER_CAPABILITIES,
        )
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let url = flow.url().to_string();
    let id = tidal::random_urlsafe(24);
    let mut logins = s.controller_logins.lock().await;
    logins.retain(|_, (_, at)| at.elapsed() < CONTROLLER_LOGIN_TTL);
    logins.insert(id.clone(), (flow, std::time::Instant::now()));
    let qr_svg = qr_svg(&url);
    Ok(Json(ControllerLoginStarted {
        id,
        url,
        owner,
        qr_svg,
    }))
}

/// `GET /v1/auth/pubky/poll?id=`: `pending` until Ring approves; `ok` with
/// the API token when the approver is the owner; `denied` otherwise.
async fn controller_login_poll(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<ControllerLoginStatus> {
    let id = q.get("id").cloned().unwrap_or_default();
    let mut logins = s.controller_logins.lock().await;
    let Some((flow, at)) = logins.get(&id) else {
        return Ok(Json(ControllerLoginStatus {
            status: "expired".into(),
            token: None,
            user: None,
            role: None,
        }));
    };
    if at.elapsed() > CONTROLLER_LOGIN_TTL {
        logins.remove(&id);
        return Ok(Json(ControllerLoginStatus {
            status: "expired".into(),
            token: None,
            user: None,
            role: None,
        }));
    }
    match flow.poll().await {
        Ok(None) => Ok(Json(ControllerLoginStatus {
            status: "pending".into(),
            token: None,
            user: None,
            role: None,
        })),
        Ok(Some(session)) => {
            logins.remove(&id);
            let who = session.public_key().to_string();
            let owner = s.sync_handle().map(|h| h.z32().to_string()).unwrap_or_default();
            if who != owner {
                // A listener in People with music access gets a personal token
                if people::may_use_music(&s, &who) {
                    let token = accounts::mint(&s, &who)?;
                    tracing::info!(%who, "listener signed in with Pubky Ring");
                    return Ok(Json(ControllerLoginStatus {
                        status: "ok".into(),
                        token: Some(token),
                        user: Some(who),
                        role: Some("listener".into()),
                    }));
                }
                tracing::warn!(%who, %owner, "controller sign-in by a different identity refused");
                return Ok(Json(ControllerLoginStatus {
                    status: "denied".into(),
                    token: None,
                    user: Some(who),
                    role: None,
                }));
            }
            tracing::info!(%who, "controller signed in with Pubky Ring");
            Ok(Json(ControllerLoginStatus {
                status: "ok".into(),
                token: s.token(),
                user: Some(who),
                role: Some("owner".into()),
            }))
        }
        Err(e) => {
            logins.remove(&id);
            Err(ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))
        }
    }
}

/// Poll the sign-in. When approved: the session is kept, its secret written
/// to the configured file, the node record published, playlists synced.
async fn pubky_login_poll(State(s): State<Arc<NodeState>>) -> ApiResult<LoginStatus> {
    if let Some(sync) = s.sync_handle() {
        if s.login.lock().await.is_none() {
            return Ok(Json(LoginStatus {
                status: "signed_in".into(),
                user: Some(sync.z32().to_string()),
                url: None,
            }));
        }
    }
    let mut guard = s.login.lock().await;
    let Some(flow) = guard.as_ref() else {
        return Ok(Json(LoginStatus {
            status: "none".into(),
            user: None,
            url: None,
        }));
    };
    let url = flow.url().to_string();
    match flow.poll().await {
        Ok(None) => Ok(Json(LoginStatus {
            status: "pending".into(),
            user: None,
            url: Some(url),
        })),
        Ok(Some(session)) => {
            *guard = None;
            drop(guard);
            let secret = pm_pubky::homeserver::export_session_secret(&session).await;
            if let (Some(path), Some(secret)) = (&s.session_secret_path, &secret) {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                // Created 0600, never written first and made private after
                if let Err(e) = imports::write_private(path, secret.as_bytes()) {
                    tracing::warn!(path = %path.display(), "cannot store the session secret: {e}");
                }
            }
            let hs = s
                .homeserver()
                .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
            let sync = Arc::new(sync::PubkySync::from_session(hs, session));
            let user = sync.z32().to_string();
            s.set_sync(Some(sync.clone()));
            tracing::info!(user = %user, "pubky sign-in: session ready");
            // "Signed in" now; the node record, settings and the first sync follow in the
            // background (they took minutes, one request at a time, before the answer)
            sync::first_sync_in_background(&s, sync.clone());
            Ok(Json(LoginStatus {
                status: "signed_in".into(),
                user: Some(user),
                url: None,
            }))
        }
        Err(e) => {
            *guard = None;
            Err(ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))
        }
    }
}

/// Push local changes to the private homeserver folder and pull what is missing.
async fn pubky_sync(State(s): State<Arc<NodeState>>) -> ApiResult<SyncReport> {
    let Some(sync) = s.sync_handle() else {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "no Pubky session: set `pubky.session_secret_file` in the node config",
        ));
    };
    let report = sync
        .sync(&s.library)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    sync.backup_settings(&s).await;
    if report.pulled > 0 {
        s.notify_library();
    }
    sync.carry_on(&s, &report);
    Ok(Json(report))
}

#[cfg(test)]
mod l16_tests {
    use super::L16;

    #[test]
    fn pcm_becomes_big_endian_lpcm_across_any_chunking() {
        let pcm = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06];
        let want = vec![0x02, 0x01, 0x04, 0x03, 0x06, 0x05];
        for cut in 0..pcm.len() {
            for cut2 in cut..pcm.len() {
                let mut c = L16::default();
                let mut got = c.convert(&pcm[..cut]);
                got.extend(c.convert(&pcm[cut..cut2]));
                got.extend(c.convert(&pcm[cut2..]));
                assert_eq!(got, want, "cuts at {cut}, {cut2}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use pm_audio::PipelineConfig;
    use pm_library::Library;
    use pm_zones::outputs::NullOutput;
    use pm_zones::OutputRegistry;
    use tower::ServiceExt;

    fn write_wav(path: &std::path::Path, frames: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            w.write_sample((i % 100) as i16).unwrap();
            w.write_sample(-((i % 100) as i16)).unwrap();
        }
        w.finalize().unwrap();
    }

    async fn state(dir: &std::path::Path, token: Option<&str>) -> Arc<NodeState> {
        state_with(dir, token, Default::default()).await
    }

    async fn state_with(
        dir: &std::path::Path,
        token: Option<&str>,
        pubky: pm_pubky::config::PmPubkyConfig,
    ) -> Arc<NodeState> {
        let db = dir.join("lib.sqlite");
        let lib = Library::open(&db).unwrap();
        let outputs = OutputRegistry::new();
        outputs.register(Arc::new(NullOutput::new()));
        let s = NodeState::new(
            NodeSettings {
                name: "test".into(),
                version: "0".into(),
                db_path: db,
                scan_folders: vec![dir.join("music")],
                art_dir: dir.join("art"),
                mirror_dir: dir.join("homeserver"),
                pipeline: PipelineConfig::default(),
                token: token.map(str::to_string),
                subsonic_user: "pubky".into(),
                pubky,
                public_url: "http://127.0.0.1:7788".into(),
                upnp: false,
                cast: false,
                slimproto_port: 0,
                session_secret_path: None,
                tidal: Default::default(),
                spotify: Default::default(),
                acoustid_key: String::new(),
                jamendo: Default::default(),
            },
            lib,
            outputs,
        );
        // Never reach the real Deezer API from tests; a closed port fails fast.
        *s.deezer_base.write().unwrap() = "http://127.0.0.1:9".into();
        s.zones_for_all_outputs();
        s
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(match body {
                Some(b) => Body::from(b.to_string()),
                None => Body::empty(),
            })
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!(
                    "{method} {uri}: {status}: not JSON ({e}): {}",
                    String::from_utf8_lossy(&bytes)
                )
            })
        };
        (status, json)
    }

    #[tokio::test]
    async fn scan_then_queue_and_play_a_library_track() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/one.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());

        let (st, info) = call(&app, "GET", "/v1/info", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(info["api"], "v1");
        assert_eq!(info["zones"], 1);

        let (st, _) = call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
        assert_eq!(st, StatusCode::OK);
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!s.is_scanning());
        let (st, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        assert_eq!(st, StatusCode::OK);
        let id = tracks["tracks"][0]["id"].as_i64().unwrap();
        let (st, found) = call(&app, "GET", "/v1/library/search?q=one", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(found["tracks"][0]["id"], id);

        let (st, zones) = call(&app, "GET", "/v1/zones", None).await;
        assert_eq!(st, StatusCode::OK);
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"track_ids": [id], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        assert_eq!(q["ids"].as_array().unwrap().len(), 1);
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        let st = zone.wait_stopped().await;
        assert_eq!(st.error, None);
        let (st, q) = call(&app, "GET", &format!("/v1/zones/{zid}/queue"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(q[0]["uri"], format!("track:{id}"));
        assert_eq!(q[0]["title"], "One", "an untagged file is titled from its name");

        // The play was recorded by the zone event hook; a client can add its own.
        let mut recorded = false;
        for _ in 0..200 {
            let (_, h) = call(&app, "GET", "/v1/library/history", None).await;
            if !h["plays"].as_array().unwrap().is_empty() {
                assert_eq!(h["plays"][0]["track"]["id"], id);
                assert_eq!(h["plays"][0]["zone"], zid);
                recorded = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(recorded, "node zone play not recorded");
        let (st, h) = call(
            &app,
            "POST",
            "/v1/library/history",
            Some(serde_json::json!({"track_id": id, "zone": "local"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{h}");
        assert_eq!(h["plays"][0]["zone"], "local");
        let (st, _) = call(
            &app,
            "POST",
            "/v1/library/history",
            Some(serde_json::json!({"track_id": 999_999})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, home) = call(&app, "GET", "/v1/library/home", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(home["history"].as_array().unwrap().len(), 2);
        assert_eq!(home["most_played"][0]["plays"], 2);
        assert_eq!(
            home["recently_added"].as_array().unwrap().len(),
            home["stats"]["albums"]
        );

        // Without a contact, the Pubky view of an object is a clear 412; the
        // contact can be set through the API and discovery comes alive.
        let (st, e) = call(&app, "GET", &format!("/v1/discover/uri?track_id={id}"), None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED, "{e}");
        let (st, v) = call(&app, "GET", "/v1/node/settings", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["discovery"], false);
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"contact": "github.com/x"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"contact": " me@example.org "})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["contact"], "me@example.org");
        assert_eq!(v["discovery"], true);
        assert_eq!(
            s.library
                .lock()
                .unwrap()
                .get_setting("node.contact")
                .unwrap()
                .as_deref(),
            Some("me@example.org")
        );

        // Unknown track and unknown zone are 404s.
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"track_ids": [999]})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = call(&app, "GET", "/v1/zones/nope", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        // Transport on an empty-after-clear zone is accepted and reports state.
        let (st, _) = call(&app, "DELETE", &format!("/v1/zones/{zid}/queue"), None).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (st, z) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transport"),
            Some(serde_json::json!({"action": "play"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(z["transport"], "stopped");
    }

    /// Serve the FLAC fixture over HTTP (directly and behind a PLS playlist)
    /// and play both through a zone: the HTTP stream source end to end.
    #[tokio::test]
    async fn http_streams_play_like_files() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let pls =
            format!("[playlist]\nNumberOfEntries=1\nFile1=http://127.0.0.1:{port}/a.flac\nTitle1=Fixture\n");
        let files = Router::new()
            .route(
                "/a.flac",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            )
            .route(
                "/list.pls",
                get(move || {
                    let pls = pls.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/x-scpls")], pls) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, files).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        for uri in [
            format!("http://127.0.0.1:{port}/a.flac"),
            format!("http://127.0.0.1:{port}/list.pls"),
        ] {
            let (st, q) = call(
                &app,
                "POST",
                &format!("/v1/zones/{zid}/queue"),
                Some(serde_json::json!({"items": [{"uri": uri, "title": "Fixture radio"}], "replace": true, "play": true})),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{q}");
            let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
            let st = zone.wait_stopped().await;
            assert_eq!(st.error, None, "{uri}");
        }
        // Saved stations round trip; playing one goes through the same path.
        let (st, saved) = call(
            &app,
            "PUT",
            "/v1/radio/stations",
            Some(serde_json::json!({"id": "", "name": "Fixture FM", "url": format!("http://127.0.0.1:{port}/a.flac"), "tags": ["test"]})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{saved}");
        let id = saved["stations"][0]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("url-"));
        let (st, played) = call(
            &app,
            "POST",
            "/v1/radio/play",
            Some(serde_json::json!({"zone": zid, "station": saved["stations"][0]})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{played}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
        let (_, q) = call(&app, "GET", &format!("/v1/zones/{zid}/queue"), None).await;
        assert_eq!(q[0]["title"], "Fixture FM");
        assert_eq!(q[0]["album"], "Internet radio");
        let (st, _) = call(&app, "DELETE", &format!("/v1/radio/stations/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(&app, "DELETE", &format!("/v1/radio/stations/{id}"), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (_, left) = call(&app, "GET", "/v1/radio/stations", None).await;
        assert!(left["stations"].as_array().unwrap().is_empty());
        let (st, _) = call(&app, "GET", "/v1/radio/search", None).await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "search without terms is refused before any network call"
        );
    }

    #[tokio::test]
    async fn folders_can_be_added_and_removed_through_the_api() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        let extra = dir.path().join("more");
        std::fs::create_dir(&extra).unwrap();
        write_wav(&extra.join("extra.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (st, _) = call(
            &app,
            "POST",
            "/v1/library/folders",
            Some(serde_json::json!({"path": "/nope/missing"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, r) = call(
            &app,
            "POST",
            "/v1/library/folders",
            Some(serde_json::json!({"path": extra.to_string_lossy()})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["scan_started"], true);
        assert!(r["folders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f[0] == extra.to_string_lossy().as_ref()));
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        assert_eq!(tracks["tracks"].as_array().unwrap().len(), 1);
        let (st, r) = call(
            &app,
            "DELETE",
            &format!("/v1/library/folders?path={}", extra.to_string_lossy()),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["removed_tracks"], 1);
        // Only the config's folder is left
        let music = dir.path().join("music");
        assert_eq!(r["folders"], serde_json::json!([[music.to_string_lossy(), true]]));
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        assert!(tracks["tracks"].as_array().unwrap().is_empty());
    }

    /// A zone's queue is saved on every change and comes back after a restart.
    #[tokio::test]
    async fn queues_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/one.wav"), 4410);
        let zid;
        {
            let s = state(dir.path(), None).await;
            let app = router(s.clone());
            call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
            for _ in 0..500 {
                if !s.is_scanning() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
            let id = tracks["tracks"][0]["id"].as_i64().unwrap();
            let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
            zid = zones[0]["id"].as_str().unwrap().to_string();
            let (st, _) = call(
                &app,
                "POST",
                &format!("/v1/zones/{zid}/queue"),
                Some(serde_json::json!({"track_ids": [id], "items": [{"uri": "https://radio.example/live", "title": "Live"}], "play": false})),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            let (st, _) = call(
                &app,
                "PUT",
                &format!("/v1/zones/{zid}/repeat"),
                Some(serde_json::json!({"repeat": "all"})),
            )
            .await;
            assert_eq!(st, StatusCode::NO_CONTENT);
            // The save happens on the state watch; give it a moment.
            let key = format!("queue.{zid}");
            let mut saved = None;
            for _ in 0..200 {
                saved = s.library.lock().unwrap().get_setting(&key).unwrap();
                if saved
                    .as_deref()
                    .is_some_and(|j| j.contains("radio.example") && j.contains("\"all\""))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(saved.is_some(), "queue was not saved");
        }
        // "Restart": a new node state over the same database and outputs.
        let s2 = state(dir.path(), None).await;
        let app2 = router(s2.clone());
        let (st, q) = call(&app2, "GET", &format!("/v1/zones/{zid}/queue"), None).await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let q = q.as_array().unwrap();
        assert_eq!(q.len(), 2, "{q:?}");
        assert_eq!(q[0]["title"], "One");
        assert_eq!(q[1]["uri"], "https://radio.example/live");
        let (_, z) = call(&app2, "GET", &format!("/v1/zones/{zid}"), None).await;
        assert_eq!(z["repeat"], "all");
        assert_eq!(z["transport"], "stopped", "restored queues do not start playing");
    }

    /// TIDAL full tracks (unofficial) against a mock of TIDAL's auth and private API: the device
    /// sign-in, a BTS stream and a DASH stream read whole, an encrypted track refused, and a
    /// refused client switching the mode off (playback then falls back to the official path).
    #[tokio::test]
    async fn tidal_full_tracks_sign_in_and_stream_from_a_mock() {
        use base64::Engine;
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let b64 = |v: &str| base64::engine::general_purpose::STANDARD.encode(v);
        let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mpd = format!(
            r#"<MPD><Period><AdaptationSet mimeType="audio/mp4"><Representation codecs="flac" audioSamplingRate="96000"><SegmentTemplate initialization="{base}/seg/init" media="{base}/seg/$Number$" startNumber="1"><SegmentTimeline><S d="1" r="1"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#
        );
        let bts = format!(
            r#"{{"mimeType":"audio/flac","codecs":"flac","encryptionType":"NONE","urls":["{base}/file.flac"]}}"#
        );
        let enc = format!(
            r#"{{"mimeType":"audio/flac","codecs":"flac","encryptionType":"OLD_AES","urls":["{base}/file.flac"]}}"#
        );
        let mock = Router::new()
            .route(
                "/v1/oauth2/device_authorization",
                post(|body: String| async move {
                    // An app from TIDAL's developer portal gets TIDAL's refusal
                    if body.contains("client_id=portal-app") {
                        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid_request", "error_description": "Client is not a Limited Input Device client"})));
                    }
                    assert!(body.contains("client_id=dev-id"), "{body}");
                    (StatusCode::OK, Json(serde_json::json!({"deviceCode": "DC", "userCode": "ABCDE", "verificationUriComplete": "link.tidal.com/ABCDE", "expiresIn": 300, "interval": 1})))
                }),
            )
            .route(
                "/v1/oauth2/token",
                post({
                    let polls = polls.clone();
                    move |body: String| {
                        let polls = polls.clone();
                        async move {
                            assert!(body.contains("client_secret=dev-secret"), "{body}");
                            // The owner approves at the second poll
                            if polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "authorization_pending"})));
                            }
                            (StatusCode::OK, Json(serde_json::json!({"access_token": "AT", "refresh_token": "RT", "expires_in": 3600})))
                        }
                    }
                }),
            )
            .route(
                "/v1/sessions",
                get(|| async { Json(serde_json::json!({"userId": 42, "countryCode": "CH", "sessionId": "sid"})) }),
            )
            .route(
                "/v1/tracks/{id}/playbackinfopostpaywall",
                get(move |Path(id): Path<String>, q: axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let (mpd, bts, enc) = (mpd.clone(), bts.clone(), enc.clone());
                    async move {
                        assert_eq!(q.get("countryCode").map(String::as_str), Some("CH"));
                        assert_eq!(q.get("audioquality").map(String::as_str), Some("HI_RES_LOSSLESS"));
                        let (kind, m) = match id.as_str() {
                            "1" => ("application/vnd.tidal.bts", bts),
                            "2" => ("application/dash+xml", mpd),
                            "3" => ("application/vnd.tidal.bts", enc),
                            _ => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"userMessage": "client not allowed"}))),
                        };
                        (StatusCode::OK, Json(serde_json::json!({"assetPresentation": "FULL", "manifestMimeType": kind, "manifest": b64(&m)})))
                    }
                }),
            )
            .route("/file.flac", get(move || { let f = flac.clone(); async move { f } }))
            .route("/seg/{n}", get(|Path(n): Path<String>| async move { format!("[{n}]") }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        {
            let mut ep = s.tidal.write().unwrap();
            ep.auth_base = base.clone();
            ep.v1_base = format!("{base}/v1");
        }
        let app = router(s.clone());
        let (_, st) = call(&app, "GET", "/v1/sources/tidal/unofficial", None).await;
        assert_eq!(st["enabled"], false, "off by default");
        assert!(st["warning"].as_str().unwrap().contains("not approved"));
        let (st, r) = call(&app, "POST", "/v1/sources/tidal/unofficial/login", None).await;
        assert_eq!(st, StatusCode::BAD_GATEWAY, "no device client yet: {r}");
        // A developer-portal app is not a device client: plain words, not TIDAL's raw error
        let (_, _) = call(
            &app,
            "PUT",
            "/v1/sources/tidal/unofficial",
            Some(serde_json::json!({"client_id": "portal-app", "client_secret": "portal-secret"})),
        )
        .await;
        let (st, r) = call(&app, "POST", "/v1/sources/tidal/unofficial/login", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert_eq!(r["error"], tidal_unofficial::NOT_A_DEVICE_CLIENT, "{r}");
        let (_, r) = call(
            &app,
            "PUT",
            "/v1/sources/tidal/unofficial",
            Some(serde_json::json!({"client_id": "dev-id", "client_secret": "dev-secret"})),
        )
        .await;
        assert_eq!(r["client_from"], "settings");
        let (st, r) = call(&app, "POST", "/v1/sources/tidal/unofficial/login", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["pending"]["user_code"], "ABCDE");
        assert_eq!(r["pending"]["uri"], "https://link.tidal.com/ABCDE");
        let mut signed = serde_json::Value::Null;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let (_, r) = call(&app, "GET", "/v1/sources/tidal/unofficial", None).await;
            if r["signed_in"] == true {
                signed = r;
                break;
            }
        }
        assert_eq!(signed["user"], "42", "{signed}");
        assert_eq!(signed["enabled"], true, "signing in switches the mode on");
        assert!(signed.get("pending").is_none());

        let open = |id: &'static str| {
            let s = s.clone();
            tokio::task::spawn_blocking(move || {
                use std::io::Read;
                let ep = s.tidal.read().unwrap().clone();
                let r = crate::tidal_unofficial::open_blocking(&s.library, &ep, id);
                crate::tidal_unofficial::record(&s.library, &r);
                r.map(|o| {
                    o.map(|mut f| {
                        let mut b = Vec::new();
                        f.reader.read_to_end(&mut b).unwrap();
                        (f.ext, b)
                    })
                })
            })
        };
        let (ext, bytes) = open("1").await.unwrap().unwrap().unwrap();
        assert_eq!(ext, "flac");
        assert!(
            bytes.starts_with(b"fLaC") && bytes.len() > 1000,
            "the whole file through BTS"
        );
        let (ext, bytes) = open("2").await.unwrap().unwrap().unwrap();
        assert_eq!(
            (ext, String::from_utf8(bytes).unwrap().as_str()),
            ("m4a", "[init][1][2]"),
            "DASH: init, then the segments in order"
        );
        match open("3").await.unwrap() {
            Err(crate::tidal_unofficial::FullError::Track(e)) => assert!(e.contains("encrypted"), "{e}"),
            other => panic!(
                "an encrypted track is refused, never decrypted: {:?}",
                other.map(|o| o.map(|x| x.0))
            ),
        }
        let (_, r) = call(&app, "GET", "/v1/sources/tidal/unofficial", None).await;
        assert_eq!(r["enabled"], true, "one track's trouble leaves the mode on");
        match open("9").await.unwrap() {
            Err(crate::tidal_unofficial::FullError::Mode(e)) => {
                assert!(e.contains("client not allowed"), "{e}")
            }
            other => panic!(
                "a refused client is the mode's failure: {:?}",
                other.map(|o| o.map(|x| x.0))
            ),
        }
        let (_, r) = call(&app, "GET", "/v1/sources/tidal/unofficial", None).await;
        assert_eq!(r["enabled"], false, "the mode switched itself off: {r}");
        assert_eq!(r["health"]["ok"], false);
        assert_eq!(r["signed_in"], true, "the sign-in is kept for Retry");
        assert!(
            open("1").await.unwrap().unwrap().is_none(),
            "off: the official path plays"
        );
        let (_, r) = call(
            &app,
            "PUT",
            "/v1/sources/tidal/unofficial",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        assert!(r["enabled"] == true && r.get("health").is_none(), "Retry: {r}");
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/tidal/unofficial",
            Some(serde_json::json!({"quality": "best"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// YouTube is a Sources tab (on/off) that plays in the browser only: a zone refuses its items
    /// with the reason, and it does not search yet.
    #[tokio::test]
    async fn youtube_items_play_only_in_the_browser() {
        use pm_zones::SourceFactory;
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, sources) = call(&app, "GET", "/v1/sources", None).await;
        let yt = sources
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "youtube")
            .unwrap()
            .clone();
        assert_eq!(yt["status"], "browser");
        let (st, _) = call(&app, "GET", "/v1/sources/youtube/search?q=x", None).await;
        assert_eq!(
            st,
            StatusCode::PRECONDITION_FAILED,
            "search needs the owner's key"
        );
        assert!(!crate::search::sources_for(&s, "everywhere").contains(&"youtube".to_string()));
        let err = s
            .sources
            .open(&pm_zones::QueueItem::new("youtube:video:dQw4w9WgXcQ"))
            .err()
            .expect("a zone refuses a YouTube item");
        assert!(err.to_string().contains("only in This browser"), "{err}");
    }

    /// A source switched off on Sources: hidden from search and the play order, kept listed,
    /// remembered across restarts.
    #[tokio::test]
    async fn a_source_switched_off_is_hidden_and_keeps_its_settings() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, sources) = call(&app, "GET", "/v1/sources", None).await;
        assert!(
            sources.as_array().unwrap().iter().all(|x| x["enabled"] == true),
            "{sources}"
        );
        for id in ["deezer", "tidal"] {
            let (st, r) = call(
                &app,
                "PUT",
                &format!("/v1/sources/{id}/enabled"),
                Some(serde_json::json!({"enabled": false})),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{r}");
            assert_eq!(r["enabled"], false);
        }
        let (_, sources) = call(&app, "GET", "/v1/sources", None).await;
        let deezer = sources
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "deezer")
            .unwrap()
            .clone();
        assert_eq!(
            deezer["enabled"], false,
            "still listed, so it can be switched on again"
        );
        let (st, _) = call(&app, "GET", "/v1/sources/deezer/search?q=x", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "an off source does not search");
        assert!(!crate::search::sources_for(&s, "everywhere").contains(&"deezer".to_string()));
        assert_eq!(crate::search::sources_for(&s, "deezer"), ["library"]);
        let (_, po) = call(&app, "GET", "/v1/play-order", None).await;
        let tidal = po["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "tidal")
            .unwrap()
            .clone();
        assert_eq!(tidal["note"], "turned off on Sources", "{po}");
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/nope/enabled",
            Some(serde_json::json!({"enabled": false})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // Kept across a restart; switching on brings it back
        drop(app);
        drop(s);
        let s = state(dir.path(), None).await;
        assert!(!s.source_enabled("deezer") && !s.source_enabled("tidal") && s.source_enabled("jamendo"));
        let app = router(s.clone());
        let (_, r) = call(
            &app,
            "PUT",
            "/v1/sources/deezer/enabled",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        assert_eq!(r["enabled"], true);
        assert!(crate::search::sources_for(&s, "everywhere").contains(&"deezer".to_string()));
    }

    /// The Internet Archive source against a local mock: search, item
    /// tracks with the best format, and the tracks play through a zone.
    #[tokio::test]
    async fn internet_archive_source_searches_and_plays() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route(
                "/advancedsearch.php",
                get(|| async {
                    Json(serde_json::json!({"response": {"docs": [
                        {"identifier": "fixture-set", "title": "Fixture Set", "creator": "Tester", "year": "2020", "downloads": 7}
                    ]}}))
                }),
            )
            .route(
                "/metadata/{id}",
                get(|| async {
                    Json(serde_json::json!({
                        "metadata": {"title": "Fixture Set", "creator": "Tester", "year": "2020"},
                        "files": [
                            {"name": "one.flac", "format": "Flac", "source": "original", "title": "One", "track": "1", "length": "5.15"},
                            {"name": "one.mp3", "format": "VBR MP3", "source": "derivative", "original": "one.flac", "title": "One", "track": "1"}
                        ]
                    }))
                }),
            )
            .route(
                "/download/{id}/{file}",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        *s.archive_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (st, sources) = call(&app, "GET", "/v1/sources", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(sources[0]["id"], "archive");
        let (st, _) = call(&app, "GET", "/v1/sources/archive/search?q=", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, r) = call(&app, "GET", "/v1/sources/archive/search?q=fixture", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["items"][0]["id"], "fixture-set");
        let (st, it) = call(&app, "GET", "/v1/sources/archive/items/fixture-set", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(
            it["tracks"].as_array().unwrap().len(),
            1,
            "one file per recording"
        );
        assert_eq!(it["tracks"][0]["format"], "Flac");
        let url = it["tracks"][0]["url"].as_str().unwrap().to_string();
        assert!(
            url.starts_with(&format!("http://127.0.0.1:{port}/download/fixture-set/")),
            "{url}"
        );
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": url, "title": "One", "album": "Fixture Set"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
    }

    /// Audius against a local mock: the discovery host is configured, a search
    /// yields track hits, an item has one streamable track, and it plays.
    #[tokio::test]
    async fn health_says_which_part_is_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(|| async { StatusCode::NOT_FOUND }),
            )
            .await
            .unwrap()
        });
        // A port nothing listens on
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{closed}"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        {
            let mut j = s.jamendo.write().unwrap();
            j.base = format!("http://127.0.0.1:{port}");
            j.creds.client_id = "x".into();
        }
        let app = router(s.clone());
        let (st, h) = call(&app, "GET", "/v1/health", None).await;
        assert_eq!(st, StatusCode::OK, "{h}");
        let by = |id: &str| h.as_array().unwrap().iter().find(|p| p["id"] == id).cloned();
        let pinakes = by("pinakes").expect("pinakes is listed");
        assert_eq!(pinakes["state"], "down", "{h}");
        assert_eq!(pinakes["detail"], "cannot connect");
        assert_eq!(by("jamendo").unwrap()["state"], "ok", "any HTTP answer counts");
        assert!(
            by("tidal").is_none() && by("homeserver").is_none(),
            "parts not set up are left out"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listenbrainz_gets_what_zones_play_and_gives_back_history_and_picks() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let got: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Default::default();
        let down = Arc::new(AtomicBool::new(true));
        let (g1, d1) = (got.clone(), down.clone());
        const PL: &str = "e81452b9-3165-4f43-a8d9-f6c2c5ed7154";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token_ok = |h: &axum::http::HeaderMap| {
            h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Token LBT")
        };
        let mock = Router::new()
            .route(
                "/1/validate-token",
                get(move |h: axum::http::HeaderMap| async move {
                    Json(if token_ok(&h) { serde_json::json!({"valid": true, "user_name": "owner"}) } else { serde_json::json!({"valid": false}) })
                }),
            )
            .route(
                "/1/submit-listens",
                post(move |h: axum::http::HeaderMap, Json(b): Json<serde_json::Value>| {
                    let (g, d) = (g1.clone(), d1.clone());
                    async move {
                        assert!(token_ok(&h));
                        if b["listen_type"] == "playing_now" {
                            return StatusCode::OK;
                        }
                        if d.load(Ordering::SeqCst) {
                            return StatusCode::SERVICE_UNAVAILABLE;
                        }
                        g.lock().unwrap().extend(b["payload"].as_array().unwrap().iter().cloned());
                        StatusCode::OK
                    }
                }),
            )
            .route(
                "/1/user/owner/listens",
                get(|| async {
                    Json(serde_json::json!({"payload": {"count": 2, "listens": [
                        {"listened_at": 1792593797, "track_metadata": {"track_name": "Some Resolve", "artist_name": "X", "mbid_mapping": {"recording_mbid": "30d08f4c-d825-4ae1-b79c-44242cddd7c0"}}},
                        {"listened_at": 1792593336, "track_metadata": {"track_name": "Congruence", "artist_name": "Y"}}]}}))
                }),
            )
            .route(
                "/1/cf/recommendation/user/owner/recording",
                get(|| async {
                    Json(serde_json::json!({"payload": {"mbids": [{"recording_mbid": "bd953e77-f7df-467b-b1cf-e42d3eec5968", "score": 0.6}, {"recording_mbid": "2456f690-add7-467f-9ca0-000000000000", "score": 0.5}]}}))
                }),
            )
            .route(
                "/1/user/owner/playlists/createdfor",
                get(|| async {
                    Json(serde_json::json!({"playlist_count": 1, "playlists": [{"playlist": {"title": "Weekly Jams for owner", "identifier": format!("https://listenbrainz.org/playlist/{PL}"), "track": []}}]}))
                }),
            )
            .route(
                "/1/playlist/{id}",
                get(|| async {
                    Json(serde_json::json!({"playlist": {"title": "Weekly Jams for owner", "track": [
                        {"identifier": ["https://musicbrainz.org/recording/bd953e77-f7df-467b-b1cf-e42d3eec5968"], "title": "Sherpa"},
                        {"identifier": ["https://example.com/x"], "title": "?"}]}}))
                }),
            )
            .route(
                "/song.flac",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        *s.listenbrainz_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/listenbrainz",
            Some(serde_json::json!({"token": "nope"})),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, v) = call(
            &app,
            "PUT",
            "/v1/listenbrainz",
            Some(serde_json::json!({"token": "LBT"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(
            (v["user"].as_str(), v["submit"].as_bool()),
            (Some("owner"), Some(true))
        );
        assert!(!v.to_string().contains("LBT"));

        // A zone plays a song to its end while ListenBrainz is down: the listen waits
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        call(&app, "POST", &format!("/v1/zones/{zid}/queue"), Some(serde_json::json!({"items": [
            {"uri": format!("http://127.0.0.1:{port}/song.flac"), "title": "Song", "artist": "Band", "album": "Record"}], "replace": true, "play": true}))).await;
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
        let mut waiting = 0;
        for _ in 0..300 {
            let (_, v) = call(&app, "GET", "/v1/listenbrainz", None).await;
            waiting = v["pending"].as_u64().unwrap();
            if waiting == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(waiting, 1, "kept for later");
        // ListenBrainz is back: the waiting listen goes
        down.store(false, Ordering::SeqCst);
        let (_, v) = call(
            &app,
            "PUT",
            "/v1/listenbrainz",
            Some(serde_json::json!({"submit": true})),
        )
        .await;
        assert_eq!(v["pending"], 0);
        {
            let got = got.lock().unwrap();
            assert_eq!(got.len(), 1, "{got:?}");
            let m = &got[0]["track_metadata"];
            assert_eq!(
                (
                    m["track_name"].as_str(),
                    m["artist_name"].as_str(),
                    m["release_name"].as_str()
                ),
                (Some("Song"), Some("Band"), Some("Record"))
            );
            assert_eq!(m["additional_info"]["media_player"], "Pubky Music");
            assert!(got[0]["listened_at"].as_i64().unwrap() > 1_700_000_000);
        }

        // History comes back by MusicBrainz recording, once
        let (st, imp) = call(&app, "POST", "/v1/listenbrainz/import", None).await;
        assert_eq!(st, StatusCode::OK, "{imp}");
        assert_eq!(
            (imp["imported"].as_u64(), imp["skipped"].as_u64()),
            (Some(1), Some(1))
        );
        let (_, again) = call(&app, "POST", "/v1/listenbrainz/import", None).await;
        assert_eq!(again["imported"], 0);
        let (_, acts) = call(&app, "GET", "/v1/activity", None).await;
        assert!(
            acts.to_string().contains("30d08f4c-d825-4ae1-b79c-44242cddd7c0"),
            "{acts}"
        );

        // Picks and the playlists made for the user
        let (st, rec) = call(&app, "GET", "/v1/listenbrainz/recommendations", None).await;
        assert_eq!(st, StatusCode::OK, "{rec}");
        assert_eq!(rec.as_array().unwrap().len(), 2);
        let (_, pls) = call(&app, "GET", "/v1/listenbrainz/playlists", None).await;
        assert_eq!(pls[0]["id"], PL);
        let (st, imp) = call(
            &app,
            "POST",
            &format!("/v1/listenbrainz/playlists/{PL}/import"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{imp}");
        assert_eq!(
            (imp["imported"].as_u64(), imp["skipped"].as_u64()),
            (Some(1), Some(1))
        );

        let (_, v) = call(&app, "DELETE", "/v1/listenbrainz", None).await;
        assert_eq!(v["connected"], false);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jellyfin_signs_in_once_and_keeps_only_the_token() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        const SONG: &str = "0123456789abcdef0123456789abcdef";
        const ALBUM: &str = "fedcba9876543210fedcba9876543210";
        const PL: &str = "aaaabbbbccccddddeeeeffff00001111";
        let song = serde_json::json!({"Id": SONG, "Name": "Bohemian Rhapsody", "Type": "Audio", "Artists": ["Queen"],
            "Album": "A Night at the Opera", "AlbumId": ALBUM, "AlbumPrimaryImageTag": "x", "RunTimeTicks": 50_000_000u64, "IndexNumber": 11,
            "ProviderIds": {"MusicBrainzRecording": "b1a9c0e9-d987-4042-ae91-78d6a3267d69"}, "MediaSources": [{"Container": "flac"}]});
        let album = serde_json::json!({"Id": ALBUM, "Name": "A Night at the Opera", "Type": "MusicAlbum", "AlbumArtist": "Queen",
            "ProductionYear": 1975, "ImageTags": {"Primary": "y"}});
        // Every call but the public info and the sign-in carries the token
        fn authed(h: &axum::http::HeaderMap, q: &std::collections::HashMap<String, String>) -> bool {
            h.get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|a| a.contains("Token=\"T1\"") && a.contains("Client=\"Pubky Music\""))
                || q.get("api_key").map(String::as_str) == Some("T1")
        }
        type Q = axum::extract::Query<std::collections::HashMap<String, String>>;
        let (s1, s2, a1) = (song.clone(), song.clone(), album.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route("/System/Info/Public", get(|| async { Json(serde_json::json!({"ServerName": "Rigifin", "Version": "10.11.11", "ProductName": "Jellyfin Server"})) }))
            .route(
                "/Users/AuthenticateByName",
                post(|h: axum::http::HeaderMap, Json(b): Json<serde_json::Value>| async move {
                    assert!(h["authorization"].to_str().unwrap().starts_with("MediaBrowser Client="));
                    if b["Username"] == "owner" && b["Pw"] == "pw" {
                        (StatusCode::OK, Json(serde_json::json!({"AccessToken": "T1", "User": {"Id": "u1", "Name": "owner"}})))
                    } else {
                        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})))
                    }
                }),
            )
            .route(
                "/Items",
                get(move |h: axum::http::HeaderMap, q: Q| {
                    let (s1, a1) = (s1.clone(), a1.clone());
                    async move {
                        assert!(authed(&h, &q));
                        let items = match (q.get("ParentId"), q.get("IncludeItemTypes").map(String::as_str)) {
                            (Some(_), _) => vec![s1],
                            (None, Some("Playlist")) => vec![serde_json::json!({"Id": PL, "Name": "Car", "Type": "Playlist", "MediaType": "Audio", "ChildCount": 2})],
                            _ => vec![s1, a1],
                        };
                        Json(serde_json::json!({"Items": items}))
                    }
                }),
            )
            .route(
                "/Items/{id}",
                get(move |h: axum::http::HeaderMap, q: Q, axum::extract::Path(id): axum::extract::Path<String>| {
                    let (s2, a2) = (s2.clone(), album.clone());
                    async move {
                        assert!(authed(&h, &q));
                        Json(match id.as_str() {
                            ALBUM => a2,
                            PL => serde_json::json!({"Id": PL, "Name": "Car", "Type": "Playlist"}),
                            _ => s2,
                        })
                    }
                }),
            )
            .route(
                "/Playlists/{id}/Items",
                get(move |h: axum::http::HeaderMap, q: Q| {
                    let s3 = song.clone();
                    async move {
                        assert!(authed(&h, &q));
                        Json(serde_json::json!({"Items": [s3, {"Id": "b", "Name": "Untagged", "Type": "Audio", "ProviderIds": {}}]}))
                    }
                }),
            )
            .route(
                "/Audio/{id}/stream",
                get(move |h: axum::http::HeaderMap, q: Q| {
                    let flac = flac.clone();
                    async move {
                        if !authed(&h, &q) {
                            return (StatusCode::UNAUTHORIZED, [(header::CONTENT_TYPE, "text/plain")], Vec::new());
                        }
                        (StatusCode::OK, [(header::CONTENT_TYPE, "audio/flac")], flac)
                    }
                }),
            )
            .route(
                "/Items/{id}/Images/Primary",
                get(|h: axum::http::HeaderMap, q: Q| async move {
                    assert!(authed(&h, &q));
                    ([(header::CONTENT_TYPE, "image/png")], b"\x89PNG".to_vec())
                }),
            )
            .route("/Sessions/Logout", post(|| async { StatusCode::NO_CONTENT }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let url = format!("http://127.0.0.1:{port}/");
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/jellyfin/server",
            Some(serde_json::json!({"url": url, "user": "owner", "password": "nope"})),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, v) = call(
            &app,
            "PUT",
            "/v1/sources/jellyfin/server",
            Some(serde_json::json!({"url": url, "user": "owner", "password": "pw"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["server_name"], "Rigifin");
        {
            let stored = s
                .library
                .lock()
                .unwrap()
                .get_setting("jellyfin.server")
                .unwrap()
                .unwrap();
            assert!(
                stored.contains("T1") && !stored.contains("\"pw\""),
                "the token is kept, the password is not"
            );
        }
        let (st, r) = call(&app, "GET", "/v1/sources/jellyfin/search?q=bohemian", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["items"][0]["kind"], "album");
        assert_eq!(r["items"][1]["uri"], format!("jellyfin:track:{SONG}"));
        assert_eq!(
            r["items"][1]["url"],
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"
        );
        assert!(!r.to_string().contains("T1"));
        let (st, al) = call(
            &app,
            "GET",
            &format!("/v1/sources/jellyfin/items/album:{ALBUM}"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{al}");
        assert_eq!(al["tracks"][0]["format"], "FLAC (Jellyfin)");
        // A zone plays the original file
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        call(&app, "POST", &format!("/v1/zones/{zid}/queue"), Some(serde_json::json!({"items": [{"uri": format!("jellyfin:track:{SONG}"), "title": "x"}], "replace": true, "play": true}))).await;
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
        // Browsers and covers go through the node
        for (path, magic) in [
            (format!("/v1/sources/jellyfin/stream/{SONG}"), &b"fLaC"[..]),
            (format!("/v1/sources/jellyfin/cover/{ALBUM}"), &b"\x89PNG"[..]),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(&resp.into_body().collect().await.unwrap().to_bytes()[..4], magic);
        }
        // Playlists import by MusicBrainz recording
        let (_, pls) = call(&app, "GET", "/v1/sources/jellyfin/playlists", None).await;
        assert_eq!(pls[0]["name"], "Car");
        let (st, imp) = call(
            &app,
            "POST",
            &format!("/v1/sources/jellyfin/playlists/{PL}/import"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{imp}");
        assert_eq!(
            (imp["imported"].as_u64(), imp["skipped"].as_u64()),
            (Some(1), Some(1))
        );
        let (_, v) = call(&app, "DELETE", "/v1/sources/jellyfin/server", None).await;
        assert_eq!(v["connected"], false);
        assert!(s
            .library
            .lock()
            .unwrap()
            .get_setting("jellyfin.server")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn tunnel_settings_go_into_the_shared_folder_and_out_again() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let tun = dir.path().join("tunnel");
        std::fs::create_dir_all(&tun).unwrap();
        s.set_tunnel_dir(tun.clone());
        let app = router(s.clone());
        let tok = base64::engine::general_purpose::STANDARD.encode(
            r#"{"a":"0123456789abcdef","t":"6ff42ae2-765d-4adf-8112-31c55c1551ef","s":"c2VjcmV0c2VjcmV0"}"#,
        );
        let (st, _) = call(&app, "PUT", "/v1/remote/tunnel", Some(serde_json::json!({"token": tok, "homeserver_domain": "hs.example.org", "music_hostname": "hs.example.org"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "two different hostnames");
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/remote/tunnel",
            Some(serde_json::json!({"token": "abc", "homeserver_domain": "hs.example.org"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "not a tunnel token");
        let (st, v) = call(&app, "PUT", "/v1/remote/tunnel", Some(serde_json::json!({"token": tok, "homeserver_domain": "HS.example.org", "music_hostname": "music.example.org"}))).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(
            (
                v["configured"].as_bool(),
                v["homeserver_domain"].as_str(),
                v["music_hostname"].as_str()
            ),
            (Some(true), Some("hs.example.org"), Some("music.example.org"))
        );
        assert!(!v.to_string().contains(&tok), "never the token");
        assert_eq!(std::fs::read_to_string(tun.join("token")).unwrap().trim(), tok);
        assert_eq!(
            std::fs::read_to_string(tun.join("domain")).unwrap().trim(),
            "hs.example.org"
        );
        let (_, v) = call(&app, "DELETE", "/v1/remote/tunnel", None).await;
        assert_eq!(v["configured"], false);
        assert!(!tun.join("token").exists() && !tun.join("domain").exists());
        // Without a tunnel folder the install has no tunnel setup
        let other = tempfile::tempdir().unwrap();
        let s2 = state(other.path(), None).await;
        let (_, v) = call(&router(s2), "GET", "/v1/remote/tunnel", None).await;
        assert_eq!(v["available"], false);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plex_signs_in_with_a_pin_and_keeps_the_token_to_itself() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let song = serde_json::json!({"ratingKey": "1201", "type": "track", "title": "Bohemian Rhapsody",
            "grandparentTitle": "Queen", "parentTitle": "A Night at the Opera", "parentRatingKey": "1200",
            "parentThumb": "/library/metadata/1200/thumb/1", "duration": 5000, "index": 11,
            "Media": [{"audioCodec": "flac", "container": "flac", "Part": [{"key": "/library/parts/9/1/file.flac"}]}]});
        let album = serde_json::json!({"ratingKey": "1200", "type": "album", "title": "A Night at the Opera",
            "parentTitle": "Queen", "year": 1975, "thumb": "/library/metadata/1200/thumb/1"});
        type Q = axum::extract::Query<std::collections::HashMap<String, String>>;
        fn authed(h: &axum::http::HeaderMap, q: &std::collections::HashMap<String, String>) -> bool {
            h.get("x-plex-token").and_then(|v| v.to_str().ok()) == Some("T1")
                || q.get("X-Plex-Token").map(String::as_str) == Some("T1")
        }
        let mc = |m: Vec<serde_json::Value>| Json(serde_json::json!({"MediaContainer": {"Metadata": m}}));
        let pms_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pms_port = pms_l.local_addr().unwrap().port();
        let (s1, s2, a1, a2) = (song.clone(), song.clone(), album.clone(), album.clone());
        let pms = Router::new()
            .route("/identity", get(|| async { Json(serde_json::json!({"MediaContainer": {"machineIdentifier": "m1", "version": "1.41.0"}})) }))
            .route(
                "/",
                get(|h: axum::http::HeaderMap, q: Q| async move {
                    if !authed(&h, &q) {
                        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
                    }
                    (StatusCode::OK, Json(serde_json::json!({"MediaContainer": {"friendlyName": "Rigiplex", "machineIdentifier": "m1", "version": "1.41.0"}})))
                }),
            )
            .route(
                "/hubs/search",
                get(move |h: axum::http::HeaderMap, q: Q| {
                    let (s1, a1) = (s1.clone(), a1.clone());
                    async move {
                        assert!(authed(&h, &q) && q["query"] == "bohemian");
                        Json(serde_json::json!({"MediaContainer": {"Hub": [
                            {"type": "movie", "Metadata": [{"ratingKey": "7", "type": "movie", "title": "Bohemian Rhapsody"}]},
                            {"type": "track", "Metadata": [s1]},
                            {"type": "album", "Metadata": [a1]}]}}))
                    }
                }),
            )
            .route(
                "/library/metadata/{id}",
                get(move |h: axum::http::HeaderMap, q: Q, axum::extract::Path(id): axum::extract::Path<String>| {
                    let (s2, a2) = (s2.clone(), a2.clone());
                    async move {
                        assert!(authed(&h, &q));
                        mc(vec![if id == "1200" { a2 } else { s2 }])
                    }
                }),
            )
            .route(
                "/library/metadata/{id}/children",
                get(move |h: axum::http::HeaderMap, q: Q| {
                    let s3 = song.clone();
                    async move {
                        assert!(authed(&h, &q));
                        mc(vec![s3])
                    }
                }),
            )
            .route(
                "/library/metadata/{id}/thumb/{ver}",
                get(|h: axum::http::HeaderMap, q: Q| async move {
                    assert!(authed(&h, &q));
                    ([(header::CONTENT_TYPE, "image/png")], b"\x89PNG".to_vec())
                }),
            )
            .route(
                "/library/parts/{a}/{b}/{file}",
                get(move |h: axum::http::HeaderMap, q: Q| {
                    let flac = flac.clone();
                    async move {
                        if !authed(&h, &q) {
                            return (StatusCode::UNAUTHORIZED, [(header::CONTENT_TYPE, "text/plain")], Vec::new());
                        }
                        (StatusCode::OK, [(header::CONTENT_TYPE, "audio/flac")], flac)
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(pms_l, pms).await.unwrap() });

        // plex.tv: the PIN is approved on the second poll
        let tv_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tv_port = tv_l.local_addr().unwrap().port();
        let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tv = Router::new()
            .route(
                "/api/v2/pins",
                post(|h: axum::http::HeaderMap, q: Q| async move {
                    assert!(h["x-plex-client-identifier"].to_str().unwrap().starts_with("pubky-music-"));
                    assert_eq!(q["strong"], "true");
                    Json(serde_json::json!({"id": 42, "code": "abcd1234"}))
                }),
            )
            .route(
                "/api/v2/pins/{id}",
                get(move || {
                    let n = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        Json(if n == 0 {
                            serde_json::json!({"id": 42, "authToken": null})
                        } else {
                            serde_json::json!({"id": 42, "authToken": "ACCOUNT"})
                        })
                    }
                }),
            )
            .route("/api/v2/user", get(|| async { Json(serde_json::json!({"username": "owner"})) }))
            .route(
                "/api/v2/resources",
                get(move |h: axum::http::HeaderMap| async move {
                    assert_eq!(h["x-plex-token"], "ACCOUNT");
                    Json(serde_json::json!([
                        {"name": "Phone", "provides": "player", "connections": []},
                        {"name": "Rigiplex", "provides": "server", "owned": true, "accessToken": "T1", "connections": [
                            {"uri": "http://127.0.0.1:1", "local": true, "relay": false},
                            {"uri": format!("http://127.0.0.1:{pms_port}"), "local": false, "relay": false}]}]))
                }),
            );
        tokio::spawn(async move { axum::serve(tv_l, tv).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        s.plex.write().unwrap().tv_base = format!("http://127.0.0.1:{tv_port}");
        let app = router(s.clone());
        // By address: a wrong token is refused
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/plex/server",
            Some(serde_json::json!({"url": format!("http://127.0.0.1:{pms_port}"), "token": "nope"})),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        // Sign in with Plex
        let (st, pin) = call(&app, "POST", "/v1/sources/plex/pin", None).await;
        assert_eq!(st, StatusCode::OK, "{pin}");
        let auth = pin["auth_url"].as_str().unwrap();
        assert!(
            auth.starts_with("https://app.plex.tv/auth#?clientID=pubky-music-")
                && auth.contains("code=abcd1234"),
            "{auth}"
        );
        let (_, w) = call(&app, "GET", "/v1/sources/plex/pin/42", None).await;
        assert_eq!(w["status"], "waiting");
        let (st, c) = call(&app, "GET", "/v1/sources/plex/pin/42", None).await;
        assert_eq!(st, StatusCode::OK, "{c}");
        assert_eq!(c["status"], "connected");
        assert_eq!(c["server"]["server_name"], "Rigiplex");
        assert_eq!(c["server"]["user"], "owner");
        assert_eq!(
            c["server"]["url"],
            format!("http://127.0.0.1:{pms_port}"),
            "the dead local connection is skipped"
        );
        {
            let stored = s
                .library
                .lock()
                .unwrap()
                .get_setting("plex.server")
                .unwrap()
                .unwrap();
            assert!(
                stored.contains("T1") && !stored.contains("ACCOUNT"),
                "the server's token is kept, the account's is not"
            );
        }
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        assert!(srcs.to_string().contains("Your Plex “Rigiplex”"), "{srcs}");
        let (st, r) = call(&app, "GET", "/v1/sources/plex/search?q=bohemian", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["items"].as_array().unwrap().len(), 2, "films are left out: {r}");
        assert_eq!(r["items"][0]["kind"], "album");
        assert_eq!(r["items"][1]["uri"], "plex:track:1201");
        assert!(!r.to_string().contains("T1"));
        let (st, al) = call(&app, "GET", "/v1/sources/plex/items/album:1200", None).await;
        assert_eq!(st, StatusCode::OK, "{al}");
        assert_eq!(
            (al["year"].as_i64(), al["tracks"][0]["format"].as_str()),
            (Some(1975), Some("FLAC (Plex)"))
        );
        // A zone plays the original file
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        call(&app, "POST", &format!("/v1/zones/{zid}/queue"), Some(serde_json::json!({"items": [{"uri": "plex:track:1201", "title": "x"}], "replace": true, "play": true}))).await;
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
        // Browsers and covers go through the node
        for (path, magic) in [
            ("/v1/sources/plex/stream/1201", &b"fLaC"[..]),
            ("/v1/sources/plex/cover/1200/1", &b"\x89PNG"[..]),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            assert_eq!(&resp.into_body().collect().await.unwrap().to_bytes()[..4], magic);
        }
        let (_, v) = call(&app, "DELETE", "/v1/sources/plex/server", None).await;
        assert_eq!(v["connected"], false);
        assert!(s
            .library
            .lock()
            .unwrap()
            .get_setting("plex.server")
            .unwrap()
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audiobookshelf_asks_non_admins_for_a_key_and_ends_the_session() {
        let logouts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let lo = logouts.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route("/status", get(|| async { Json(serde_json::json!({"app": "audiobookshelf", "serverVersion": "2.37.1"})) }))
            .route(
                "/login",
                post(|h: axum::http::HeaderMap| async move {
                    assert_eq!(h["x-return-tokens"], "true");
                    Json(serde_json::json!({"user": {"id": "u2", "username": "kid", "type": "user", "accessToken": "eyJACCESS", "refreshToken": "eyJREFRESH"}}))
                }),
            )
            .route(
                "/logout",
                post(move |h: axum::http::HeaderMap| {
                    let lo = lo.clone();
                    async move {
                        assert_eq!(h["x-refresh-token"], "eyJREFRESH");
                        lo.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        StatusCode::OK
                    }
                }),
            )
            .route(
                "/api/me",
                get(|h: axum::http::HeaderMap| async move {
                    if h["authorization"] != "Bearer KEY1" {
                        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
                    }
                    (StatusCode::OK, Json(serde_json::json!({"username": "kid", "type": "user"})))
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let url = format!("http://127.0.0.1:{port}");
        let (st, v) = call(
            &app,
            "PUT",
            "/v1/sources/audiobookshelf/server",
            Some(serde_json::json!({"url": url, "user": "kid", "password": "pw"})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(v.to_string().contains("API Keys"), "{v}");
        assert_eq!(
            logouts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the session is ended"
        );
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/audiobookshelf/server",
            Some(serde_json::json!({"url": url, "api_key": "WRONG"})),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, v) = call(
            &app,
            "PUT",
            "/v1/sources/audiobookshelf/server",
            Some(serde_json::json!({"url": url, "api_key": "KEY1"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(
            (v["connected"].as_bool(), v["user"].as_str()),
            (Some(true), Some("kid"))
        );
        assert!(!v.to_string().contains("KEY1"));
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        assert!(srcs.to_string().contains("Your audiobooks at"), "{srcs}");
    }

    /// One mock for Google (OAuth, Drive API) and Microsoft (OAuth, Graph, pre-authenticated
    /// downloads). Files: Music/01 Song.flac and Music/Sub/02 Two.flac.
    async fn mock_drives(flac: Vec<u8>, refreshes: Arc<std::sync::atomic::AtomicU32>) -> String {
        use axum::extract::{Path as P, Query as Q};
        use std::collections::HashMap as M;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        fn ranged(h: &axum::http::HeaderMap, body: &[u8]) -> axum::response::Response {
            use axum::response::IntoResponse;
            match h
                .get("range")
                .and_then(|v| v.to_str().ok())
                .and_then(|r| r.strip_prefix("bytes="))
            {
                Some(r) => {
                    let (a, b) = r.split_once('-').unwrap();
                    let a: usize = a.parse().unwrap();
                    let b: usize = b.parse::<usize>().unwrap_or(body.len() - 1).min(body.len() - 1);
                    (
                        StatusCode::PARTIAL_CONTENT,
                        [(header::CONTENT_TYPE, "application/octet-stream")],
                        body[a..=b].to_vec(),
                    )
                        .into_response()
                }
                None => (
                    [(header::CONTENT_TYPE, "application/octet-stream")],
                    body.to_vec(),
                )
                    .into_response(),
            }
        }
        let token = move |body: String, prefix: &'static str, r: Arc<std::sync::atomic::AtomicU32>| {
            let f: M<String, String> = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
            assert!(
                !f.contains_key("client_secret"),
                "a public client sends no secret"
            );
            assert_eq!(f["client_id"], format!("{prefix}-client"));
            match f["grant_type"].as_str() {
                "authorization_code" => {
                    assert_eq!(f["code"], "C0DE");
                    assert!(f["code_verifier"].len() >= 43, "PKCE");
                    // Expires at once, so the next call refreshes
                    Json(
                        serde_json::json!({"access_token": format!("{prefix}-A1"), "refresh_token": format!("{prefix}-R"), "expires_in": 1}),
                    )
                }
                "refresh_token" => {
                    assert_eq!(f["refresh_token"], format!("{prefix}-R"));
                    r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(serde_json::json!({"access_token": format!("{prefix}-A2"), "expires_in": 3600}))
                }
                g => panic!("grant {g}"),
            }
        };
        let auth = |h: &axum::http::HeaderMap, prefix: &str| {
            let a = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
            assert!(a.starts_with(&format!("Bearer {prefix}-A")), "{a}");
        };
        let (r1, r2) = (refreshes.clone(), refreshes);
        let (f1, f2, f3) = (flac.clone(), flac.clone(), flac);
        let b2 = base.clone();
        let app = Router::new()
            .route("/google/token", post(move |body: String| { let r = r1.clone(); async move { token(body, "g", r) } }))
            .route("/ms/token", post(move |body: String| { let r = r2.clone(); async move { token(body, "m", r) } }))
            .route("/gapi/drive/v3/about", get(move |h: axum::http::HeaderMap| async move { auth(&h, "g"); Json(serde_json::json!({"user": {"emailAddress": "me@gmail.example"}})) }))
            .route("/gapi/drive/v3/files", get(move |h: axum::http::HeaderMap, Q(q): Q<M<String, String>>| {
                let n = f1.len();
                async move {
                    auth(&h, "g");
                    let folder = q["q"].split('\'').nth(1).unwrap().to_string();
                    let file = |id: &str, name: &str| serde_json::json!({"id": id, "name": name, "mimeType": "audio/flac", "size": n.to_string(), "modifiedTime": "2026-10-20T10:00:00Z"});
                    let dir = |id: &str, name: &str| serde_json::json!({"id": id, "name": name, "mimeType": "application/vnd.google-apps.folder"});
                    Json(serde_json::json!({"files": match folder.as_str() {
                        "root" => vec![dir("F1", "Music")],
                        "F1" => vec![file("G1", "01 Song.flac"), dir("F2", "Sub")],
                        "F2" => vec![file("G2", "02 Two.flac")],
                        _ => vec![],
                    }}))
                }
            }))
            .route("/gapi/drive/v3/files/{id}", get(move |h: axum::http::HeaderMap, Q(q): Q<M<String, String>>| {
                let f = f2.clone();
                async move { auth(&h, "g"); assert_eq!(q["alt"], "media"); ranged(&h, &f) }
            }))
            .route("/graph/me", get(move |h: axum::http::HeaderMap| async move { auth(&h, "m"); Json(serde_json::json!({"userPrincipalName": "me@outlook.example"})) }))
            .route("/graph/me/drive/items/{id}/children", get(move |h: axum::http::HeaderMap, P(id): P<String>, Q(q): Q<M<String, String>>| {
                let b = b2.clone();
                async move {
                    auth(&h, "m");
                    let file = |id: &str, name: &str| serde_json::json!({"id": id, "name": name, "size": 47782, "file": {}, "lastModifiedDateTime": "2026-10-20T10:00:00Z"});
                    let dir = |id: &str, name: &str| serde_json::json!({"id": id, "name": name, "folder": {"childCount": 1}});
                    Json(match (id.as_str(), q.contains_key("page2")) {
                        ("root", _) => serde_json::json!({"value": [dir("M1", "Music")]}),
                        // Two pages: the second through @odata.nextLink
                        ("M1", false) => serde_json::json!({"value": [file("O1", "01 Song.flac")], "@odata.nextLink": format!("{b}/graph/me/drive/items/M1/children?page2=1")}),
                        ("M1", true) => serde_json::json!({"value": [dir("M2", "Sub")]}),
                        ("M2", _) => serde_json::json!({"value": [file("O2", "02 Two.flac")]}),
                        _ => serde_json::json!({"value": []}),
                    })
                }
            }))
            .route("/graph/me/drive/items/{id}/content", get(move |h: axum::http::HeaderMap, P(id): P<String>| async move {
                auth(&h, "m");
                // OneDrive answers with a pre-authenticated download address
                axum::response::Redirect::temporary(&format!("/dl/{id}"))
            }))
            .route("/dl/{id}", get(move |h: axum::http::HeaderMap| { let f = f3.clone(); async move { ranged(&h, &f) } }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_folders_sign_in_with_pkce_scan_and_play() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let refreshes = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let base = mock_drives(flac.clone(), refreshes.clone()).await;
        for (provider, prefix, scheme) in [("google", "g", "gdrive://"), ("onedrive", "m", "onedrive://")] {
            let dir = tempfile::tempdir().unwrap();
            let s = state(dir.path(), None).await;
            {
                let mut d = s.drives.write().unwrap();
                d.endpoints = drives::Endpoints {
                    google_auth: format!("{base}/google/auth"),
                    google_token: format!("{base}/google/token"),
                    google_api: format!("{base}/gapi"),
                    ms_login: format!("{base}/ms"),
                    ms_graph: format!("{base}/graph"),
                };
            }
            s.set_drive_apps(drives::apps_from(
                drives::App {
                    client_id: "g-client".into(),
                    client_secret: String::new(),
                },
                drives::App {
                    client_id: "m-client".into(),
                    client_secret: String::new(),
                },
            ));
            let app = router(s.clone());
            let (_, v) = call(&app, "GET", "/v1/library/drives", None).await;
            assert!(
                v["providers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|p| p["configured"] == true && p["connected"] == false),
                "{v}"
            );
            // Sign in: PKCE, no secret, through the return page
            let (st, login) = call(
                &app,
                "POST",
                &format!("/v1/library/drives/{provider}/login"),
                None,
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{login}");
            let u = url::Url::parse(login["url"].as_str().unwrap()).unwrap();
            let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
            assert_eq!(q["code_challenge_method"], "S256");
            assert_eq!(q["redirect_uri"], crate::project_apps::RETURN_PAGE);
            assert!(q["state"].contains('.'), "the state carries the node's callback");
            assert!(!q.contains_key("client_secret"));
            let resp = app
                .clone()
                .oneshot(
                    Request::get(format!(
                        "/drives/{provider}/callback?code=C0DE&state={}",
                        q["state"]
                    ))
                    .body(Body::empty())
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                resp.headers()["location"].to_str().unwrap().contains("drive=ok"),
                "{:?}",
                resp.headers()
            );
            let (_, v) = call(&app, "GET", "/v1/library/drives", None).await;
            let pv = v["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == provider)
                .unwrap()
                .clone();
            assert_eq!(pv["connected"], true);
            assert!(pv["user"].as_str().unwrap().starts_with("me@"), "{pv}");
            // Pick the folder and add it
            let (_, folders) = call(
                &app,
                "GET",
                &format!("/v1/library/drives/{provider}/folders"),
                None,
            )
            .await;
            assert_eq!(folders[0]["name"], "Music", "{folders}");
            let (st, root) = call(
                &app,
                "POST",
                "/v1/library/drives",
                Some(
                    serde_json::json!({"provider": provider, "folder_id": folders[0]["id"], "name": "Music"}),
                ),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{root}");
            let mut roots = serde_json::Value::Null;
            for _ in 0..100 {
                let (_, v) = call(&app, "GET", "/v1/library/drives", None).await;
                if v["roots"][0]["tracks"] == 2 {
                    roots = v["roots"].clone();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            assert_eq!(
                roots[0]["tracks"], 2,
                "{provider}: both songs, the one in Sub too"
            );
            let (_, tracks) = call(&app, "GET", "/v1/library/tracks", None).await;
            let t = tracks["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["file"]["path"].as_str().unwrap().ends_with("Sub/02 Two.flac"))
                .unwrap()
                .clone();
            assert!(t["file"]["path"].as_str().unwrap().starts_with(scheme));
            // Played in a zone, and relayed to a browser byte for byte
            let tid = t["id"].as_i64().unwrap();
            let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
            let zid = zones[0]["id"].as_str().unwrap().to_string();
            call(
                &app,
                "POST",
                &format!("/v1/zones/{zid}/queue"),
                Some(serde_json::json!({"track_ids": [tid], "replace": true, "play": true})),
            )
            .await;
            let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
            assert_eq!(zone.wait_stopped().await.error, None, "{provider}");
            let resp = app
                .clone()
                .oneshot(
                    Request::get(format!("/v1/library/tracks/{tid}/file"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.into_body().collect().await.unwrap().to_bytes().to_vec(),
                flac,
                "{provider}"
            );
            assert!(
                !tracks.to_string().contains(&format!("{prefix}-A")),
                "no token in an answer"
            );
            // Out of the library again
            let (_, gone) = call(
                &app,
                "DELETE",
                &format!("/v1/library/drives/roots/{}", root["id"].as_str().unwrap()),
                None,
            )
            .await;
            assert_eq!(gone["tracks"], 2, "{gone}");
        }
        assert!(
            refreshes.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "expired tokens were refreshed"
        );
    }

    #[tokio::test]
    async fn listeners_get_the_library_and_their_zones_not_the_owners_settings() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), Some("s3cret")).await;
        let app = router(s.clone());
        let key = "tbepfp8fisyiqpskbmxsmpub47gxb7xdsh5x9ut9qrb97gcq9phy";
        s.library.lock().unwrap().set_setting("people", &serde_json::json!({"people": [{"key": key, "access": "music", "music_enabled": true, "added_at": 1}]}).to_string()).unwrap();
        let tok = accounts::mint(&s, key).unwrap();
        let send = |method: &'static str, uri: String, token: String, body: Option<serde_json::Value>| {
            let app = app.clone();
            async move {
                let req = Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(
                        body.map(|b| Body::from(b.to_string()))
                            .unwrap_or_else(Body::empty),
                    )
                    .unwrap();
                let r = app.oneshot(req).await.unwrap();
                let st = r.status();
                let b = r.into_body().collect().await.unwrap().to_bytes();
                (
                    st,
                    serde_json::from_slice::<serde_json::Value>(&b).unwrap_or_default(),
                )
            }
        };
        let (_, me) = send("GET", "/v1/me".into(), tok.clone(), None).await;
        assert_eq!(
            (me["role"].as_str(), me["key"].as_str()),
            (Some("listener"), Some(key))
        );
        let (st, _) = send("GET", "/v1/library/tracks".into(), tok.clone(), None).await;
        assert_eq!(st, StatusCode::OK, "the shared library");
        for (m, p) in [
            ("GET", "/v1/people"),
            ("PUT", "/v1/node/settings"),
            ("POST", "/v1/library/folders"),
            ("POST", "/v1/zones"),
            ("POST", "/v1/playlists"),
            ("GET", "/v1/sources/plex/server"),
        ] {
            let (st, v) = send(m, p.into(), tok.clone(), Some(serde_json::json!({}))).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{m} {p}: {v}");
            assert!(v.to_string().contains("Not for listeners"), "{v}");
        }
        // Playing on a zone: yes, until the owner limits listeners to another zone
        let (_, zones) = call(&app, "GET", "/v1/zones?token=s3cret", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, v) = send(
            "POST",
            format!("/v1/zones/{zid}/transport"),
            tok.clone(),
            Some(serde_json::json!({"action": "stop"})),
        )
        .await;
        assert_ne!(st, StatusCode::FORBIDDEN, "{v}");
        let (st, _) = send(
            "PUT",
            "/v1/people/zones".into(),
            "s3cret".into(),
            Some(serde_json::json!({"zones": ["elsewhere"]})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = send(
            "POST",
            format!("/v1/zones/{zid}/transport"),
            tok.clone(),
            Some(serde_json::json!({"action": "stop"})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "a zone the owner kept back");
        // Music access taken away: the token stops at once; a made-up token never works
        s.library.lock().unwrap().set_setting("people", &serde_json::json!({"people": [{"key": key, "access": "music", "music_enabled": false, "added_at": 1}]}).to_string()).unwrap();
        let (st, _) = send("GET", "/v1/library/tracks".into(), tok.clone(), None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _) = send("GET", "/v1/library/tracks".into(), "pml_madeup".into(), None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        // The owner's token is the owner
        let (_, me) = send("GET", "/v1/me".into(), "s3cret".into(), None).await;
        assert_eq!(me["role"], "owner");
    }

    #[tokio::test]
    async fn a_source_turned_off_answers_404_on_its_own_routes() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        for (id, method, path) in [
            ("podcasts", "GET", "/v1/podcasts/subscriptions"),
            ("radio", "GET", "/v1/radio/tags"),
            ("plex", "GET", "/v1/sources/plex/stream/1"),
            ("audiobookshelf", "GET", "/v1/sources/audiobookshelf/continue"),
        ] {
            s.set_source_enabled(id, false).unwrap();
            let (st, v) = call(&app, method, path, None).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{path}: {v}");
            assert!(v.to_string().contains("turned off on Sources"), "{path}: {v}");
            s.set_source_enabled(id, true).unwrap();
            if id == "radio" {
                continue; // on, it asks radio-browser.info: no network in unit tests
            }
            let (st, _) = call(&app, method, path, None).await;
            assert_ne!(st, StatusCode::NOT_FOUND, "{path} works again when on");
        }
        // Settings stay readable while off, so the tab can show (and clear) a connection
        s.set_source_enabled("plex", false).unwrap();
        let (st, _) = call(&app, "GET", "/v1/sources/plex/server", None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jamendo_searches_plays_lossless_and_keeps_the_app_id_to_itself() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let track = move |fmt: &str| {
            serde_json::json!({"id": "1369762", "name": "Trifle 9", "duration": 5, "artist_name": "Vladimir",
                "album_name": "Trifles", "album_id": "164070", "releasedate": "2016-08-20", "position": 1,
                "image": "https://usercontent.jamendo.com/x.jpg",
                "audio": format!("{base}/stream?trackid=1369762&format={fmt}&from=app-SECRETID"),
                "license_ccurl": "http://creativecommons.org/licenses/by/3.0/", "shareurl": "https://www.jamendo.com/track/1369762"})
        };
        type Q = axum::extract::Query<std::collections::HashMap<String, String>>;
        let mock = Router::new()
            .route(
                "/tracks/",
                get(move |q: Q| {
                    let t = track(q.get("audioformat").map(String::as_str).unwrap_or("mp31"));
                    async move {
                        assert_eq!(q.get("client_id").map(String::as_str), Some("SECRETID"));
                        Json(serde_json::json!({"headers": {"status": "success"}, "results": [t]}))
                    }
                }),
            )
            .route(
                "/albums/",
                get(|| async {
                    Json(serde_json::json!({"headers": {"status": "success"}, "results": [
                        {"id": "164070", "name": "Trifles", "artist_name": "Vladimir", "releasedate": "2016-08-20", "image": "https://usercontent.jamendo.com/a.jpg"}]}))
                }),
            )
            .route(
                "/stream",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            )
            .route(
                "/oauth/grant",
                post(|body: String| async move {
                    let form: std::collections::HashMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
                    assert_eq!(form["client_secret"], "SECRETKEY");
                    let ok = match form["grant_type"].as_str() {
                        "authorization_code" => form["code"] == "C0DE" && form["redirect_uri"].ends_with("/jamendo/callback"),
                        "refresh_token" => form["refresh_token"] == "R1",
                        _ => false,
                    };
                    assert!(ok, "{form:?}");
                    let token = if form["grant_type"] == "refresh_token" { "A2" } else { "A1" };
                    Json(serde_json::json!({"access_token": token, "expires_in": 7200, "token_type": "bearer", "scope": "music", "refresh_token": "R1"}))
                }),
            )
            .route(
                "/users/",
                get(|| async { Json(serde_json::json!({"headers": {"status": "success"}, "results": [{"id": "9", "name": "owner", "dispname": "The Owner"}]})) }),
            )
            .route(
                "/users/tracks/",
                get(|q: Q| async move {
                    assert_eq!(q.get("access_token").map(String::as_str), Some("A2"), "the renewed token");
                    assert_eq!(q.get("relation").map(String::as_str), Some("like favorite"));
                    Json(serde_json::json!({"headers": {"status": "success"}, "results": [{"id": "9", "tracks": [{"id": "1369762"}, {"id": "1427344"}, {"id": "1369762"}]}]}))
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        {
            let mut j = s.jamendo.write().unwrap();
            j.base = format!("http://127.0.0.1:{port}");
            j.creds = crate::jamendo::JamendoCredentials {
                client_id: "SECRETID".into(),
                client_secret: "SECRETKEY".into(),
            };
        }
        let app = router(s.clone());
        let (_, sources) = call(&app, "GET", "/v1/sources", None).await;
        assert!(sources
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == "jamendo" && x["status"] == "ready"));
        let (st, r) = call(&app, "GET", "/v1/sources/jamendo/search?q=trifle", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["items"][0]["kind"], "album");
        assert_eq!(r["items"][1]["uri"], "jamendo:track:1369762");
        let (st, al) = call(&app, "GET", "/v1/sources/jamendo/items/album:164070", None).await;
        assert_eq!(st, StatusCode::OK, "{al}");
        assert_eq!(al["tracks"][0]["url"], "jamendo:track:1369762");
        assert_eq!(al["tracks"][0]["format"], "FLAC (Jamendo)");
        for body in [&r, &al] {
            assert!(
                !body.to_string().contains("SECRET"),
                "the app id never leaves the node: {body}"
            );
        }
        // A zone plays it (the lossless stream, looked up at play time)
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": "jamendo:track:1369762", "title": "Trifle 9"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
        // Browsers get it relayed by the node
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::get("/v1/sources/jamendo/stream/1369762")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get("location").is_none(),
            "relayed, not redirected"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..4], b"fLaC");
        // A Jamendo link in a playlist plays from Jamendo
        let (_, pl) = call(
            &app,
            "POST",
            "/v1/playlists",
            Some(serde_json::json!({"name": "CC", "uris": ["https://www.jamendo.com/track/1369762"]})),
        )
        .await;
        let id = pl["summary"]["id"].as_str().unwrap().to_string();
        let (_, pf) = call(&app, "GET", &format!("/v1/playlists/{id}/play-from"), None).await;
        assert_eq!(pf["items"][0]["plays_from"]["service"], "jamendo", "{pf}");
        assert_eq!(pf["items"][0]["stream_uri"], "jamendo:track:1369762");
        assert!(!pf.to_string().contains("SECRET"));

        // Sign in (OAuth, scope music), import likes and favourites, sign out
        let (_, st0) = call(&app, "GET", "/v1/sources/jamendo/status", None).await;
        assert_eq!(st0["signed_in"], false);
        let (st, l) = call(&app, "POST", "/v1/sources/jamendo/login", None).await;
        assert_eq!(st, StatusCode::OK, "{l}");
        let url = url::Url::parse(l["url"].as_str().unwrap()).unwrap();
        assert!(url.path().ends_with("/oauth/authorize"));
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], "music");
        assert!(q["redirect_uri"].ends_with("/jamendo/callback"));
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::get(format!("/jamendo/callback?code=C0DE&state={}", q["state"]))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.headers()["location"], "/source/jamendo?jamendo=signed_in");
        let (_, st1) = call(&app, "GET", "/v1/sources/jamendo/status", None).await;
        assert_eq!(st1["signed_in"], true);
        assert_eq!(st1["name"], "The Owner");
        // The access token has expired: the import renews it first
        {
            let lib = s.library.lock().unwrap();
            let mut u: serde_json::Value =
                serde_json::from_str(&lib.get_setting("jamendo.user").unwrap().unwrap()).unwrap();
            u["expires_at"] = serde_json::json!(0);
            lib.set_setting("jamendo.user", &u.to_string()).unwrap();
        }
        let (st, r) = call(&app, "POST", "/v1/sources/jamendo/collection/import", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["imported"], 2, "two tracks, one listed twice");
        let (_, favs) = call(&app, "GET", "/v1/bookmarks", None).await;
        assert!(
            favs.to_string().contains("https://www.jamendo.com/track/1427344"),
            "{favs}"
        );
        let (_, st2) = call(&app, "POST", "/v1/sources/jamendo/logout", None).await;
        assert_eq!(st2["signed_in"], false);
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::get("/jamendo/callback?code=C0DE&state=unknown")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.headers()["location"]
            .to_str()
            .unwrap()
            .starts_with("/source/jamendo?jamendo=error"));
    }

    #[tokio::test]
    async fn audius_source_searches_and_plays() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let track = serde_json::json!({"id": "W0NJ0", "title": "Ambient Piano Interlude", "user": {"name": "Techwipe"}, "duration": 5,
            "permalink": "/techwipe/ambient-piano-interlude-353852", "artwork": {"480x480": "https://img/480"}, "play_count": 3});
        let t1 = track.clone();
        let t2 = track.clone();
        let mock = Router::new()
            .route(
                "/v1/tracks/search",
                get(move || {
                    let t = t1.clone();
                    async move { Json(serde_json::json!({"data": [t]})) }
                }),
            )
            .route(
                "/v1/tracks/{id}",
                get(move || {
                    let t = t2.clone();
                    async move { Json(serde_json::json!({"data": t})) }
                }),
            )
            .route(
                "/v1/tracks/{id}/stream",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        *s.audius_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (_, sources) = call(&app, "GET", "/v1/sources", None).await;
        assert!(sources.as_array().unwrap().iter().any(|x| x["id"] == "audius"));
        let (st, _) = call(&app, "GET", "/v1/sources/nope/search?q=x", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, r) = call(&app, "GET", "/v1/sources/audius/search?q=ambient", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["source"], "audius");
        assert_eq!(r["items"][0]["kind"], "track");
        assert_eq!(
            r["items"][0]["url"],
            "https://audius.co/techwipe/ambient-piano-interlude-353852"
        );
        let (st, it) = call(&app, "GET", "/v1/sources/audius/items/W0NJ0", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        let url = it["tracks"][0]["url"].as_str().unwrap().to_string();
        assert!(
            url.ends_with("/v1/tracks/W0NJ0/stream?app_name=pubky-music-node"),
            "{url}"
        );
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": url, "title": "Ambient Piano Interlude"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
    }

    /// Deezer against a local mock: artists, albums and tracks come back from
    /// one search with ISRC/UPC, an album item lists its tracks, versions of a
    /// library track/album are found by ISRC/UPC, the browser proxy redirects
    /// to the preview, and a `deezer:track:` URI plays through the zone.
    #[tokio::test]
    async fn deezer_source_searches_matches_and_plays_previews() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let preview = format!("http://127.0.0.1:{port}/preview.mp3");
        let track = serde_json::json!({"id": 138547415, "title": "Creep", "isrc": "GBAYE9200070", "duration": 238, "rank": 9,
            "preview": preview, "artist": {"id": 399, "name": "Radiohead"},
            "album": {"id": 12047952, "title": "Pablo Honey", "cover_medium": "http://img/pablo.jpg", "release_date": "1993-02-22"}});
        let album = serde_json::json!({"id": 12047952, "title": "Pablo Honey", "upc": "724381230321", "release_date": "1993-02-22", "label": "Parlophone",
            "cover_big": "http://img/pablo-big.jpg", "artist": {"id": 399, "name": "Radiohead"}, "tracks": {"data": [track]}});
        let artist = serde_json::json!({"id": 399, "name": "Radiohead", "nb_fan": 1000, "picture_big": "http://img/rh.jpg"});
        let (t1, t2, t3, a1, a2, a3, a4, ar1, ar2, ar3) = (
            track.clone(),
            track.clone(),
            track.clone(),
            album.clone(),
            album.clone(),
            album.clone(),
            album.clone(),
            artist.clone(),
            artist.clone(),
            artist.clone(),
        );
        let mock = Router::new()
            .route(
                "/search/track",
                get(move || {
                    let t = t1.clone();
                    async move { Json(serde_json::json!({"data": [t]})) }
                }),
            )
            .route(
                "/search/album",
                get(move || {
                    let a = a1.clone();
                    async move { Json(serde_json::json!({"data": [a]})) }
                }),
            )
            .route(
                "/search/artist",
                get(move || {
                    let a = ar1.clone();
                    async move { Json(serde_json::json!({"data": [a]})) }
                }),
            )
            .route(
                "/album/{id}",
                get(move |Path(id): Path<String>| {
                    let a = a2.clone();
                    async move {
                        if id == "12047952" || id == "upc:724381230321" {
                            Json(a)
                        } else {
                            Json(serde_json::json!({"error": {"code": 800, "message": "no data"}}))
                        }
                    }
                }),
            )
            .route(
                "/track/{id}",
                get(move |Path(id): Path<String>| {
                    let t = t2.clone();
                    async move {
                        if id == "138547415" || id == "isrc:GBAYE9200070" {
                            Json(t)
                        } else {
                            Json(serde_json::json!({"error": {"code": 800, "message": "no data"}}))
                        }
                    }
                }),
            )
            .route(
                "/artist/{id}",
                get(move || {
                    let a = ar2.clone();
                    async move { Json(a) }
                }),
            )
            .route(
                "/artist/{id}/albums",
                get(move || {
                    let a = a3.clone();
                    async move { Json(serde_json::json!({"data": [a]})) }
                }),
            )
            .route(
                "/preview.mp3",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            );
        let _ = (t3, a4, ar3);
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        *s.deezer_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (st, r) = call(&app, "GET", "/v1/sources/deezer/search?q=creep", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let kinds: Vec<&str> = r["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["artist", "album", "track"]);
        assert_eq!(r["items"][2]["uri"], "deezer:track:138547415");
        assert_eq!(r["items"][2]["isrc"], "GBAYE9200070");
        assert_eq!(r["items"][2]["duration_ms"], 30_000, "the preview length");
        assert_eq!(r["items"][1]["upc"], "724381230321");
        let (st, it) = call(&app, "GET", "/v1/sources/deezer/items/album:12047952", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(it["tracks"][0]["url"], "deezer:track:138547415");
        assert_eq!(it["image"], "http://img/pablo-big.jpg");
        let (st, _) = call(&app, "GET", "/v1/sources/deezer/items/album:1", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, ar) = call(&app, "GET", "/v1/sources/deezer/artists/artist:399", None).await;
        assert_eq!(st, StatusCode::OK, "{ar}");
        assert_eq!(ar["albums"][0]["id"], "album:12047952");
        // The unified search shows Deezer without any configuration.
        let (st, u) = call(&app, "GET", "/v1/library/search?q=creep", None).await;
        assert_eq!(st, StatusCode::OK, "{u}");
        assert!(
            u["sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["source"] == "deezer"),
            "{u}"
        );
        // Versions of a library track/album with the same ISRC/UPC.
        let tid = {
            let lib = s.library.lock().unwrap();
            let t = pm_model::Track {
                id: None,
                title: "Creep".into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: "Radiohead".into(),
                    mbid: None,
                }],
                album: Some(pm_model::Album {
                    id: None,
                    title: "Pablo Honey".into(),
                    album_artist: Some("Radiohead".into()),
                    release_mbid: None,
                    release_group_mbid: None,
                    year: Some(1993),
                    upc: Some(pm_model::Upc::parse("724381230321").unwrap()),
                    total_discs: None,
                }),
                track_no: Some(2),
                disc_no: None,
                duration_ms: Some(238_000),
                file: pm_model::AudioFile {
                    path: dir.path().join("creep.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(238_000),
                },
                recording_mbid: None,
                isrc: Some(pm_model::Isrc::parse("GBAYE9200070").unwrap()),
                acoustid: None,
                canonical: None,
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            lib.upsert_track(&t).unwrap().0
        };
        let (st, v) = call(&app, "GET", &format!("/v1/library/tracks/{tid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v[0]["source"], "deezer");
        assert_eq!(v[0]["uri"], "deezer:track:138547415");
        let (_, t) = call(&app, "GET", &format!("/v1/library/tracks/{tid}"), None).await;
        let aid = t["album"]["id"].as_i64().unwrap();
        let (st, v) = call(&app, "GET", &format!("/v1/library/albums/{aid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v[0]["id"], "album:12047952");
        // Browser proxy: a redirect to the preview.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/sources/deezer/stream/138547415")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            resp.headers()["location"],
            format!("http://127.0.0.1:{port}/preview.mp3")
        );
        // And the zone plays the URI itself.
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": "deezer:track:138547415", "title": "Creep"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
    }

    /// Library radio: with the flag on, a zone that runs out of queue gets
    /// similar tracks appended and keeps playing.
    #[tokio::test]
    async fn backup_downloads_a_backup_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/v1/backup")
            .header("authorization", "Bearer t")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let disposition = resp
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(disposition.contains("pubky-music-"), "{disposition}");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        // A backup file (zip): its library is a plain SQLite database
        let parsed = crate::backup::parse(&body).unwrap();
        assert!(parsed.database.starts_with(b"SQLite format 3\0"));
        let copy = dir.path().join("copy.db");
        std::fs::write(&copy, &parsed.database).unwrap();
        assert!(pm_library::Library::open(&copy).is_ok());
    }

    #[tokio::test]
    async fn lyrics_come_from_a_sidecar_lrc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/song.wav"), 4410);
        std::fs::write(
            dir.path().join("music/song.lrc"),
            "[ti:Song]\n[00:01.00]Hello\n[00:02.50]World\n",
        )
        .unwrap();
        write_wav(&dir.path().join("music/mute.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        let id_of = |name: &str| {
            tracks["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["file"]["path"].as_str().unwrap().ends_with(name))
                .map(|t| t["id"].as_i64().unwrap())
                .unwrap()
        };
        let (st, l) = call(
            &app,
            "GET",
            &format!("/v1/library/tracks/{}/lyrics", id_of("song.wav")),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{l}");
        assert_eq!(l["source"], "lrc");
        assert_eq!(l["synced"], true);
        assert_eq!(l["lines"].as_array().unwrap().len(), 2);
        assert_eq!(l["lines"][1]["ms"], 2500);
        assert_eq!(l["lines"][1]["text"], "World");
        let (st, _) = call(
            &app,
            "GET",
            &format!("/v1/library/tracks/{}/lyrics", id_of("mute.wav")),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_queue_can_be_saved_as_a_playlist() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("untagged.wav");
        write_wav(&wav, 441);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let archive = "https://archive.org/download/gd1977-05-08/gd77-05-08d1t01.flac";
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [
                {"uri": archive, "title": "Scarlet Begonias"},
                {"uri": wav.to_string_lossy(), "title": "Untagged"},
            ], "replace": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, r) = call(
            &app,
            "POST",
            "/v1/playlists/from-queue",
            Some(serde_json::json!({"zone": zid})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert!(r["playlist"]["summary"]["name"]
            .as_str()
            .unwrap()
            .starts_with("Queue · "));
        assert_eq!(r["playlist"]["playlist"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(r["playlist"]["playlist"]["items"][0]["uri"], archive);
        assert_eq!(r["skipped"], serde_json::json!(["Untagged"]));
        // Explicit URIs (the browser's queue) and a name of one's own.
        let (st, r) = call(
            &app,
            "POST",
            "/v1/playlists/from-queue",
            Some(serde_json::json!({"uris": [archive, archive], "name": "Twice"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["playlist"]["summary"]["name"], "Twice");
        assert_eq!(r["playlist"]["playlist"]["items"].as_array().unwrap().len(), 2);
        // Nothing canonical: 422, no playlist.
        let (st, _) = call(
            &app,
            "POST",
            "/v1/playlists/from-queue",
            Some(serde_json::json!({"uris": [wav.to_string_lossy()]})),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (_, pls) = call(&app, "GET", "/v1/playlists", None).await;
        assert_eq!(pls.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn new_releases_come_from_musicbrainz_for_tagged_artists() {
        let artist_mbid = "056e4f3e-d505-4dad-8ec1-d04f521cbb56";
        let fresh_rg = "11111111-2222-3333-4444-555555555555";
        let owned_rg = "66666666-7777-8888-9999-000000000000";
        let today = time::OffsetDateTime::now_utc().date();
        let recent = (today - time::Duration::days(30)).to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        let mock = Router::new().route(
            "/ws/2/release-group",
            get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
                let hits = hits2.clone();
                let recent = recent.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(q.get("artist").map(String::as_str), Some(artist_mbid));
                    Json(serde_json::json!({"release-group-count": 4, "release-group-offset": 0, "release-groups": [
                        {"id": fresh_rg, "title": "Brand New", "first-release-date": recent, "primary-type": "Album", "secondary-types": []},
                        {"id": owned_rg, "title": "Already Here", "first-release-date": recent, "primary-type": "EP", "secondary-types": []},
                        {"id": "aaaaaaaa-0000-0000-0000-000000000000", "title": "Old", "first-release-date": "2001-05-01", "primary-type": "Album", "secondary-types": []},
                        {"id": "bbbbbbbb-0000-0000-0000-000000000000", "title": "Live", "first-release-date": recent, "primary-type": "Album", "secondary-types": ["Live"]}
                    ]}))
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        s.info_bases.write().unwrap().musicbrainz = format!("http://127.0.0.1:{port}");
        s.set_contact("tests@example.org").ok();
        {
            let lib = s.library.lock().unwrap();
            let mut t = pm_model::Track {
                id: None,
                title: "Song".into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: "Daft Punk".into(),
                    mbid: Some(pm_model::Mbid::parse(artist_mbid).unwrap()),
                }],
                album: Some(pm_model::Album {
                    id: None,
                    title: "Already Here".into(),
                    album_artist: Some("Daft Punk".into()),
                    release_mbid: None,
                    release_group_mbid: Some(pm_model::Mbid::parse(owned_rg).unwrap()),
                    year: Some(2026),
                    upc: None,
                    total_discs: None,
                }),
                track_no: Some(1),
                disc_no: None,
                duration_ms: Some(1000),
                file: pm_model::AudioFile {
                    path: dir.path().join("song.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(1000),
                },
                recording_mbid: None,
                isrc: None,
                acoustid: None,
                canonical: None,
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            t.id = None;
            lib.upsert_track(&t).unwrap();
        }
        let app = router(s.clone());
        // First call: nothing cached yet, a refresh starts in the background.
        let (st, p) = call(&app, "GET", "/v1/library/new-releases", None).await;
        assert_eq!(st, StatusCode::OK, "{p}");
        assert_eq!(p["artists_total"], 1);
        let mut page = p;
        for _ in 0..300 {
            if page["artists_checked"] == 1 && page["refreshing"] == false {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            page = call(&app, "GET", "/v1/library/new-releases", None).await.1;
        }
        assert_eq!(page["artists_checked"], 1, "{page}");
        let items = page["items"].as_array().unwrap();
        let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
        assert_eq!(
            titles,
            ["Brand New", "Already Here"],
            "old and live records are left out"
        );
        assert!(items[0].get("in_library").is_none());
        assert_eq!(items[1]["in_library"]["title"], "Already Here");
        assert_eq!(
            items[0]["url"],
            format!("https://musicbrainz.org/release-group/{fresh_rg}")
        );
        // Cached for a week: another page read does not hit MusicBrainz again.
        let before = hits.load(std::sync::atomic::Ordering::SeqCst);
        call(&app, "GET", "/v1/library/new-releases", None).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), before);
    }

    #[tokio::test]
    async fn a_volume_limit_caps_the_dsp_volume_and_requests() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, _) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/dsp"),
            Some(serde_json::json!({"max_volume_db": 3.0})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, cfg) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/dsp"),
            Some(serde_json::json!({"max_volume_db": -20.0, "volume_db": -6.0})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{cfg}");
        assert_eq!(cfg["volume_db"], -20.0, "software volume is capped at the limit");
        let (_, cfg) = call(&app, "GET", &format!("/v1/zones/{zid}/dsp"), None).await;
        assert_eq!(cfg["max_volume_db"], -20.0);
        // Requests are capped before they reach the output.
        use pm_model::Volume;
        assert_eq!(cap_volume(Volume::Db(-3.0), Some(-20.0)), Volume::Db(-20.0));
        assert_eq!(cap_volume(Volume::Db(-30.0), Some(-20.0)), Volume::Db(-30.0));
        assert_eq!(cap_volume(Volume::Db(-3.0), None), Volume::Db(-3.0));
        assert_eq!(cap_volume(Volume::Up, Some(-20.0)), Volume::Up);
        // The null output has no volume control at all: the request is refused as before.
        let (st, body) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/volume"),
            Some(serde_json::json!({"volume": {"db": -3.0}})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    }

    /// A renderer on another subnet is added by its description URL, becomes
    /// An endpoint added by address (the desktop app's player, a Pi on
    /// another subnet) gets a zone per output, is remembered for the next
    /// start, and leaves nothing behind when forgotten.
    #[tokio::test]
    async fn an_endpoint_can_be_added_by_address() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let key = pm_stream::Key::random();
        let ep = pm_endpoint::Endpoint::new("Gil's laptop", pm_endpoint::local_outputs(), key.clone());
        std::thread::spawn(move || ep.serve(listener));
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (st, outs) = call(
            &app,
            "POST",
            "/v1/outputs/endpoints",
            Some(serde_json::json!({"address": address, "key": key.expose()})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{outs}");
        let outs = outs.as_array().unwrap().clone();
        assert!(!outs.is_empty());
        assert!(outs.iter().all(|o| o["protocol"] == "net"), "{outs:?}");
        assert!(outs[0]["name"].as_str().unwrap().starts_with("Gil's laptop · "));
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        assert!(
            zones
                .as_array()
                .unwrap()
                .iter()
                .any(|z| z["output"] == outs[0]["id"]),
            "{zones}"
        );
        let (_, rep) = call(&app, "GET", "/v1/outputs/discovery", None).await;
        assert_eq!(rep["saved_endpoints"][0], address);
        // Adding again (the app restarted) changes nothing.
        let (st, again) = call(
            &app,
            "POST",
            "/v1/outputs/endpoints",
            Some(serde_json::json!({"address": address, "key": key.expose()})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(again.as_array().unwrap().len(), outs.len());
        let (_, zones2) = call(&app, "GET", "/v1/zones", None).await;
        assert_eq!(zones2.as_array().unwrap().len(), zones.as_array().unwrap().len());
        let (st, e) = call(
            &app,
            "POST",
            "/v1/outputs/endpoints",
            Some(serde_json::json!({"address": "127.0.0.1:1", "key": key.expose()})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
        let enc = address.replace(':', "%3A");
        let (st, left) = call(&app, "DELETE", &format!("/v1/outputs/endpoints/{enc}"), None).await;
        assert_eq!(st, StatusCode::OK, "{left}");
        assert!(left.as_array().unwrap().is_empty());
        let (_, zones3) = call(&app, "GET", "/v1/zones", None).await;
        assert!(zones3
            .as_array()
            .unwrap()
            .iter()
            .all(|z| z["output"] != outs[0]["id"]));
        let (_, outs3) = call(&app, "GET", "/v1/outputs", None).await;
        assert!(outs3.as_array().unwrap().iter().all(|o| o["protocol"] != "net"));
    }

    /// The player's key: the node stores it per address in a
    /// private file, replaces it when the app registers again with a new one, uses it after a
    /// restart, and never shows it; a missing or wrong key is refused with a reason.
    #[tokio::test]
    async fn an_endpoint_key_is_kept_per_address_and_never_shown() {
        fn endpoint(key: &pm_stream::Key, l: std::net::TcpListener) -> pm_endpoint::Serving {
            let outs = pm_zones::OutputRegistry::new();
            outs.register(Arc::new(pm_zones::outputs::NullOutput::new()));
            pm_endpoint::Endpoint::new("Laptop", outs, key.clone())
                .spawn(l)
                .unwrap()
        }
        let first = pm_stream::Key::random();
        let serving = endpoint(&first, std::net::TcpListener::bind("127.0.0.1:0").unwrap());
        let address = serving.addr().to_string();
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let register = |key: serde_json::Value| {
            let (app, address) = (app.clone(), address.clone());
            async move {
                call(
                    &app,
                    "POST",
                    "/v1/outputs/endpoints",
                    Some(serde_json::json!({"address": address, "key": key})),
                )
                .await
            }
        };
        let stored = || std::fs::read_to_string(dir.path().join("endpoint-keys.json")).unwrap_or_default();
        let (st, e) = call(
            &app,
            "POST",
            "/v1/outputs/endpoints",
            Some(serde_json::json!({"address": address})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(e.to_string().contains("update the Pubky Music app"), "{e}");
        let (st, e) = register(serde_json::json!(pm_stream::Key::random().expose())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(e.to_string().contains("wrong key"), "{e}");
        assert!(stored().is_empty(), "a key that did not work is not kept");

        let (st, outs) = register(serde_json::json!(first.expose())).await;
        assert_eq!(st, StatusCode::OK, "{outs}");
        let out_id = outs[0]["id"].as_str().unwrap().to_string();
        assert!(stored().contains(first.expose()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("endpoint-keys.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        for path in ["/v1/outputs", "/v1/zones", "/v1/outputs/discovery?quick=1"] {
            let (_, body) = call(&app, "GET", path, None).await;
            assert!(!body.to_string().contains(first.expose()), "{path} shows the key");
        }
        assert!(!outs.to_string().contains(first.expose()));

        // The app starts again: the same address, a new key. The zone it had plays with it.
        drop(serving);
        let second = pm_stream::Key::random();
        let _serving = endpoint(&second, std::net::TcpListener::bind(&address).unwrap());
        let (st, again) = register(serde_json::json!(second.expose())).await;
        assert_eq!(st, StatusCode::OK, "{again}");
        assert!(stored().contains(second.expose()) && !stored().contains(first.expose()));
        let out = s.outputs.get(&OutputId(out_id.clone())).unwrap();
        let fmt = pm_model::AudioFormat::pcm(48_000, 16, 2);
        drop(out.open(fmt).await.unwrap());

        // The node restarts: the stored key reaches the player
        let s2 = state(dir.path(), None).await;
        assert!(s2.outputs.get(&OutputId(out_id.clone())).is_none());
        let st2 = s2.clone();
        let found = tokio::task::spawn_blocking(move || st2.restore_endpoint_addresses())
            .await
            .unwrap();
        assert_eq!(found, vec![OutputId(out_id.clone())]);

        // Forgotten: the key goes too
        let enc = address.replace(':', "%3A");
        let (st, _) = call(&app, "DELETE", &format!("/v1/outputs/endpoints/{enc}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(!stored().contains(second.expose()));
    }

    /// an output and a zone, is remembered for the next start, and can be
    /// forgotten again.
    #[tokio::test]
    async fn a_renderer_can_be_added_by_address() {
        let desc = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Living Room DSM</friendlyName>
    <manufacturer>Linn Products Ltd.</manufacturer>
    <UDN>uuid:4c494e4e-0026-0f21-cc3f-01234567890a</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/AVTransport/control</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new().route(
            "/desc.xml",
            get(move || async move { ([(header::CONTENT_TYPE, "text/xml")], desc) }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let address = format!("http://127.0.0.1:{port}/desc.xml");
        let (st, outs) = call(
            &app,
            "POST",
            "/v1/outputs/upnp",
            Some(serde_json::json!({"address": address})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{outs}");
        assert!(
            outs.as_array()
                .unwrap()
                .iter()
                .any(|o| o["name"] == "Living Room DSM"),
            "{outs}"
        );
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        assert!(zones
            .as_array()
            .unwrap()
            .iter()
            .any(|z| z["name"] == "Living Room DSM"));
        let (_, rep) = call(&app, "GET", "/v1/outputs/discovery", None).await;
        assert_eq!(rep["saved_addresses"][0], address);
        // Adding again is idempotent; a dead address is a 400 with the reason.
        let (st, _) = call(
            &app,
            "POST",
            "/v1/outputs/upnp",
            Some(serde_json::json!({"address": address})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, e) = call(
            &app,
            "POST",
            "/v1/outputs/upnp",
            Some(serde_json::json!({"address": "127.0.0.1:1"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
        assert!(e["error"].as_str().unwrap().contains("did not answer"), "{e}");
        let enc = address.replace('/', "%2F").replace(':', "%3A");
        let (st, left) = call(&app, "DELETE", &format!("/v1/outputs/upnp/{enc}"), None).await;
        assert_eq!(st, StatusCode::OK, "{left}");
        assert_eq!(left.as_array().unwrap().len(), 0);
    }

    /// `GET /v1/discover/play` resolves through the node's connected
    /// services: with nothing connected it names the services that have the
    /// song (and offers a preview); once the library owns the recording it
    /// plays from there.
    #[tokio::test]
    async fn play_plan_resolves_through_connected_services() {
        let fixture = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-pubky/tests/fixtures/pinakes/music/detail-tidal-track-bohemian.json"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new().route(
            "/v1/resources/by-uri",
            get(move || {
                let fixture = fixture.clone();
                async move { ([(header::CONTENT_TYPE, "application/json")], fixture) }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        let app = router(s.clone());
        let q = "/v1/discover/play?uri=https%3A%2F%2Ftidal.com%2Ftrack%2F534050211";
        let (st, plan) = call(&app, "GET", q, None).await;
        assert_eq!(st, StatusCode::OK, "{plan}");
        assert!(plan["plays_from"].is_null(), "nothing is connected: {plan}");
        let connect: Vec<&str> = plan["connect"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            connect.contains(&"tidal") && connect.contains(&"spotify"),
            "{plan}"
        );
        assert_eq!(plan["preview"]["service"], "deezer");
        assert_eq!(plan["preview"]["mode"], "preview");
        assert!(plan["items"].is_null() || plan["items"].as_array().unwrap().is_empty());
        // The item page says the same, without expanding anything.
        let (st, d) = call(
            &app,
            "GET",
            "/v1/discover/item?uri=https%3A%2F%2Ftidal.com%2Ftrack%2F534050211",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{d}");
        assert!(d["item"]["plays_from"].is_null());
        assert_eq!(d["item"]["connect"][0], "tidal");
        assert_eq!(d["item"]["page"], "https://tidal.com/track/534050211");
        // Own the recording: the library wins.
        let tid = {
            let lib = s.library.lock().unwrap();
            let mbid = pm_model::Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap();
            let t = pm_model::Track {
                id: None,
                title: "Bohemian Rhapsody".into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: "Queen".into(),
                    mbid: None,
                }],
                album: None,
                track_no: Some(11),
                disc_no: None,
                duration_ms: Some(355_000),
                file: pm_model::AudioFile {
                    path: dir.path().join("bo.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(355_000),
                },
                recording_mbid: Some(mbid),
                isrc: None,
                acoustid: None,
                canonical: Some((
                    pm_model::MusicUri::Recording { mbid },
                    pm_model::Confidence::TaggedMbid,
                )),
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            lib.upsert_track(&t).unwrap().0
        };
        let (st, plan) = call(&app, "GET", q, None).await;
        assert_eq!(st, StatusCode::OK, "{plan}");
        assert_eq!(plan["plays_from"]["service"], "library");
        assert_eq!(plan["plays_from"]["mode"], "full");
        assert_eq!(plan["track_ids"][0], tid);
        assert!(plan["connect"].is_null());
    }

    fn tone_wav(path: &std::path::Path, secs: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(secs * 44_100) {
            let t = i as f64 / 44_100.0;
            let f = [261.6, 329.6, 392.0, 440.0][((t * 2.0) as usize) % 4];
            let v = ((t * f * std::f64::consts::TAU).sin() * 12_000.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
    }

    fn untagged_track(path: std::path::PathBuf, title: &str, artist: &str) -> pm_model::Track {
        pm_model::Track {
            id: None,
            title: title.into(),
            artists: vec![pm_model::Artist {
                id: None,
                name: artist.into(),
                mbid: None,
            }],
            album: None,
            track_no: Some(11),
            disc_no: Some(1),
            duration_ms: Some(10_000),
            file: pm_model::AudioFile {
                path,
                size_bytes: 1,
                mtime_unix: 0,
                container: "wav".into(),
                format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                duration_ms: Some(10_000),
            },
            recording_mbid: None,
            isrc: None,
            acoustid: None,
            canonical: None,
            replaygain_track_gain_db: None,
            replaygain_album_gain_db: None,
            genres: vec![],
        }
    }

    /// Identify library: an untagged file is fingerprinted, looked up on
    /// AcoustID once (the answer is cached by file key), and identified;
    /// without a key, MusicBrainz search gives suggestions the owner must
    /// confirm.
    #[tokio::test]
    async fn identify_library_fingerprints_looks_up_and_suggests() {
        let single = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-library/tests/fixtures/acoustid/single.json"
        ))
        .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route(
                "/v2/lookup",
                post(move |body: String| {
                    let single = single.clone();
                    let calls = c2.clone();
                    async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        assert!(body.contains("client=test-key") && body.contains("fingerprint=AQ"), "{body}");
                        ([(header::CONTENT_TYPE, "application/json")], single)
                    }
                }),
            )
            .route(
                "/ws/2/recording",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"recordings":[{"id":"b1a9c0e9-d987-4042-ae91-78d6a3267d69","score":100,"title":"Bohemian Rhapsody"},{"id":"ebf79ba5-085e-48d2-9eb8-2d992fbf0f6d","score":62,"title":"Bohemian Rhapsody"}]}"#,
                    )
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        let a = music.join("11 bohemian.wav");
        tone_wav(&a, 10);
        let s = state(dir.path(), None).await;
        s.set_contact("test@example.org").unwrap();
        *s.acoustid.write().unwrap() = identify::AcoustIdSettings {
            api_key: "test-key".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            configured: String::new(),
        };
        *s.mb_base.write().unwrap() = format!("http://127.0.0.1:{port}/ws/2");
        let tid = s
            .library
            .lock()
            .unwrap()
            .upsert_track(&untagged_track(a.clone(), "Bohemian Rhapsody", "Queen"))
            .unwrap();
        let app = router(s.clone());
        let (_, st) = call(&app, "GET", "/v1/library/identify", None).await;
        assert_eq!(st["counts"]["unchecked"], 1, "{st}");
        assert_eq!(st["acoustid"], true);
        identify::run(&s, false).await.unwrap();
        let t = s.library.lock().unwrap().get_track(tid).unwrap().unwrap();
        let (uri, conf) = t.canonical.clone().expect("identified");
        assert_eq!(
            uri.to_url(),
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"
        );
        assert!(
            matches!(conf, pm_model::Confidence::Fingerprint { .. }),
            "{conf:?}"
        );
        assert_eq!(
            t.acoustid.as_deref(),
            Some("9ff43b6a-4f16-427c-93c2-92307ca505e0")
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The same audio under another name: no second lookup (cached by key).
        let b = music.join("copy.wav");
        std::fs::copy(&a, &b).unwrap();
        let tid2 = s
            .library
            .lock()
            .unwrap()
            .upsert_track(&untagged_track(b, "Bohemian Rhapsody", "Queen"))
            .unwrap();
        identify::run(&s, false).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(s
            .library
            .lock()
            .unwrap()
            .get_track(tid2)
            .unwrap()
            .unwrap()
            .canonical
            .is_some());
        // A key entered in Settings: kept, used, and removing it goes back to the configured one
        let (st, b) = call(
            &app,
            "PUT",
            "/v1/library/identify/settings",
            Some(serde_json::json!({"acoustid_key": "own-key!"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        let (st, b) = call(
            &app,
            "PUT",
            "/v1/library/identify/settings",
            Some(serde_json::json!({"acoustid_key": " OwnKey42 "})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            (b["acoustid"].clone(), b["acoustid_own"].clone()),
            (true.into(), true.into())
        );
        assert_eq!(s.acoustid.read().unwrap().api_key, "OwnKey42");
        let (_, b) = call(
            &app,
            "PUT",
            "/v1/library/identify/settings",
            Some(serde_json::json!({"acoustid_key": ""})),
        )
        .await;
        assert_eq!(b["acoustid_own"], false, "{b}");
        assert_eq!(s.acoustid.read().unwrap().api_key, "");
        // Without a key: a new file gets MusicBrainz search suggestions only.
        s.acoustid.write().unwrap().api_key.clear();
        let c = music.join("other.wav");
        tone_wav(&c, 9);
        let tid3 = s
            .library
            .lock()
            .unwrap()
            .upsert_track(&untagged_track(c, "Bohemian Rhapsody", "Queen"))
            .unwrap();
        identify::run(&s, false).await.unwrap();
        assert!(s
            .library
            .lock()
            .unwrap()
            .get_track(tid3)
            .unwrap()
            .unwrap()
            .canonical
            .is_none());
        let (_, sug) = call(&app, "GET", "/v1/library/suggestions", None).await;
        let sug = sug.as_array().unwrap();
        assert_eq!(sug.len(), 1, "only MusicBrainz scores >= 80: {sug:?}");
        assert_eq!(sug[0]["source"], "search");
        assert_eq!(sug[0]["track"]["id"], tid3.0);
        let (st, t3) = call(
            &app,
            "POST",
            &format!("/v1/library/suggestions/{}/accept", sug[0]["id"]),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{t3}");
        assert_eq!(t3["canonical"][1]["source"], "confirmed_by_user", "{t3}");
        let (_, st) = call(&app, "GET", "/v1/library/identify", None).await;
        assert_eq!(st["counts"]["identified"], 3, "{st}");
        assert_eq!(st["counts"]["suggested"], 0);
    }

    /// Favourites and playlist items from other services render as rows:
    /// Pinakes's card gives title, artist and cover in one batch; a link
    /// nobody knows keeps an empty title (never the URL) and its service.
    #[tokio::test]
    async fn describe_names_links_outside_the_library() {
        let card = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-pubky/tests/fixtures/pinakes/music/card-tidal-track-bohemian.json"
        ))
        .unwrap();
        let card: serde_json::Value = serde_json::from_str(&card).unwrap();
        let card = serde_json::json!([card.get("card").cloned().unwrap_or(card.clone())]).to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new().route(
            "/v1/resources/lookup",
            post(move || {
                let card = card.clone();
                async move { ([(header::CONTENT_TYPE, "application/json")], card) }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        let app = router(s.clone());
        let body = serde_json::json!({"uris": ["https://tidal.com/track/534050211", "https://tidal.com/album/338204560"]});
        let (st, v) = call(&app, "POST", "/v1/discover/describe", Some(body.clone())).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let v = v.as_array().unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["title"], "Bohemian Rhapsody");
        assert!(v[0]["image"].as_str().unwrap().starts_with("https://"));
        assert!(v[1]["title"].is_null(), "never the URL as a name: {}", v[1]);
        assert_eq!(v[1]["site"], "TIDAL");
        assert_eq!(v[1]["page"], "https://tidal.com/album/338204560");
        // Cached: the same answer without Pinakes.
        let (st, again) = call(&app, "POST", "/v1/discover/describe", Some(body)).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(again[0]["title"], "Bohemian Rhapsody");
    }

    /// An item page for a link nobody tagged (owner bug "Pinakes has not seen
    /// this link"): by-uri says 404, so the node asks Pinakes to read it
    /// (once, even for two pages at the same time); while Pinakes reads, the
    /// page answers with what is known and `reading`; then the card arrives.
    #[tokio::test]
    async fn an_unknown_link_is_resolved_never_an_error() {
        let card = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-pubky/tests/fixtures/pinakes/music/card-deezer-album-cut-to-the-feeling.json"
        ))
        .unwrap();
        let resolves = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r2 = resolves.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route(
                "/v1/resources/by-uri",
                get(|| async { (StatusCode::NOT_FOUND, r#"{"error":"not found"}"#) }),
            )
            .route(
                "/v1/resolve",
                post(move |Json(body): Json<serde_json::Value>| {
                    let card = card.clone();
                    let n = r2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        assert_eq!(body["uri"], "https://www.deezer.com/album/41839661");
                        // Slow enough that two pages overlap.
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        if n == 0 {
                            let mut v: serde_json::Value = serde_json::from_str(&card).unwrap();
                            v["status"] = "pending".into();
                            v["also_on"] = serde_json::json!([]);
                            (
                                StatusCode::ACCEPTED,
                                [(header::CONTENT_TYPE, "application/json")],
                                v.to_string(),
                            )
                        } else {
                            (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], card)
                        }
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        let app = router(s.clone());
        let q = "/v1/discover/item?uri=https%3A%2F%2Fwww.deezer.com%2Falbum%2F41839661";
        let (a, b) = tokio::join!(call(&app, "GET", q, None), call(&app, "GET", q, None));
        for (st, d) in [&a, &b] {
            assert_eq!(*st, StatusCode::OK, "never an error page: {d}");
            assert_eq!(d["reading"], true, "{d}");
            assert_eq!(
                d["item"]["title"], "Cut To The Feeling",
                "the pending card's name: {d}"
            );
        }
        assert_eq!(
            resolves.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one resolve for two pages"
        );
        // A moment later the page asks again and Pinakes has read it.
        *s.pinakes_reader.retry_after.lock().unwrap() = std::time::Duration::ZERO;
        let (st, d) = call(&app, "GET", q, None).await;
        assert_eq!(st, StatusCode::OK, "{d}");
        assert!(d["reading"].is_null(), "{d}");
        assert_eq!(d["upc"], "0602557656527");
        assert!(!d["item"]["also_on"].as_array().unwrap().is_empty());
        assert_eq!(resolves.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Found cards are cached: no more resolves.
        *s.pinakes_reader.retry_after.lock().unwrap() = crate::pinakes_reader::RETRY_AFTER;
        call(&app, "GET", q, None).await;
        assert_eq!(resolves.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// Deep links and reloads of web UI paths land on the UI (SPA fallback).
    #[tokio::test]
    async fn web_ui_paths_serve_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s);
        for path in [
            "/song/mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69",
            "/album/u-aHR0cHM6Ly90aWRhbC5jb20vYWxidW0vMzM4MjA0NTYw",
            "/artist/mb-0383dadf-2a4e-4d10-a46a-e9e041da8eb3",
            "/search?q=cro%20bad%20chick",
            "/discover",
            "/settings",
            "/settings/backup",
            "/sources/tidal",
            "/shared/zswn37ir9t8ge4r4posn4r7ow3eh384c8ud4kkxm585igt38x31o/0035WKVKQDHSM",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            let body = http_body_util::BodyExt::collect(resp.into_body())
                .await
                .unwrap()
                .to_bytes();
            assert!(
                String::from_utf8_lossy(&body).contains("/app.js"),
                "{path} is not the UI"
            );
        }
    }

    /// An identified local file (fingerprint, no IDs in its tags) gets its
    /// versions from its MusicBrainz URL's own card: Pinakes treats MusicBrainz
    /// links as hubs (pubky/pinakes#24).
    #[tokio::test]
    async fn identified_files_get_versions_from_their_musicbrainz_hub() {
        let mut card: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../pm-pubky/tests/fixtures/pinakes/music/detail-tidal-track-bohemian.json"
            ))
            .unwrap(),
        )
        .unwrap();
        // The hub card: the MusicBrainz URL with the group's service links.
        let all = card["details"]["also_on"].as_array().unwrap().clone();
        let mut links = vec![
            serde_json::json!({"uri": "https://tidal.com/track/534050211", "site": "TIDAL", "id": "a14a55b508a614dd965baefb8c4587cc"}),
        ];
        links.extend(all);
        card["uri"] = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69".into();
        card["site"] = "MusicBrainz".into();
        card["also_on"] = serde_json::json!(links.clone());
        card["details"]["also_on"] = serde_json::json!(links);
        let card = card.to_string();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route(
                "/v1/resources/by-uri",
                get(
                    move |Query(q): Query<std::collections::HashMap<String, String>>| {
                        let card = card.clone();
                        let seen = s2.clone();
                        async move {
                            seen.lock()
                                .unwrap()
                                .push(q.get("uri").cloned().unwrap_or_default());
                            ([(header::CONTENT_TYPE, "application/json")], card)
                        }
                    },
                ),
            )
            .route(
                "/v1/search",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"items":[],"cursor":null,"reach_truncated":false}"#,
                    )
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        let tid = {
            let lib = s.library.lock().unwrap();
            let id = lib
                .upsert_track(&untagged_track(
                    dir.path().join("11.flac"),
                    "Bohemian Rhapsody",
                    "Queen",
                ))
                .unwrap();
            let mbid = pm_model::Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap();
            lib.set_identity(
                id.0,
                &pm_model::MusicUri::Recording { mbid },
                pm_model::Confidence::Fingerprint { score: 0.97 },
                Some("9ff43b6a-4f16-427c-93c2-92307ca505e0"),
            )
            .unwrap();
            id
        };
        let app = router(s.clone());
        let (st, v) = call(
            &app,
            "GET",
            &format!("/v1/library/tracks/{}/versions", tid.0),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(
            seen.lock().unwrap().first().map(String::as_str),
            Some("https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"),
            "asks for the hub card by the canonical URL"
        );
        let sources: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["source"].as_str().unwrap())
            .collect();
        for want in ["tidal", "spotify", "deezer", "youtube"] {
            assert!(sources.contains(&want), "{want} missing: {v}");
        }
        let tidal = v
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["source"] == "tidal")
            .unwrap();
        assert_eq!(tidal["uri"], "tidal:track:534050211", "TIDAL plays: {tidal}");
    }

    /// "Write IDs into my files" is off by default; when the owner asks, a
    /// fingerprinted FLAC gets its recording and release MBIDs in its own
    /// tags, after a backup, and the scanner then reads them as tag IDs.
    #[tokio::test]
    async fn identified_ids_are_written_into_files_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("music").join("11 bohemian.flac");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../pm-library/tests/fixtures/ietf-60-mono-audio.flac"
            ),
            &f,
        )
        .unwrap();
        let s = state(dir.path(), None).await;
        let mbid = pm_model::Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap();
        let rel = pm_model::Mbid::parse("1a5b6a49-4c4f-3d18-9e3d-1c4d1b0a7a3e").unwrap();
        let tid = {
            let lib = s.library.lock().unwrap();
            let id = lib
                .upsert_track(&untagged_track(f.clone(), "Bohemian Rhapsody", "Queen"))
                .unwrap();
            lib.set_identity(
                id.0,
                &pm_model::MusicUri::Recording { mbid },
                pm_model::Confidence::FingerprintRelease { score: 0.97 },
                Some("84eed5cd-5b1f-4f5c-8c92-2686d2877862"),
            )
            .unwrap();
            lib.set_identify_state(id.0, "identified", Some("fingerprint+release"))
                .unwrap();
            lib.set_identify_release(id.0, &rel).unwrap();
            id
        };
        let app = router(s.clone());
        let (_, st) = call(&app, "GET", "/v1/library/identify", None).await;
        assert_eq!(st["write_ids"], false, "off by default");
        assert!(pm_library::probe_and_read(&f).unwrap().1.recording_mbid.is_none());
        let (st, on) = call(
            &app,
            "PUT",
            "/v1/library/identify/settings",
            Some(serde_json::json!({"write_ids": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{on}");
        assert_eq!(on["write_ids"], true);
        let (st, r) = call(&app, "POST", "/v1/library/identify/write", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["written"], 1, "{r}");
        let (_, tags) = pm_library::probe_and_read(&f).unwrap();
        assert_eq!(tags.recording_mbid, Some(mbid));
        assert_eq!(tags.release_mbid, Some(rel));
        let backups: Vec<_> = std::fs::read_dir(r["backup_dir"].as_str().unwrap())
            .unwrap()
            .collect();
        assert_eq!(backups.len(), 1, "one backup per write");
        let _ = tid;
    }

    /// One item page (docs/ux/08-item-page.md): an album only a service knows
    /// lists its songs (each with its own song page), Pinakes not knowing the
    /// link is no error, and library albums and songs answer the same shape.
    #[tokio::test]
    async fn item_pages_list_songs_for_every_source() {
        let t = |id: u64, title: &str, n: u64| {
            serde_json::json!({"id": id, "title": title, "duration": 200 + n, "track_position": n, "preview": "http://x/p.mp3",
                "artist": {"id": 7, "name": "DAT ADAM"}, "album": {"id": 1999, "title": "HYDRA 3D"}})
        };
        let album = serde_json::json!({"id": 1999, "title": "HYDRA 3D", "release_date": "2024-01-01", "cover_big": "http://img/h.jpg",
            "artist": {"id": 7, "name": "DAT ADAM"}, "tracks": {"data": [t(11, "HYDRA", 1), t(12, "MEDUSA", 2), t(13, "NEBEL", 3)]}});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route(
                "/album/{id}",
                get(move || {
                    let a = album.clone();
                    async move { Json(a) }
                }),
            )
            // Pinakes has never seen the link and cannot read it.
            .route(
                "/v1/resources/by-uri",
                get(|| async { (StatusCode::NOT_FOUND, r#"{"error":"not found"}"#) }),
            )
            .route(
                "/v1/resolve",
                post(|| async { (StatusCode::NOT_FOUND, r#"{"error":"not found"}"#) }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        *s.deezer_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let id = discover::page_id("https://www.deezer.com/album/1999");
        assert_eq!(
            discover::url_of_page("album", &id).as_deref(),
            Some("https://www.deezer.com/album/1999"),
            "page ids round-trip"
        );
        let (st, pg) = call(&app, "GET", &format!("/v1/item/album/{id}"), None).await;
        assert_eq!(st, StatusCode::OK, "{pg}");
        assert_eq!(pg["title"], "HYDRA 3D");
        assert_eq!(pg["artist"]["name"], "DAT ADAM");
        let songs = pg["songs"].as_array().unwrap();
        assert_eq!(songs.len(), 3, "{pg}");
        assert_eq!(songs[1]["title"], "MEDUSA");
        assert_eq!(songs[1]["no"], 2);
        assert_eq!(songs[1]["uri"], "deezer:track:12", "each row plays its own track");
        let song_id = songs[1]["id"].as_str().unwrap();
        assert_eq!(
            discover::url_of_page("song", song_id).as_deref(),
            Some("https://www.deezer.com/track/12"),
            "the title opens that song's page"
        );
        assert_eq!(pg["plays_from"]["mode"], "preview");
        assert_eq!(pg["target"]["uri"], "https://www.deezer.com/album/1999");
        assert_eq!(pg["section"], "home");
        // The library: an album and a song answer the same shape.
        let (aid, tid) = {
            let lib = s.library.lock().unwrap();
            let mut tr = untagged_track(dir.path().join("01.flac"), "Death on Two Legs", "Queen");
            tr.album = Some(pm_model::Album {
                id: None,
                title: "A Night at the Opera".into(),
                album_artist: Some("Queen".into()),
                release_mbid: None,
                release_group_mbid: None,
                year: Some(1975),
                upc: None,
                total_discs: None,
            });
            let mbid = pm_model::Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap();
            tr.canonical = Some((
                pm_model::MusicUri::Recording { mbid },
                pm_model::Confidence::TaggedMbid,
            ));
            let tid = lib.upsert_track(&tr).unwrap();
            let t = lib.get_track(tid).unwrap().unwrap();
            (t.album.unwrap().id.unwrap(), tid)
        };
        // Another recording of the same song (a live take) in the library
        let live = {
            let lib = s.library.lock().unwrap();
            let tr = untagged_track(dir.path().join("02.flac"), "Death on Two Legs (Live)", "Queen");
            lib.upsert_track(&tr).unwrap()
        };
        assert_eq!(id, "deezer-1999", "a service page has a readable id");
        let (st, al) = call(&app, "GET", &format!("/v1/item/album/lib-{aid}"), None).await;
        assert_eq!(st, StatusCode::OK, "{al}");
        assert_eq!(al["songs"][0]["track_id"], tid.0);
        assert_eq!(
            al["songs"][0]["id"], "mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69",
            "matched songs use their canonical id"
        );
        assert_eq!(al["target"]["album_id"], aid);
        assert_eq!(al["section"], "albums");
        // The canonical id opens the library's song page.
        let (st, so) = call(
            &app,
            "GET",
            "/v1/item/song/mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{so}");
        assert_eq!(so["target"]["track_id"], tid.0);
        // The generic id is the song's global page; the library copy is one of its sources
        assert_eq!(so["id"], "mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69");
        assert_eq!(so["source_id"], "global");
        assert!(
            so["also_on"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["page"] == format!("lib-{}", tid.0)),
            "{so}"
        );
        // The library page itself is the thin view, linking to the global page
        let (_, lp) = call(&app, "GET", &format!("/v1/item/song/lib-{}", tid.0), None).await;
        assert_eq!(
            lp["song_uri"],
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"
        );
        // Versions are other recordings only; the page itself and MusicBrainz are not "also on"
        let versions = so["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 1, "{so}");
        assert_eq!(versions[0]["page"], format!("lib-{}", live.0));
        assert!(so["also_on"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["source"] != "musicbrainz" && v["page"] != so["id"]));
        assert_eq!(so["album"]["id"], format!("lib-{aid}"));
        assert!(
            so["about"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["label"] == "File"),
            "library facts in About"
        );
        let (st, _) = call(&app, "GET", "/v1/item/playlist/x", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// A body that doesn't fit answers in plain words, with the detail apart.
    #[tokio::test]
    async fn bad_bodies_answer_plainly() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (st, e) = call(
            &app,
            "POST",
            "/v1/playlists",
            Some(serde_json::json!({"uris": "not a list"})),
        )
        .await;
        assert!(st.is_client_error(), "{st}");
        assert_eq!(
            e["error"], "The node didn't understand that request. Reload the page and try again.",
            "{e}"
        );
        assert!(e["detail"].as_str().unwrap().contains("uris"), "{e}");
    }

    /// Library tags are Pubky tags (STEWARD-UX-INSTANCES item 6): an unmatched file takes a tag
    /// that waits and is published once matched; the files' genres become tags when switched on.
    #[tokio::test]
    async fn unmatched_files_take_tags_and_genres_publish_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let tid = {
            let lib = s.library.lock().unwrap();
            let mut tr = untagged_track(dir.path().join("01.flac"), "Trip to Serbia", "Sefa");
            tr.genres = vec!["Hardcore".into()];
            lib.upsert_track(&tr).unwrap().0
        };
        let app = router(s.clone());
        let (st, r) = call(
            &app,
            "PUT",
            "/v1/tags",
            Some(serde_json::json!({"track_id": tid, "label": "Frenchcore"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let (_, t) = call(&app, "GET", &format!("/v1/tags?track_id={tid}"), None).await;
        assert_eq!(t["tags"][0]["label"], "frenchcore", "{t}");
        assert_eq!(t["tags"][0]["pending"], true);
        let (_, all) = call(&app, "GET", "/v1/tags", None).await;
        assert!(
            all["tags"].as_array().unwrap().is_empty(),
            "nothing to publish yet: {all}"
        );
        // Genres switched on: nothing yet (not matched)
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"genre_tags": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        // Matched: the waiting tag becomes an ordinary one, and the genre follows at the next sync pass
        s.library
            .lock()
            .unwrap()
            .set_identity(
                tid,
                &pm_model::MusicUri::Isrc {
                    isrc: pm_model::Isrc::parse("NLZ3R2400021").unwrap(),
                },
                pm_model::Confidence::ConfirmedByUser,
                None,
            )
            .unwrap();
        let (_, t) = call(&app, "GET", &format!("/v1/tags?track_id={tid}"), None).await;
        assert_eq!(t["uri"], "https://musicbrainz.org/isrc/NLZ3R2400021", "{t}");
        assert_eq!(t["tags"][0]["label"], "frenchcore");
        assert!(t["tags"][0].get("pending").is_none());
        crate::tags::before_sync(&s.library.lock().unwrap());
        let (_, t) = call(&app, "GET", &format!("/v1/tags?track_id={tid}"), None).await;
        let labels: Vec<&str> = t["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"hardcore"), "{t}");
    }

    /// The owner's Deezer page (STEWARD-UX-INSTANCES item 9): Play and the eyebrow come from one
    /// decision. A track Deezer doesn't offer here says why instead of failing with a 404; a
    /// playable one plays its own 30 s preview.
    #[tokio::test]
    async fn a_deezer_page_plays_its_preview_or_says_why_not() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new().route(
            "/track/{id}",
            get(|Path(id): Path<String>| async move {
                let readable = id != "4329888262";
                Json(serde_json::json!({"id": id.parse::<u64>().unwrap(), "title": "Bohemian Rhapsody", "readable": readable,
                    "preview": if readable { "https://cdn.example/p.mp3" } else { "" }, "duration": 354,
                    "artist": {"id": 412, "name": "Queen"}, "album": {"id": 9, "title": "Greatest Hits"}}))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        *s.deezer_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (st, pg) = call(&app, "GET", "/v1/item/song/deezer-4329888262", None).await;
        assert_eq!(st, StatusCode::OK, "{pg}");
        assert!(pg["plays_note"].as_str().unwrap().contains("country"), "{pg}");
        assert_ne!(pg["plays_from"]["service"], "deezer", "{pg}");
        let (_, pg) = call(&app, "GET", "/v1/item/song/deezer-1109731", None).await;
        assert!(pg.get("plays_note").is_none(), "{pg}");
        assert_eq!(pg["plays_from"]["service"], "deezer");
        assert_eq!(pg["plays_from"]["mode"], "preview");
        assert_eq!(pg["songs"][0]["uri"], "deezer:track:1109731");
    }

    /// Search finds Pubky tags (STEWARD-FIX-search-tags.md): a term that only a tag matches
    /// still finds the song; `#a #b` asks for both tags; suggestions list yours first.
    #[tokio::test]
    async fn search_finds_songs_by_their_pubky_tags() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let card = |id: &str, name: &str| serde_json::json!({"id": id, "uri": format!("https://www.deezer.com/track/{id}"), "status": "ok", "type": "MusicRecording", "name": name, "site": "Deezer"});
        let mock = Router::new()
            .route(
                "/v1/search",
                get(
                    move |Query(q): Query<std::collections::HashMap<String, String>>| {
                        let items = match (
                            q.get("q"),
                            q.get("tags").map(String::as_str),
                            q.get("match").map(String::as_str),
                        ) {
                            (Some(_), _, _) => vec![],
                            (None, Some("chill"), None) => vec![card("1", "Tagged Only")],
                            (None, Some("chill,90s"), Some("all")) => vec![card("2", "Both Tags")],
                            _ => vec![],
                        };
                        async move { Json(serde_json::json!({"items": items})) }
                    },
                ),
            )
            .route(
                "/nexus/v0/search/tags/by_prefix/{p}",
                get(|| async { Json(serde_json::json!(["chill", "chicago"])) }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        let tid = {
            let lib = s.library.lock().unwrap();
            let mut tr = untagged_track(dir.path().join("c.flac"), "Calm Song", "Someone");
            tr.canonical = Some((
                pm_model::MusicUri::Isrc {
                    isrc: pm_model::Isrc::parse("USAAA1111111").unwrap(),
                },
                pm_model::Confidence::TaggedIsrc,
            ));
            lib.upsert_track(&tr).unwrap().0
        };
        let app = router(s.clone());
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/tags",
            Some(serde_json::json!({"track_id": tid, "label": "chill"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let lines = |app: Router, uri: String| async move {
            let resp = app
                .oneshot(
                    axum::http::Request::get(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            String::from_utf8_lossy(&body)
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v.get("source").is_some())
                .collect::<Vec<_>>()
        };
        let ans = lines(app.clone(), "/v1/search?q=chill&in=pubky".into()).await;
        let pubky = ans
            .iter()
            .find(|a| a["source"] == "pubky")
            .expect("pubky answered");
        assert_eq!(pubky["hits"][0]["title"], "Tagged Only", "{pubky}");
        assert_eq!(pubky["hits"][0]["tagged"], "#chill");
        let ans = lines(app.clone(), "/v1/search?q=%23chill%20%2390s".into()).await;
        let srcs: Vec<&str> = ans.iter().filter_map(|a| a["source"].as_str()).collect();
        assert!(srcs.iter().all(|x| *x == "library" || *x == "pubky"), "{srcs:?}");
        let pubky = ans.iter().find(|a| a["source"] == "pubky").unwrap();
        assert_eq!(pubky["hits"][0]["title"], "Both Tags", "{pubky}");
        let ans = lines(app.clone(), "/v1/search?q=%23chill".into()).await;
        let lib = ans.iter().find(|a| a["source"] == "library").unwrap();
        assert_eq!(
            lib["hits"][0]["title"], "Calm Song",
            "your own tag, offline: {lib}"
        );
        let (_, sug) = call(&app, "GET", "/v1/search/tags?prefix=chi", None).await;
        assert_eq!(sug["mine"][0]["label"], "chill", "{sug}");
        assert_eq!(sug["others"], serde_json::json!(["chicago"]));
    }

    /// Profiles (STEWARD-UX-PEOPLE.md): cards in one batch, a person's page with what they
    /// tagged, avatars served by the node, and the faces behind a song's tags.
    #[tokio::test]
    async fn profiles_come_from_nexus_and_tags_show_faces() {
        const A: &str = "gujx6qd8ksydh1makdphd3bxu351d9b8waqka8hfg6q7hnqkxexo";
        const B: &str = "pxnu33x7jtpx9ar1ytsi4yxbp6a5o36gwhffs8zoxmbuptici1jy";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let batches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let b2 = batches.clone();
        let user = |id: &str| serde_json::json!({"details": {"id": id, "name": "John", "bio": "music", "image": format!("pubky://{id}/pub/pubky.app/files/1"), "links": [{"title": "web", "url": "https://synonym.to"}]}, "counts": {"followers": 3, "following": 2}});
        let mock = Router::new()
            .route(
                "/nexus/v0/stream/users/by_ids",
                post(move |Json(b): Json<serde_json::Value>| {
                    b2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let ids: Vec<String> = serde_json::from_value(b["user_ids"].clone()).unwrap();
                    async move { Json(serde_json::Value::Array(ids.iter().filter(|i| *i == A).map(|i| user(i)).collect())) }
                }),
            )
            .route("/nexus/v0/user/{id}", get(move |Path(id): Path<String>| async move { Json(user(&id)) }))
            .route("/nexus/static/avatar/{id}", get(|| async { ([(header::CONTENT_TYPE, "image/png")], vec![0x89u8, b'P', b'N', b'G', 1, 2, 3]) }))
            .route(
                "/nexus/v0/resource/by-uri",
                get(|| async { Json(serde_json::json!({"resource": {"id": "r1", "uri": "https://musicbrainz.org/isrc/USAAA1111111"}, "tags": [{"label": "chill", "taggers": [A, B], "taggers_count": 2}]})) }),
            )
            .route(
                "/v1/search",
                get(|Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    assert_eq!(q.get("author").map(String::as_str), Some(A));
                    Json(serde_json::json!({"items": [{"id": "c1", "uri": "https://www.deezer.com/track/9", "status": "ok", "type": "MusicRecording", "name": "Calm Song", "site": "Deezer"}],
                        "facets": {"tags": [{"label": "chill", "count": 4}]}}))
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        let app = router(s.clone());
        let (st, cards) = call(&app, "GET", &format!("/v1/profiles?ids={A},{B},nonsense"), None).await;
        assert_eq!(st, StatusCode::OK, "{cards}");
        assert_eq!(cards[0]["name"], "John");
        assert_eq!(cards[0]["avatar"], format!("/v1/profiles/{A}/avatar"));
        assert_eq!(cards[1]["name"], "pxnu…i1jy", "not indexed: a short key");
        assert_eq!(cards[1]["known"], false);
        assert_eq!(cards.as_array().unwrap().len(), 2, "invalid keys are dropped");
        // Cached: a second ask makes no call
        call(&app, "GET", &format!("/v1/profiles?ids={A},{B}"), None).await;
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (_, pg) = call(&app, "GET", &format!("/v1/profiles/{A}"), None).await;
        assert_eq!(pg["bio"], "music");
        assert_eq!(pg["tags"][0][0], "chill");
        assert_eq!(pg["tagged"][0]["title"], "Calm Song");
        assert_eq!(pg["profile_url"], format!("https://pubky.app/profile/{A}"));
        let req = Request::builder()
            .uri(format!("/v1/profiles/{A}/avatar"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.headers()["content-type"], "image/png");
        assert!(
            dir.path().join("art/people").join(A).is_file(),
            "kept on the node"
        );
        // Following needs a Pubky sign-in
        let (st, _) = call(&app, "PUT", &format!("/v1/profiles/{A}/follow"), None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        // The faces behind a tag
        let (_, op) = call(
            &app,
            "GET",
            "/v1/discover/uri?uri=https%3A%2F%2Fmusicbrainz.org%2Fisrc%2FUSAAA1111111",
            None,
        )
        .await;
        assert_eq!(op["faces"]["chill"], serde_json::json!([A, B]), "{op}");
    }

    /// Smart playlists (UX review #9): rules over genre, year, favourites, plays and your tags,
    /// all or any, previewed while editing, saved, and worked out again each time.
    #[tokio::test]
    async fn smart_playlists_pick_songs_by_rules() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let ids: Vec<i64> = {
            let lib = s.library.lock().unwrap();
            let mut out = Vec::new();
            for (i, (title, genre, year)) in [
                ("Calm", "Ambient", 1995),
                ("Fast", "Techno", 2020),
                ("Faster", "Techno", 2005),
            ]
            .iter()
            .enumerate()
            {
                let mut t = untagged_track(dir.path().join(format!("{i}.flac")), title, "Someone");
                t.genres = vec![genre.to_string()];
                t.duration_ms = Some(200_000);
                t.album = Some(pm_model::Album {
                    id: None,
                    title: format!("Album {i}"),
                    album_artist: Some("Someone".into()),
                    release_mbid: None,
                    release_group_mbid: None,
                    year: Some(*year),
                    upc: None,
                    total_discs: None,
                });
                t.canonical = Some((
                    pm_model::MusicUri::Isrc {
                        isrc: pm_model::Isrc::parse(&format!("USAAA111111{i}")).unwrap(),
                    },
                    pm_model::Confidence::TaggedIsrc,
                ));
                out.push(lib.upsert_track(&t).unwrap().0);
            }
            lib.record_play(TrackId(out[2]), Some("z")).unwrap();
            lib.record_play(TrackId(out[2]), Some("z")).unwrap();
            out
        };
        let app = router(s.clone());
        call(
            &app,
            "PUT",
            "/v1/bookmarks",
            Some(serde_json::json!({"track_id": ids[1]})),
        )
        .await;
        call(
            &app,
            "PUT",
            "/v1/tags",
            Some(serde_json::json!({"track_id": ids[0], "label": "sleep"})),
        )
        .await;
        let titles = |v: &serde_json::Value| -> Vec<String> {
            let mut t: Vec<String> = v["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["title"].as_str().unwrap().to_string())
                .collect();
            t.sort();
            t
        };
        let (st, r) = call(
            &app,
            "POST",
            "/v1/smart/preview",
            Some(serde_json::json!({"rules": [{"field": "genre", "op": "is", "value": "techno"}]})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(titles(&r), ["Fast", "Faster"]);
        let (_, r) = call(&app, "POST", "/v1/smart/preview", Some(serde_json::json!({"rules": [{"field": "genre", "value": "techno"}, {"field": "bookmark", "value": true}]}))).await;
        assert_eq!(titles(&r), ["Fast"], "all rules");
        // A rule saved under the older name still works
        let (_, r) = call(&app, "POST", "/v1/smart/preview", Some(serde_json::json!({"rules": [{"field": "genre", "value": "techno"}, {"field": "favourite", "value": true}]}))).await;
        assert_eq!(titles(&r), ["Fast"], "favourite is bookmark");
        let (_, r) = call(&app, "POST", "/v1/smart/preview", Some(serde_json::json!({"matches": "any", "rules": [{"field": "tag", "value": "#sleep"}, {"field": "plays", "op": "at_least", "value": 2}]}))).await;
        assert_eq!(titles(&r), ["Calm", "Faster"], "any rule");
        let (_, r) = call(
            &app,
            "POST",
            "/v1/smart/preview",
            Some(
                serde_json::json!({"rules": [{"field": "year", "op": "between", "value": 2000, "to": 2010}]}),
            ),
        )
        .await;
        assert_eq!(titles(&r), ["Faster"]);
        let (st, _) = call(
            &app,
            "POST",
            "/v1/smart/preview",
            Some(serde_json::json!({"rules": []})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, p) = call(&app, "POST", "/v1/smart", Some(serde_json::json!({"name": "Techno", "rules": [{"field": "genre", "value": "techno"}], "sort": "most_played"}))).await;
        assert_eq!(st, StatusCode::OK, "{p}");
        let id = p["id"].as_str().unwrap().to_string();
        let (_, list) = call(&app, "GET", "/v1/smart", None).await;
        assert_eq!(list[0]["name"], "Techno");
        let (_, t) = call(&app, "GET", &format!("/v1/smart/{id}/tracks"), None).await;
        assert_eq!(t["tracks"][0]["title"], "Faster", "most played first: {t}");
        assert_eq!(t["matched"], 2);
        let (st, _) = call(&app, "DELETE", &format!("/v1/smart/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        let (_, list) = call(&app, "GET", "/v1/smart", None).await;
        assert!(list.as_array().unwrap().is_empty());
        // Seeds: the playlist grows from songs like the seed (same genre first), the seed itself first
        let (st, r) = call(
            &app,
            "POST",
            "/v1/smart/preview",
            Some(serde_json::json!({"seeds": [{"kind": "track", "id": ids[1]}], "sort": "title"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(
            titles(&r),
            ["Calm", "Fast", "Faster"],
            "the whole library is reachable through genre, then artist: {r}"
        );
        // Exclusions: a genre, recently played songs, a song
        let (_, r) = call(&app, "POST", "/v1/smart/preview", Some(serde_json::json!({"seeds": [{"kind": "track", "id": ids[1]}], "exclude": {"genres": ["ambient"]}}))).await;
        assert_eq!(titles(&r), ["Fast", "Faster"]);
        let (_, r) = call(&app, "POST", "/v1/smart/preview", Some(serde_json::json!({"seeds": [{"kind": "track", "id": ids[1]}], "exclude": {"played_within_days": 1}}))).await;
        assert_eq!(titles(&r), ["Calm", "Fast"], "Faster was played today");
        let (_, r) = call(&app, "POST", "/v1/smart/preview", Some(serde_json::json!({"seeds": [{"kind": "track", "id": ids[1]}], "exclude": {"tracks": [ids[0]], "artists": ["nobody"]}}))).await;
        assert_eq!(titles(&r), ["Fast", "Faster"]);
        // Rules still narrow a seeded pool; a length in minutes caps it
        let (_, r) = call(&app, "POST", "/v1/smart/preview", Some(serde_json::json!({"seeds": [{"kind": "track", "id": ids[1]}], "rules": [{"field": "favourite", "value": true}]}))).await;
        assert_eq!(titles(&r), ["Fast"]);
        let (_, r) = call(&app, "POST", "/v1/smart/preview", Some(serde_json::json!({"seeds": [{"kind": "track", "id": ids[1]}], "minutes": 1, "sort": "title"}))).await;
        assert_eq!(
            r["tracks"].as_array().unwrap().len(),
            1,
            "one song fills a minute when lengths are unknown: {r}"
        );
        assert_eq!(r["matched"], 3);
        // An artist seed, and a bad kind
        let aid = s
            .library
            .lock()
            .unwrap()
            .list_artists()
            .unwrap()
            .into_iter()
            .find(|a| a.name == "Someone")
            .unwrap()
            .id;
        let (_, r) = call(
            &app,
            "POST",
            "/v1/smart/preview",
            Some(serde_json::json!({"seeds": [{"kind": "artist", "id": aid}]})),
        )
        .await;
        assert_eq!(titles(&r), ["Calm", "Fast", "Faster"]);
        let (st, _) = call(
            &app,
            "POST",
            "/v1/smart/preview",
            Some(serde_json::json!({"seeds": [{"kind": "mood", "id": 1}]})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // Saved with its seeds and exclusions
        let (st, p) = call(&app, "POST", "/v1/smart", Some(serde_json::json!({"name": "Like Fast", "seeds": [{"kind": "track", "id": ids[1], "label": "Fast"}], "exclude": {"genres": ["ambient"]}, "minutes": 90}))).await;
        assert_eq!(st, StatusCode::OK, "{p}");
        assert_eq!(
            (
                p["seeds"][0]["label"].clone(),
                p["minutes"].clone(),
                p["exclude"]["genres"][0].clone()
            ),
            (
                serde_json::json!("Fast"),
                serde_json::json!(90),
                serde_json::json!("ambient")
            )
        );
        let (_, t) = call(
            &app,
            "GET",
            &format!("/v1/smart/{}/tracks", p["id"].as_str().unwrap()),
            None,
        )
        .await;
        assert_eq!(titles(&t), ["Fast", "Faster"]);
    }

    /// Lyrics from LRCLIB for a streamed song and for a library song without its own; a song
    /// LRCLIB doesn't know answers 404 and is asked about once.
    #[tokio::test]
    async fn lyrics_come_from_lrclib_when_the_files_have_none() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let a2 = asked.clone();
        let mock = Router::new().route(
            "/api/get",
            get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
                a2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if q.get("track_name").map(String::as_str) == Some("Known Song") {
                        (StatusCode::OK, Json(serde_json::json!({"syncedLyrics": "[00:01.00]First\n[00:03.50]Second", "plainLyrics": "First\nSecond"})))
                    } else {
                        (StatusCode::NOT_FOUND, Json(serde_json::json!({"message": "not found"})))
                    }
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        crate::lrclib::set_base(&format!("http://127.0.0.1:{port}"));
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        s.set_contact("test@example.org").unwrap();
        let tid = {
            let lib = s.library.lock().unwrap();
            lib.upsert_track(&untagged_track(
                dir.path().join("k.flac"),
                "Known Song",
                "Someone",
            ))
            .unwrap()
            .0
        };
        let app = router(s.clone());
        let (st, l) = call(
            &app,
            "GET",
            "/v1/lyrics?title=Known%20Song&artist=Someone&duration_ms=200000",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{l}");
        assert_eq!(
            (l["source"].as_str(), l["synced"].as_bool()),
            (Some("lrclib"), Some(true))
        );
        assert_eq!(l["lines"][1]["ms"], 3500);
        let (st, l) = call(&app, "GET", &format!("/v1/library/tracks/{tid}/lyrics"), None).await;
        assert_eq!(st, StatusCode::OK, "{l}");
        assert_eq!(l["source"], "lrclib");
        let (st, _) = call(&app, "GET", "/v1/lyrics?title=Nothing&artist=Nobody", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let before = asked.load(std::sync::atomic::Ordering::SeqCst);
        call(&app, "GET", "/v1/lyrics?title=Nothing&artist=Nobody", None).await;
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "a none is kept too"
        );
    }

    /// One search, streamed per source: the library answers first, a slow
    /// source still arrives, a failing one reports itself without holding
    /// the others back, and a second ask comes from the cache.
    #[tokio::test]
    async fn search_streams_each_source_as_it_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let track = serde_json::json!({"id": 3135556, "title": "Bohemian Rhapsody", "isrc": "GBUM71029604", "duration": 355, "preview": "http://x/p.mp3",
            "artist": {"id": 412, "name": "Queen"}, "album": {"id": 1, "title": "A Night at the Opera", "cover_medium": "http://img/o.jpg"}});
        let mock = Router::new()
            .route(
                "/search/track",
                get(move || {
                    let t = track.clone();
                    async move {
                        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                        Json(serde_json::json!({"data": [t]}))
                    }
                }),
            )
            .route(
                "/search/album",
                get(|| async { Json(serde_json::json!({"data": []})) }),
            )
            .route(
                "/search/artist",
                get(|| async { Json(serde_json::json!({"data": []})) }),
            )
            .route(
                "/v1/search",
                get(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        r#"{"error":"index rebuilding"}"#,
                    )
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        *s.deezer_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        {
            let lib = s.library.lock().unwrap();
            lib.upsert_track(&untagged_track(
                dir.path().join("b.flac"),
                "Bohemian Rhapsody",
                "Queen",
            ))
            .unwrap();
        }
        let app = router(s.clone());
        let ask = |app: Router| async move {
            let resp = app
                .oneshot(
                    axum::http::Request::get("/v1/search?q=bohemian")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.headers()["content-type"], "application/x-ndjson");
            let body = http_body_util::BodyExt::collect(resp.into_body())
                .await
                .unwrap()
                .to_bytes();
            String::from_utf8(body.to_vec())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str::<search::SourceAnswer>(l).unwrap())
                .collect::<Vec<_>>()
        };
        let lines = ask(app.clone()).await;
        let order: Vec<&str> = lines.iter().map(|a| a.source.as_str()).collect();
        assert_eq!(order.len(), 3, "{order:?}");
        assert_eq!(order[0], "library", "the library answers first: {order:?}");
        let lib = &lines[0];
        assert_eq!(lib.status, "ok");
        assert_eq!(lib.hits[0].kind, "song");
        assert_eq!(lib.hits[0].title, "Bohemian Rhapsody");
        let dz = lines.iter().find(|a| a.source == "deezer").unwrap();
        assert_eq!(dz.status, "ok");
        assert!(dz.ms >= 500, "the slow source: {} ms", dz.ms);
        assert_eq!(
            dz.hits[0].keys,
            vec!["isrc:GBUM71029604".to_string()],
            "rows merge by ISRC"
        );
        assert_eq!(dz.hits[0].uri.as_deref(), Some("deezer:track:3135556"));
        let pk = lines.iter().find(|a| a.source == "pubky").unwrap();
        assert_eq!(pk.status, "error", "{pk:?}");
        assert!(pk.error.as_deref().unwrap().contains("index rebuilding"));
        // Scoped to one source, and cached the second time.
        let again = ask(app.clone()).await;
        assert!(again.iter().find(|a| a.source == "deezer").unwrap().cached);
        let resp = app
            .oneshot(
                axum::http::Request::get("/v1/search?q=bohemian&in=library")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(
            String::from_utf8_lossy(&body).lines().count(),
            1,
            "in=library asks only the library"
        );
    }

    /// Home: one Pinakes call per section; empty sections say why, a failing
    /// one carries its own error, signed-out and city-less sections ask for
    /// what they need, and a tag page returns co-tags.
    #[tokio::test]
    async fn home_sections_answer_items_reasons_or_their_own_error() {
        let chart = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-pubky/tests/fixtures/pinakes/music/search-trending-tracks-7d.json"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route(
                "/v1/search",
                get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
                    let chart = chart.clone();
                    async move {
                        let t = q.get("type").map(String::as_str);
                        let body = match (t, q.get("sort").map(String::as_str), q.get("facet").map(String::as_str)) {
                            (Some("MusicRecording"), Some("trending"), _) => chart,
                            (Some("MusicAlbum"), Some("trending"), _) => r#"{"items":[],"cursor":null}"#.into(),
                            (Some("MusicGroup"), _, _) => return (StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"index rebuilding"}"#.to_string()),
                            (Some("MusicRecording"), None, Some("tags")) => r#"{"items":[],"facets":{"tags":[{"label":"hip-hop","count":12},{"label":"german","count":4}]}}"#.into(),
                            (None, None, Some("tags")) => r#"{"items":[],"facets":{"tags":[{"label":"hip-hop","count":12},{"label":"rap","count":7}]}}"#.into(),
                            _ => r#"{"items":[]}"#.into(),
                        };
                        (StatusCode::OK, body)
                    }
                }),
            )
            .route(
                "/search",
                get(|| async { Json(serde_json::json!([{"name": "Zürich", "lat": "47.3744", "lon": "8.5410"}])) }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        *s.nominatim_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (st, songs) = call(&app, "GET", "/v1/home/trending-songs?window=7d", None).await;
        assert_eq!(st, StatusCode::OK, "{songs}");
        // The recorded chart lists the same songs more than once (two groups, a remaster): merged
        let items = songs["items"].as_array().unwrap();
        assert_eq!(items.len(), 6, "{songs}");
        assert_eq!(
            items
                .iter()
                .map(|i| i["versions"].as_u64().unwrap_or(0))
                .sum::<u64>(),
            2
        );
        assert!(songs["note"].is_null());
        let (_, albums) = call(&app, "GET", "/v1/home/trending-albums", None).await;
        assert!(albums["items"].as_array().unwrap().is_empty());
        assert_eq!(albums["note"], "No albums were tagged in this window.");
        let (st, artists) = call(&app, "GET", "/v1/home/trending-artists", None).await;
        assert_eq!(
            st,
            StatusCode::OK,
            "a failing section is still a 200 with its own error"
        );
        assert!(
            artists["error"].as_str().unwrap().contains("index rebuilding"),
            "{artists}"
        );
        let (_, following) = call(&app, "GET", "/v1/home/following", None).await;
        assert_eq!(following["needs"], "pubky", "signed out: {following}");
        let (_, tags) = call(&app, "GET", "/v1/home/tags", None).await;
        assert_eq!(tags["tags"][0][0], "hip-hop");
        let (_, concerts) = call(&app, "GET", "/v1/home/concerts", None).await;
        assert_eq!(concerts["needs"], "location");
        let (st, loc) = call(
            &app,
            "PUT",
            "/v1/home/location",
            Some(serde_json::json!({"city": "Zurich"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{loc}");
        assert_eq!(loc["label"], "Zürich");
        let (_, concerts) = call(&app, "GET", "/v1/home/concerts", None).await;
        assert_eq!(concerts["note"], "No concerts near you in the next 30 days.");
        let (_, tag) = call(&app, "GET", "/v1/home/tag?labels=hip-hop", None).await;
        assert_eq!(tag["tags"][1][0], "rap", "co-tags on the tag page: {tag}");
        let (st, _) = call(&app, "GET", "/v1/home/nope", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[test]
    fn service_links_are_recognised() {
        use discover::service_link;
        let l = service_link("https://tidal.com/album/495402659", true).unwrap();
        assert_eq!(
            (l.service, l.kind, l.id.as_str()),
            ("tidal", "album", "495402659")
        );
        let l = service_link("https://www.deezer.com/en/track/4329888262", false).unwrap();
        assert_eq!((l.service, l.kind), ("deezer", "track"));
        let l = service_link("https://open.spotify.com/track/2OBofMJx94NryV2SK8p8Zf", false).unwrap();
        assert_eq!(
            (l.service, l.kind, l.id.as_str()),
            ("spotify", "track", "2OBofMJx94NryV2SK8p8Zf")
        );
        assert!(service_link("https://music.apple.com/us/album/dracula/1830000000", true).is_none());
        assert!(service_link(
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69",
            true
        )
        .is_none());
    }

    /// Pinakes's `also_on` joins the Versions list: every service that has
    /// the same recording, tagged links first, Deezer playable as a preview.
    #[tokio::test]
    async fn versions_include_pinakes_also_on_links() {
        let fixture = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-pubky/tests/fixtures/pinakes/music/search-key-isrc.json"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let mock = Router::new().route(
            "/v1/search",
            get(
                move |Query(q): Query<std::collections::HashMap<String, String>>| {
                    let fixture = fixture.clone();
                    let seen = seen2.clone();
                    async move {
                        seen.lock()
                            .unwrap()
                            .push(q.get("key").cloned().unwrap_or_default());
                        ([(header::CONTENT_TYPE, "application/json")], fixture)
                    }
                },
            ),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let pubky = pm_pubky::config::PmPubkyConfig {
            pinakes_base: format!("http://127.0.0.1:{port}"),
            nexus_base: format!("http://127.0.0.1:{port}/nexus"),
            user_agent_contact: "test@example.org".into(),
            ..Default::default()
        };
        let s = state_with(dir.path(), None, pubky).await;
        s.set_contact("test@example.org").unwrap();
        let tid = {
            let lib = s.library.lock().unwrap();
            let mbid = pm_model::Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap();
            let t = pm_model::Track {
                id: None,
                title: "Bohemian Rhapsody".into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: "Queen".into(),
                    mbid: None,
                }],
                album: None,
                track_no: Some(11),
                disc_no: None,
                duration_ms: Some(355_000),
                file: pm_model::AudioFile {
                    path: dir.path().join("bo.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(355_000),
                },
                recording_mbid: Some(mbid),
                isrc: None,
                acoustid: None,
                canonical: Some((
                    pm_model::MusicUri::Recording { mbid },
                    pm_model::Confidence::TaggedMbid,
                )),
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            lib.upsert_track(&t).unwrap().0
        };
        let app = router(s.clone());
        let (st, v) = call(&app, "GET", &format!("/v1/library/tracks/{tid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["mbid:b1a9c0e9-d987-4042-ae91-78d6a3267d69"]
        );
        let sources: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["source"].as_str().unwrap())
            .collect();
        assert!(
            sources.contains(&"spotify") && sources.contains(&"youtube") && sources.contains(&"deezer"),
            "{v}"
        );
        assert!(
            sources.contains(&"tidal"),
            "another TIDAL page is still a version"
        );
        let deezer = v
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["source"] == "deezer")
            .unwrap();
        assert_eq!(deezer["uri"], "deezer:track:4329888262");
        assert_eq!(deezer["label"], "Deezer", "a chip names the source");
        // No page twice, even though every item repeats the group's links.
        let mut urls: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["url"].as_str().unwrap())
            .collect();
        let n = urls.len();
        urls.sort();
        urls.dedup();
        assert_eq!(urls.len(), n, "{v}");
        assert!(v
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["source"] != "spotify" || x["uri"].is_null()));
        // Cached: a second read does not ask Pinakes again.
        let _ = call(&app, "GET", &format!("/v1/library/tracks/{tid}/versions"), None).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// Browsers seek by asking for byte ranges; the library file endpoint
    /// must answer 206 with Accept-Ranges or a seek restarts the track.
    #[tokio::test]
    async fn library_files_serve_byte_ranges() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/one.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (st, _) = call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
        assert_eq!(st, StatusCode::OK);
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        let id = tracks["tracks"][0]["id"].as_i64().unwrap();
        let full = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/library/tracks/{id}/file"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(full.status(), StatusCode::OK);
        assert_eq!(full.headers()["accept-ranges"], "bytes");
        let total = full.into_body().collect().await.unwrap().to_bytes().len();
        let part = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/library/tracks/{id}/file"))
                    .header("range", "bytes=100-199")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(part.headers()["content-range"], format!("bytes 100-199/{total}"));
        assert_eq!(part.into_body().collect().await.unwrap().to_bytes().len(), 100);
    }

    /// Behind a reverse proxy with no public_url, the TIDAL redirect URI is
    /// built from the request the browser made.
    #[tokio::test]
    async fn the_tidal_redirect_follows_the_request_host() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        // The node's own app, with its secret: the sign-in comes straight back
        s.tidal.write().unwrap().creds = tidal::TidalCredentials {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            scopes: String::new(),
        };
        let app = router(s);
        let req = Request::builder()
            .uri("/v1/sources/tidal/status")
            .header("host", "music.example")
            .header("x-forwarded-proto", "https")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["redirect_uri"], "https://music.example/tidal/callback");
    }

    /// One label on every identified track; the sync publishes each as a
    /// v1 PubkyAppTag on its canonical MusicBrainz URL, which Pinakes joins.
    #[tokio::test]
    async fn bulk_tagging_covers_identified_tracks_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/one.wav"), 4410); // no identity
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (st, _) = call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
        assert_eq!(st, StatusCode::OK);
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        {
            let lib = s.library.lock().unwrap();
            let mbid = pm_model::Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap();
            let t = pm_model::Track {
                id: None,
                title: "Bohemian Rhapsody".into(),
                artists: vec![],
                album: None,
                track_no: None,
                disc_no: None,
                duration_ms: None,
                file: pm_model::AudioFile {
                    path: dir.path().join("bo.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: None,
                },
                recording_mbid: Some(mbid),
                isrc: None,
                acoustid: None,
                canonical: Some((
                    pm_model::MusicUri::Recording { mbid },
                    pm_model::Confidence::TaggedMbid,
                )),
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            lib.upsert_track(&t).unwrap();
        }
        let (st, r) = call(
            &app,
            "POST",
            "/v1/tags/bulk",
            Some(serde_json::json!({"label": " Library "})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["label"], "library");
        assert_eq!(r["tagged"], 1);
        assert_eq!(r["skipped_no_identity"], 1);
        let (_, r) = call(
            &app,
            "POST",
            "/v1/tags/bulk",
            Some(serde_json::json!({"label": "library"})),
        )
        .await;
        assert_eq!(r["tagged"], 0);
        assert_eq!(r["already"], 1);
        let (_, tags) = call(&app, "GET", "/v1/tags", None).await;
        assert_eq!(
            tags["tags"][0]["uri"],
            "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"
        );
        assert_eq!(tags["tags"][0]["synced"], false);
        let (st, e) = call(
            &app,
            "POST",
            "/v1/tags/bulk",
            Some(serde_json::json!({"label": ""})),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    }

    /// Controllers sign in with Pubky Ring; without a node session there is
    /// no owner to compare against, so the node says so instead of starting.
    #[tokio::test]
    async fn controller_sign_in_needs_the_node_signed_in_first() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), Some("t0k")).await;
        let app = router(s);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/pubky/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
        let (st, v) = call(&app, "GET", "/v1/auth/pubky/poll?id=nope", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["status"], "expired");
    }

    #[tokio::test]
    async fn the_musicbrainz_proxy_needs_a_contact_and_rejects_bad_paths() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/mb/ws/2/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
        s.set_contact("test@example.org").unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/mb/ws/2/..%2Fetc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_discovery_report_explains_an_empty_network() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s);
        let (st, v) = call(&app, "GET", "/v1/outputs/discovery", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["upnp_enabled"], false);
        assert_eq!(v["upnp"].as_array().unwrap().len(), 0);
        assert_eq!(v["public_url"], "http://127.0.0.1:7788");
        let notes: Vec<String> = serde_json::from_value(v["notes"].clone()).unwrap();
        assert!(notes.iter().any(|n| n.contains("both off")), "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("public_url is http://127.0.0.1:7788")),
            "{notes:?}"
        );
    }

    #[tokio::test]
    async fn the_jukebox_zone_is_a_node_setting() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (_, v) = call(&app, "GET", "/v1/node/settings", None).await;
        assert!(v.get("jukebox_zone").is_none());
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"jukebox_zone": "nope"})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, v) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"jukebox_zone": zid})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["jukebox_zone"], zid);
        let (_, v) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"jukebox_zone": ""})),
        )
        .await;
        assert!(v.get("jukebox_zone").is_none());
    }

    /// Shared apps (STEWARD-SHARED-APPS.md): an app without its secret on the node (the project's
    /// TIDAL app, a Spotify app from the guided setup) signs in with PKCE through the return page,
    /// and no secret ever reaches the token endpoint.
    #[tokio::test]
    async fn apps_without_a_secret_sign_in_through_the_return_page() {
        use base64::Engine;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let token = {
            let bodies = bodies.clone();
            move |headers: axum::http::HeaderMap, body: String| {
                let bodies = bodies.clone();
                async move {
                    bodies.lock().unwrap().push(body.clone());
                    if headers.contains_key("authorization") || body.contains("client_secret") {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(serde_json::json!({"error": "invalid_client"})),
                        );
                    }
                    if body.contains("grant_type=client_credentials") {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(serde_json::json!({"error": "invalid_client"})),
                        );
                    }
                    assert!(
                        body.contains("code_verifier=") && body.contains("code=the-code"),
                        "{body}"
                    );
                    (
                        StatusCode::OK,
                        Json(
                            serde_json::json!({"access_token": "user-token", "refresh_token": "r1", "expires_in": 3600, "user_id": 4242, "scope": "x"}),
                        ),
                    )
                }
            }
        };
        let mock = Router::new()
            .route("/v1/oauth2/token", post(token.clone()))
            .route("/api/token", post(token))
            .route("/users/{id}", get(|| async { Json(serde_json::json!({"data": {"id": "4242", "type": "users", "attributes": {"country": "CH"}}})) }))
            .route("/me", get(|| async { Json(serde_json::json!({"id": "listener", "display_name": "Listener", "country": "CH", "product": "premium"})) }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let page = "https://return.example/callback";
        s.set_return_page(page);
        {
            let mut ep = s.tidal.write().unwrap();
            (ep.api_base, ep.auth_base, ep.login_base) = (base.clone(), base.clone(), base.clone());
            ep.creds = tidal::TidalCredentials {
                client_id: "project-app".into(),
                client_secret: String::new(),
                scopes: String::new(),
            };
        }
        {
            let mut ep = s.spotify.write().unwrap();
            (ep.api_base, ep.accounts_base) = (base.clone(), base.clone());
        }
        let app = router(s.clone());
        // The login URL names the return page, and state carries the nonce and the node's callback
        let sign_in = |path: &'static str, callback: &'static str| {
            let app = app.clone();
            async move {
                let (st, l) = call(&app, "GET", path, None).await;
                assert_eq!(st, StatusCode::OK, "{l}");
                let url = url::Url::parse(l["url"].as_str().unwrap()).unwrap();
                let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
                assert_eq!(q["redirect_uri"], page);
                assert_eq!(q["code_challenge_method"], "S256");
                let (_, enc) = q["state"].split_once('.').expect("state = nonce.callback");
                let back = String::from_utf8(
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(enc)
                        .unwrap(),
                )
                .unwrap();
                assert!(back.ends_with(callback), "{back}");
                // The return page forwards every parameter unchanged to that callback
                let req = Request::builder()
                    .uri(format!(
                        "{callback}?code=the-code&state={}",
                        url::form_urlencoded::byte_serialize(q["state"].as_bytes()).collect::<String>()
                    ))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.clone().oneshot(req).await.unwrap();
                resp.headers()
                    .get("location")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            }
        };

        // TIDAL with the project's app: no catalogue before sign-in (client credentials need the secret)
        let (st, _) = call(&app, "GET", "/v1/sources/tidal/search?q=x", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        let (_, stv) = call(&app, "GET", "/v1/sources/tidal/status", None).await;
        assert_eq!(
            stv["redirect_uri"], page,
            "the redirect to register is the return page"
        );
        // The pasted-address fallback: the same parameters, typed in instead of arriving by redirect
        let (_, l) = call(&app, "GET", "/v1/sources/tidal/login", None).await;
        let lurl = url::Url::parse(l["url"].as_str().unwrap()).unwrap();
        let state_param = lurl
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/tidal/callback",
            Some(serde_json::json!({"url": "not a url"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/tidal/callback",
            Some(serde_json::json!({"url": "https://gillohner.github.io/pubky-music-auth/callback?foo=bar"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "no code: {r}");
        let pasted = format!(
            "http://192.168.1.5:7788/tidal/callback?code=the-code&state={}",
            url::form_urlencoded::byte_serialize(state_param.as_bytes()).collect::<String>()
        );
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/tidal/callback",
            Some(serde_json::json!({"url": pasted})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["signed_in"], true, "{r}");
        call(&app, "DELETE", "/v1/sources/tidal/session", None).await;
        assert_eq!(
            sign_in("/v1/sources/tidal/login", "/tidal/callback").await,
            "/?tidal=signed_in"
        );
        let (_, stv) = call(&app, "GET", "/v1/sources/tidal/status", None).await;
        assert_eq!(stv["signed_in"], true, "{stv}");
        assert_eq!(stv["public_client"]["outcome"], "accepted", "{stv}");
        // A state whose nonce the node never issued is refused
        let req = Request::builder()
            .uri("/tidal/callback?code=the-code&state=forged.aHR0cDovL3g")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("error"));

        // Spotify: the guided setup takes only a client id
        let (_, sp) = call(&app, "GET", "/v1/sources/spotify/status", None).await;
        assert_eq!(sp["configured"], false);
        assert_eq!(sp["redirect_uri"], page);
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/app",
            Some(serde_json::json!({"client_id": "not-an-id"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let id = "0123456789abcdef0123456789ABCDEF";
        let (st, sp) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/app",
            Some(serde_json::json!({"client_id": id})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{sp}");
        assert_eq!(sp["app_from"], "settings");
        assert_eq!(sp["client_id"], id.to_ascii_lowercase());
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let row = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "spotify")
            .unwrap()
            .clone();
        assert_eq!(row["status"], "needs sign-in");
        assert_eq!(
            sign_in("/v1/sources/spotify/login", "/spotify/callback").await,
            "/?spotify=ok"
        );
        let (_, sp) = call(&app, "GET", "/v1/sources/spotify/status", None).await;
        assert_eq!(sp["signed_in"], true, "{sp}");
        assert_eq!(sp["display_name"], "Listener");
        assert!(bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.contains("authorization_code"))
            .all(|b| b.contains("client_id=") && !b.contains("client_secret")));
        // Forgetting the app signs out of it
        let (_, sp) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/app",
            Some(serde_json::json!({"client_id": ""})),
        )
        .await;
        assert_eq!(
            (sp["configured"].clone(), sp["signed_in"].clone()),
            (serde_json::json!(false), serde_json::json!(false))
        );
        // An app in the node config can't be replaced from the page
        s.spotify.write().unwrap().creds = spotify::SpotifyCredentials {
            client_id: "cid".into(),
            client_secret: "sec".into(),
        };
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/app",
            Some(serde_json::json!({"client_id": id})),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
    }

    /// Qobuz (unofficial-only): the owner's app and sign-in, search, an album, a signed stream
    /// request for browsers and zones, and the mode switching itself off when Qobuz refuses the app.
    #[tokio::test]
    async fn qobuz_signs_in_searches_and_streams_from_a_mock() {
        let _api_guard = qobuz::API_TEST_LOCK.lock().await;
        use md5::Digest;
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let secret_ok = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let expected_format = Arc::new(std::sync::atomic::AtomicU32::new(27));
        let file_base = base.clone();
        let ok2 = secret_ok.clone();
        let format2 = expected_format.clone();
        let mp3 = b"synthetic MP3 stream";
        let album = serde_json::json!({"id": "0060253780968", "title": "Random Access Memories", "artist": {"name": "Daft Punk"}, "upc": "0886443927087",
            "release_date_original": "2013-05-17", "image": {"large": "https://img/600.jpg"}, "maximum_bit_depth": 24, "maximum_sampling_rate": 88.2,
            "tracks": {"items": [{"id": 5966783, "title": "Get Lucky", "track_number": 8, "duration": 369, "performer": {"name": "Daft Punk"}, "isrc": "USQX91300108"}]}});
        let mock = Router::new()
            .route(
                "/user/login",
                post(|headers: axum::http::HeaderMap, body: String| async move {
                    assert_eq!(headers.get("x-app-id").unwrap(), "100000000");
                    let f: std::collections::HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
                    let want = format!("{:x}", md5::Md5::digest(b"hunter2"));
                    assert_eq!(f["password"], want, "the password goes as its MD5");
                    if f["email"] == "free@example.com" {
                        return Json(serde_json::json!({"user_auth_token": "t-free", "user": {"id": 2, "credential": {"parameters": null}}}));
                    }
                    Json(serde_json::json!({"user_auth_token": "t-1", "user": {"id": 1, "display_name": "Listener", "country_code": "CH",
                        "credential": {"label": "Studio", "parameters": {"lossless_streaming": true, "hires_streaming": true}}}}))
                }),
            )
            .route("/catalog/search", get({ let album = album.clone(); move |headers: axum::http::HeaderMap| { let album = album.clone(); async move {
                assert_eq!(headers.get("x-user-auth-token").unwrap(), "t-1");
                Json(serde_json::json!({"albums": {"items": [album.clone()]}, "tracks": {"items": [{"id": 5966783, "title": "Get Lucky", "duration": 369, "performer": {"name": "Daft Punk"}, "isrc": "USQX91300108", "album": album}]},
                    "artists": {"items": [{"id": 36819, "name": "Daft Punk"}]}}))
            } } }))
            .route("/album/get", get({ let album = album.clone(); move || { let album = album.clone(); async move { Json(album) } } }))
            .route(
                "/track/getFileUrl",
                get(move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let (base, ok, format) = (file_base.clone(), ok2.clone(), format2.clone());
                    async move {
                        let want = format!("{:x}", md5::Md5::digest(format!("trackgetFileUrlformat_id{}intentstreamtrack_id{}{}s3cret", q["format_id"], q["track_id"], q["request_ts"]).as_bytes()));
                        if !ok.load(std::sync::atomic::Ordering::SeqCst) || q["request_sig"] != want {
                            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"status": "error", "code": 400, "message": "Invalid Request Signature parameter (request_sig)"})));
                        }
                        if q["track_id"] == "2" {
                            return (StatusCode::OK, Json(serde_json::json!({"url": format!("{base}/file.flac"), "sample": true})));
                        }
                        let format = format.load(std::sync::atomic::Ordering::SeqCst);
                        assert_eq!(q["format_id"], format.to_string(), "the selected quality reaches Qobuz");
                        let (file, mime) = if format == 5 { ("file.mp3", "audio/mpeg") } else { ("file.flac", "audio/flac") };
                        (StatusCode::OK, Json(serde_json::json!({"url": format!("{base}/{file}"), "format_id": format, "mime_type": mime})))
                    }
                }),
            )
            .route("/file.flac", get({ let f = flac.clone(); move || { let f = f.clone(); async move { f } } }))
            .route("/file.mp3", get(move || async move { mp3.as_slice() }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        qobuz::set_api_base(&base);

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let row = |srcs: &serde_json::Value| {
            srcs.as_array()
                .unwrap()
                .iter()
                .find(|x| x["id"] == "qobuz")
                .unwrap()["status"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        assert!(row(&srcs).starts_with("not configured"), "no built-in app");
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sources/qobuz/unofficial/login",
            Some(serde_json::json!({"email": "a@b.c", "password": "x"})),
        )
        .await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/qobuz/unofficial",
            Some(serde_json::json!({"app_id": "abc", "app_secret": "s3cret"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, q) = call(
            &app,
            "PUT",
            "/v1/sources/qobuz/unofficial",
            Some(serde_json::json!({"app_id": "100000000", "app_secret": "s3cret", "enabled": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        assert_eq!(q["app_from"], "settings");
        assert!(
            q.to_string().find("s3cret").is_none(),
            "the secret never comes back: {q}"
        );
        // A free account is refused; a paid one signs in and only its token is kept
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sources/qobuz/unofficial/login",
            Some(serde_json::json!({"email": "free@example.com", "password": "hunter2"})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, q) = call(
            &app,
            "POST",
            "/v1/sources/qobuz/unofficial/login",
            Some(serde_json::json!({"email": "me@example.com", "password": "hunter2"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        assert_eq!(
            (q["signed_in"].clone(), q["plan"].clone(), q["hires"].clone()),
            (
                serde_json::json!(true),
                serde_json::json!("Studio"),
                serde_json::json!(true)
            )
        );
        let kept = s
            .library
            .lock()
            .unwrap()
            .get_setting(qobuz::STATE_KEY)
            .unwrap()
            .unwrap();
        assert!(
            !kept.contains("hunter2") && !kept.contains("me@example.com"),
            "neither password nor email is stored"
        );
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        assert_eq!(row(&srcs), "signed in");
        // Search, an album, and the stream for browsers
        let (st, r) = call(
            &app,
            "GET",
            "/v1/sources/qobuz/search?q=daft%20punk&limit=10",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["kind"], "artist");
        assert_eq!(items[1]["id"], "album:0060253780968");
        assert_eq!(items[1]["year"], 2013);
        assert_eq!(items[2]["uri"], "qobuz:track:5966783");
        assert_eq!(items[2]["isrc"], "USQX91300108");
        let (st, it) = call(&app, "GET", "/v1/sources/qobuz/items/album:0060253780968", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(it["tracks"][0]["url"], "qobuz:track:5966783");
        assert_eq!(it["tracks"][0]["format"], "FLAC 24-bit/88.2 kHz (Qobuz)");
        let req = Request::builder()
            .uri("/v1/sources/qobuz/stream/5966783")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "audio/flac");
        assert_eq!(
            resp.into_body().collect().await.unwrap().to_bytes().as_ref(),
            flac.as_slice()
        );
        // A zone opens it through the decoder; a sample is refused, not played
        let lib = s.library.clone();
        let opened = tokio::task::spawn_blocking(move || qobuz::open_track_blocking(&lib, "5966783").is_ok())
            .await
            .unwrap();
        assert!(opened);
        let req = Request::builder()
            .uri("/v1/sources/qobuz/stream/2")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        // The browser relay labels the format requested from Qobuz, including MP3 (format 5).
        // Changing quality affects the next request, including switching back to either FLAC mode.
        for (quality, format, mime, bytes) in [
            ("mp3", 5, "audio/mpeg", mp3.as_slice()),
            ("cd", 6, "audio/flac", flac.as_slice()),
            ("max", 27, "audio/flac", flac.as_slice()),
        ] {
            let (st, q) = call(
                &app,
                "PUT",
                "/v1/sources/qobuz/quality",
                Some(serde_json::json!({"id": quality})),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{q}");
            expected_format.store(format, std::sync::atomic::Ordering::SeqCst);
            let req = Request::builder()
                .uri("/v1/sources/qobuz/stream/5966783")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["content-type"], mime, "{quality}");
            assert_eq!(resp.headers()["content-length"], bytes.len().to_string());
            assert_eq!(
                resp.into_body().collect().await.unwrap().to_bytes().as_ref(),
                bytes,
                "{quality}"
            );
        }
        // Qobuz refusing the app switches the source off, says why, and Retry switches it on
        secret_ok.store(false, std::sync::atomic::Ordering::SeqCst);
        let req = Request::builder()
            .uri("/v1/sources/qobuz/stream/5966783")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::BAD_GATEWAY
        );
        let (_, q) = call(&app, "GET", "/v1/sources/qobuz/unofficial", None).await;
        assert_eq!(q["enabled"], false, "{q}");
        assert!(
            q["health"]["detail"].as_str().unwrap().contains("app secret"),
            "{q}"
        );
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        assert!(row(&srcs).starts_with("error:"));
        let (st, _) = call(&app, "GET", "/v1/sources/qobuz/search?q=x", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        secret_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let (_, q) = call(
            &app,
            "PUT",
            "/v1/sources/qobuz/unofficial",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        assert_eq!(
            (q["enabled"].clone(), q["health"].clone()),
            (serde_json::json!(true), serde_json::Value::Null)
        );
        // A qobuz-dl config pasted into the Sign in chooser: its app values and the email with the
        // password's MD5 sign in the same way; the answer names the way, never the token
        let (_, _) = call(&app, "DELETE", "/v1/sources/qobuz/signin", None).await;
        let ini = format!(
            "[DEFAULT]\nemail = me@example.org\npassword = {:x}\napp_id = 100000000\nsecrets = s3cret\n",
            md5::Md5::digest(b"hunter2")
        );
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/qobuz/signin/import",
            Some(serde_json::json!({"text": ini})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["in_use"]["way"], "paste");
        assert_eq!(r["in_use"]["label"], "Pasted config");
        assert_eq!(r["in_use"]["user"], "Listener");
        assert_eq!(r["line"], "Pasted config · hi-res");
        assert_eq!(r["details"]["unofficial"]["app_from"], "import");
        assert!(!r.to_string().contains("t-1"), "{r}");
    }

    /// YouTube search with the owner's own Data API key: the key is checked before it is kept,
    /// never comes back, and search finds music videos that play in This browser.
    #[tokio::test]
    async fn youtube_search_uses_the_owners_key() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        type Q = axum::extract::Query<std::collections::HashMap<String, String>>;
        let good = "AIzaSyD-ci-key-0123456789abcdefghijklmn";
        let mock = Router::new()
            .route("/videos", get(move |Query(q): Q| async move {
                if q["key"] != good {
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": {"message": "API key not valid.", "errors": [{"reason": "keyInvalid"}]}})));
                }
                (StatusCode::OK, Json(serde_json::json!({"items": [{"id": "5NV6Rdv1a3I", "snippet": {"title": "Daft Punk - Get Lucky (Official Audio) ft. Pharrell Williams, Nile Rodgers", "channelTitle": "Daft Punk - Topic", "publishedAt": "2013-05-17T00:00:00Z", "thumbnails": {"high": {"url": "https://i.ytimg.com/vi/5NV6Rdv1a3I/hq.jpg"}}}, "contentDetails": {"duration": "PT6M10S"}}]})))
            }))
            .route("/search", get(move |Query(q): Q| async move {
                assert_eq!((q["type"].as_str(), q["videoCategoryId"].as_str(), q["key"].as_str()), ("video", "10", good));
                Json(serde_json::json!({"items": [{"id": {"kind": "youtube#video", "videoId": "5NV6Rdv1a3I"}, "snippet": {"title": "Daft Punk &amp; Pharrell - Get Lucky", "channelTitle": "Daft Punk - Topic", "publishedAt": "2013-05-17T00:00:00Z", "thumbnails": {"high": {"url": "https://i.ytimg.com/vi/5NV6Rdv1a3I/hq.jpg"}}}}]}))
            }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        youtube::set_api_base(&base);
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (st, _) = call(&app, "GET", "/v1/sources/youtube/search?q=get%20lucky", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED, "no key, no search");
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/youtube/key",
            Some(serde_json::json!({"key": "AIzaSyD-wrong-key-0123456789abcdefghijk"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "a key YouTube refuses is not kept");
        let (st, k) = call(
            &app,
            "PUT",
            "/v1/sources/youtube/key",
            Some(serde_json::json!({"key": good})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(k, serde_json::json!({"set": true}), "the key never comes back");
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let yt = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "youtube")
            .unwrap()
            .clone();
        assert_eq!(yt["status"], "browser + search");
        let (st, r) = call(
            &app,
            "GET",
            "/v1/sources/youtube/search?q=get%20lucky&limit=5",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let hit = &r["items"][0];
        assert_eq!(hit["title"], "Daft Punk & Pharrell - Get Lucky");
        assert_eq!(hit["creator"], "Daft Punk", "a Topic channel is the artist");
        assert_eq!(hit["uri"], "youtube:video:5NV6Rdv1a3I");
        assert_eq!(hit["duration_ms"], 370_000);
        let (st, it) = call(&app, "GET", "/v1/sources/youtube/items/video:5NV6Rdv1a3I", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(it["tracks"][0]["url"], "youtube:video:5NV6Rdv1a3I");
        // Never in the everyday search (quota)
        assert!(!streaming::find("youtube").unwrap().unified(&s));
        let (_, k) = call(
            &app,
            "PUT",
            "/v1/sources/youtube/key",
            Some(serde_json::json!({"key": ""})),
        )
        .await;
        assert_eq!(k["set"], false);
    }

    /// SoundCloud (unofficial): with a client ID the owner entered, search, a set page, likes and
    /// playlists with the owner's token, and streams (progressive MP3 for one track, HLS stitched
    /// for another) from a stand-in api-v2; a refused client switches the mode off.
    #[tokio::test]
    async fn soundcloud_searches_and_streams_from_a_mock() {
        use pm_zones::SourceFactory;
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = {
            let refuse = refuse.clone();
            move |q: &str| -> Option<StatusCode> {
                if refuse.load(std::sync::atomic::Ordering::SeqCst) {
                    return Some(StatusCode::UNAUTHORIZED);
                }
                (!q.contains("client_id=sc-client-0123456789abcdef")).then_some(StatusCode::UNAUTHORIZED)
            }
        };
        let tr = |id: u64, title: &str, kind: &str| {
            let media = match kind {
                "progressive" => {
                    serde_json::json!([{"url": format!("{base}/media/{id}/progressive"), "preset": "mp3_1_0", "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}}])
                }
                "hls" => {
                    serde_json::json!([{"url": format!("{base}/media/{id}/hls"), "preset": "mp3_1_0", "format": {"protocol": "hls", "mime_type": "audio/mpeg"}}])
                }
                _ => {
                    serde_json::json!([{"url": format!("{base}/media/{id}/enc"), "preset": "aac_256k", "format": {"protocol": "encrypted-hls", "mime_type": "audio/mp4"}}])
                }
            };
            serde_json::json!({"id": id, "kind": "track", "title": title, "policy": "ALLOW", "permalink_url": format!("https://soundcloud.com/someone/{}", title.to_lowercase().replace(' ', "-")),
                "duration": 60000, "artwork_url": "https://i1.sndcdn.com/artworks-large.jpg", "created_at": "2024-03-02T10:00:00Z", "playback_count": 7,
                "user": {"username": "Someone"}, "track_authorization": "auth-1", "media": {"transcodings": media}})
        };
        let t1 = tr(1, "Night Drive", "progressive");
        let t2 = tr(2, "Sunrise", "hls");
        let t3 = tr(3, "Locked", "encrypted");
        let set = serde_json::json!({"id": 9, "kind": "playlist", "title": "Late Set", "permalink_url": "https://soundcloud.com/someone/sets/late-set", "user": {"username": "Someone"},
            "track_count": 2, "tracks": [t1.clone(), {"id": 2, "kind": "track"}]});
        let mk = |v: serde_json::Value| {
            let gate = gate.clone();
            move |q: axum::extract::RawQuery| {
                let v = v.clone();
                let gate = gate.clone();
                async move {
                    match gate(q.0.as_deref().unwrap_or("")) {
                        Some(st) => (st, Json(serde_json::json!({"error": "no"}))),
                        None => (StatusCode::OK, Json(v)),
                    }
                }
            }
        };
        let half = flac.len() / 2;
        let (seg1, seg2) = (flac[..half].to_vec(), flac[half..].to_vec());
        let mock = Router::new()
            .route("/search/tracks", get(mk(serde_json::json!({"collection": [t1.clone(), t2.clone(), t3.clone()], "next_href": null}))))
            .route("/search/playlists", get(mk(serde_json::json!({"collection": [set.clone()]}))))
            .route("/resolve", get(mk(t2.clone())))
            .route("/tracks/1", get(mk(t1.clone())))
            .route("/tracks/2", get(mk(t2.clone())))
            .route("/tracks/3", get(mk(t3.clone())))
            .route("/tracks", get(mk(serde_json::json!([t2.clone()]))))
            .route("/playlists/9", get(mk(set.clone())))
            .route("/me", get({
                let g = gate.clone();
                move |q: axum::extract::RawQuery, h: axum::http::HeaderMap| {
                    let g = g.clone();
                    async move {
                        let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
                        if g(q.0.as_deref().unwrap_or("")).is_some() || auth != "OAuth tok-0123456789" {
                            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "bad token"})));
                        }
                        (StatusCode::OK, Json(serde_json::json!({"id": 77, "username": "Someone", "permalink": "someone"})))
                    }
                }
            }))
            .route("/users/77/likes", get(mk(serde_json::json!({"collection": [{"track": t1.clone()}, {"playlist": set.clone()}], "next_href": null}))))
            .route("/users/77/playlists", get(mk(serde_json::json!({"collection": [set.clone()], "next_href": null}))))
            .route("/media/1/progressive", get(mk(serde_json::json!({"url": format!("{base}/cdn/1.mp3")}))))
            .route("/media/2/hls", get(mk(serde_json::json!({"url": format!("{base}/cdn/2.m3u8")}))))
            .route("/cdn/1.mp3", get({ let f = flac.clone(); move || { let f = f.clone(); async move { f } } }))
            .route("/cdn/2.m3u8", get({ let b = base.clone(); move || { let b = b.clone(); async move { format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:30\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:30.0,\n{b}/cdn/seg/1\n#EXTINF:30.0,\n{b}/cdn/seg/2\n#EXT-X-ENDLIST\n") } } }))
            .route("/cdn/seg/1", get({ let f = seg1.clone(); move || { let f = f.clone(); async move { f } } }))
            .route("/cdn/seg/2", get({ let f = seg2.clone(); move || { let f = f.clone(); async move { f } } }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        soundcloud::set_api_base(&base);
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        // Not configured: no client yet; the everyday search leaves it out
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let sc = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "soundcloud")
            .unwrap()
            .clone();
        assert!(
            sc["status"].as_str().unwrap().starts_with("not configured"),
            "{sc}"
        );
        let (st, _) = call(&app, "GET", "/v1/sources/soundcloud/search?q=night", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/soundcloud/unofficial",
            Some(serde_json::json!({"client_id": "short"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (_, u) = call(
            &app,
            "PUT",
            "/v1/sources/soundcloud/unofficial",
            Some(serde_json::json!({"client_id": "sc-client-0123456789abcdef", "enabled": true})),
        )
        .await;
        assert_eq!(
            (
                u["enabled"].clone(),
                u["client_from"].clone(),
                u["signed_in"].clone()
            ),
            (
                serde_json::json!(true),
                serde_json::json!("settings"),
                serde_json::json!(false)
            )
        );
        assert!(crate::search::sources_for(&s, "everywhere").contains(&"soundcloud".to_string()));
        // Search: sets first, then tracks; a pasted link resolves
        let (st, r) = call(
            &app,
            "GET",
            "/v1/sources/soundcloud/search?q=night&limit=10",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let items = r["items"].as_array().unwrap();
        assert_eq!(
            (items[0]["id"].as_str(), items[0]["kind"].as_str()),
            (Some("playlist:9"), Some("item"))
        );
        assert_eq!(items[1]["uri"], "soundcloud:track:1");
        assert_eq!(items[1]["url"], "https://soundcloud.com/someone/night-drive");
        let (_, r) = call(
            &app,
            "GET",
            "/v1/sources/soundcloud/search?q=https%3A%2F%2Fsoundcloud.com%2Fsomeone%2Fsunrise",
            None,
        )
        .await;
        assert_eq!(r["items"][0]["uri"], "soundcloud:track:2");
        // A set page: the stub track is fetched; the encrypted track says so on its own page
        let (_, it) = call(&app, "GET", "/v1/sources/soundcloud/items/playlist:9", None).await;
        assert_eq!(it["tracks"].as_array().unwrap().len(), 2, "{it}");
        assert_eq!(it["tracks"][1]["url"], "soundcloud:track:2");
        assert_eq!(it["tracks"][1]["format"], "MP3 (SoundCloud, HLS)");
        let (_, it) = call(&app, "GET", "/v1/sources/soundcloud/items/track:3", None).await;
        assert!(
            it["tracks"][0]["format"]
                .as_str()
                .unwrap()
                .contains("not playable"),
            "{it}"
        );
        // Streams: progressive for browsers and zones, HLS stitched for both; encrypted refused
        let req = Request::builder()
            .uri("/v1/sources/soundcloud/stream/1")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "audio/mpeg");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.len(), flac.len());
        let req = Request::builder()
            .uri("/v1/sources/soundcloud/stream/2")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.to_vec(), flac, "HLS segments come back as the one file");
        for id in ["1", "2"] {
            let src = s.sources.clone();
            let it = pm_zones::QueueItem::new(format!("soundcloud:track:{id}"));
            let r = tokio::task::spawn_blocking(move || src.open(&it).map(|_| ()).map_err(|e| e.to_string()))
                .await
                .unwrap();
            assert!(r.is_ok(), "track {id}: {r:?}");
        }
        let src = s.sources.clone();
        let it = pm_zones::QueueItem::new("soundcloud:track:3");
        let r = tokio::task::spawn_blocking(move || src.open(&it).map(|_| ()).map_err(|e| e.to_string()))
            .await
            .unwrap();
        assert!(
            r.unwrap_err().contains("Go+"),
            "encrypted tracks are refused, never decrypted"
        );
        let (_, u) = call(&app, "GET", "/v1/sources/soundcloud/unofficial", None).await;
        assert_eq!(u["health"]["ok"], true, "{u}");
        // The owner's token: checked with `me`; a wrong one is dropped
        let (st, r) = call(
            &app,
            "PUT",
            "/v1/sources/soundcloud/unofficial",
            Some(serde_json::json!({"token": "wrong-token-123"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        let (_, u) = call(&app, "GET", "/v1/sources/soundcloud/unofficial", None).await;
        assert_eq!(
            (u["signed_in"].clone(), u["enabled"].clone()),
            (serde_json::json!(false), serde_json::json!(true)),
            "a bad token doesn't switch the mode off: {u}"
        );
        let (st, u) = call(
            &app,
            "PUT",
            "/v1/sources/soundcloud/unofficial",
            Some(serde_json::json!({"token": "tok-0123456789"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{u}");
        assert_eq!(
            (u["signed_in"].clone(), u["user"].clone()),
            (serde_json::json!(true), serde_json::json!("Someone"))
        );
        assert!(
            !u.to_string().contains("tok-0123456789"),
            "the token never comes back: {u}"
        );
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let sc = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "soundcloud")
            .unwrap()
            .clone();
        assert_eq!(sc["status"], "signed in");
        // Likes and playlists, and their imports
        let (st, l) = call(&app, "GET", "/v1/sources/soundcloud/likes", None).await;
        assert_eq!(st, StatusCode::OK, "{l}");
        assert_eq!(
            (
                l["tracks"].as_array().unwrap().len(),
                l["playlists"].as_array().unwrap().len()
            ),
            (1, 1)
        );
        let (_, r) = call(&app, "POST", "/v1/sources/soundcloud/likes/import", None).await;
        assert_eq!(
            (r["imported"].clone(), r["skipped"].clone()),
            (serde_json::json!(2), serde_json::json!(0)),
            "{r}"
        );
        let (_, favs) = call(&app, "GET", "/v1/bookmarks", None).await;
        assert!(
            favs.to_string().contains("soundcloud.com/someone/night-drive"),
            "{favs}"
        );
        let (_, pls) = call(&app, "GET", "/v1/sources/soundcloud/playlists", None).await;
        assert_eq!(
            (pls[0]["id"].clone(), pls[0]["items"].clone()),
            (serde_json::json!("9"), serde_json::json!(2))
        );
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/soundcloud/playlists/9/import?keep_source=true",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["imported"], 2);
        assert_eq!(r["playlist"]["summary"]["name"], "Late Set");
        // SoundCloud refuses the client: the mode switches itself off and says why
        refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        let (st, r) = call(&app, "GET", "/v1/sources/soundcloud/search?q=night", None).await;
        assert_eq!(st, StatusCode::BAD_GATEWAY, "{r}");
        let (_, u) = call(&app, "GET", "/v1/sources/soundcloud/unofficial", None).await;
        assert_eq!(u["enabled"], false);
        assert!(
            u["health"]["detail"]
                .as_str()
                .unwrap()
                .contains("refused the client ID"),
            "{u}"
        );
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let sc = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "soundcloud")
            .unwrap()
            .clone();
        assert!(sc["status"].as_str().unwrap().starts_with("error:"), "{sc}");
        assert!(!crate::search::sources_for(&s, "everywhere").contains(&"soundcloud".to_string()));
        // Try again (on) with SoundCloud answering again
        refuse.store(false, std::sync::atomic::Ordering::SeqCst);
        let (_, u) = call(
            &app,
            "PUT",
            "/v1/sources/soundcloud/unofficial",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        assert!(u["health"].is_null() && u["enabled"] == true, "{u}");
        // Forget the client: back to not configured
        let (_, u) = call(
            &app,
            "PUT",
            "/v1/sources/soundcloud/unofficial",
            Some(serde_json::json!({"client_id": ""})),
        )
        .await;
        assert!(u["client_from"].is_null() && u["signed_in"] == false, "{u}");
        assert!(backup::SECRET_SETTINGS
            .iter()
            .any(|(k, _)| *k == soundcloud::CLIENT_KEY));
    }

    /// Spotify Soloist: the node downloads Spotify's archive (a stand-in here) and unpacks it, runs
    /// Soloist with the owner's key and the device name, reads what plays over `soloist ctl`, says
    /// in plain words when the key is refused or the build has expired, and never returns the key.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn spotify_soloist_runs_as_a_supervised_program() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        // A stand-in soloist: --version; ctl (now --json, pause, play) through the data dir's
        // ws.port; the daemon itself takes only the key "good-key", exits 10 when `expired` sits in
        // its data dir, and otherwise stays up.
        let fake = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "soloist 1.2.3 (build 2026-10-01)"; echo "x86_64-linux"; exit 0; fi
if [ "$1" = "ctl" ]; then
  shift; dir=""; cmd=""
  while [ $# -gt 0 ]; do case "$1" in -D) dir="$2"; shift 2;; --json) shift;; *) cmd="$1"; shift;; esac; done
  [ -f "$dir/ws.port" ] || exit 2
  case "$cmd" in
    now) [ -f "$dir/paired" ] || { echo "not logged in" >&2; exit 3; }
         echo '{"type":"playback_state","status":"playing","volume":50,"is_active":true,"position":{"position_ms":1000,"timestamp_ms":1,"speed":1.0},"item":{"uri":"spotify:track:69kOkLUCkxIZYexIgSG8rq","entity_type":"track","decorations":{"identity":{"name":"Get Lucky"},"creators":[{"decorations":{"identity":{"name":"Daft Punk"}}}],"parent":{"entity":{"decorations":{"identity":{"name":"Random Access Memories"}}}},"playback":{"duration_ms":369000}}}}'; exit 0;;
    pause|play|next|prev) echo "$cmd" >> "$dir/commands"; exit 0;;
    *) exit 1;;
  esac
fi
dir=""; key=""
while [ $# -gt 0 ]; do case "$1" in -D|--data-dir) dir="$2"; shift 2;; -k|--api-key) key="$2"; shift 2;; *) shift;; esac; done
[ "$key" = "good-key-0123456789abcdef" ] || { echo "error: Spotify rejected the API key $key" >&2; exit 1; }
[ -f "$dir/expired" ] && { echo "Spotify Soloist build expired." >&2; exit 10; }
echo 127.0.0.1 > "$dir/ws.addr"; echo 4242 > "$dir/ws.port"; echo $$ > "$dir/soloist.pid"
exec sleep 300
"#;
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir(work.path().join("soloist_release")).unwrap();
        std::fs::write(work.path().join("soloist_release/soloist"), fake).unwrap();
        let tar = std::process::Command::new("tar")
            .arg("-czf")
            .arg(work.path().join("soloist.tar.gz"))
            .arg("-C")
            .arg(work.path())
            .arg("soloist_release")
            .status()
            .unwrap();
        assert!(tar.success());
        let archive = std::fs::read(work.path().join("soloist.tar.gz")).unwrap();
        let asset = if cfg!(target_arch = "aarch64") {
            "soloist_release_arm64.tar.gz"
        } else {
            "soloist_release_x86_64.tar.gz"
        };
        let mock = Router::new().route(
            &format!("/{asset}"),
            get(move || {
                let a = archive.clone();
                async move { a }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        soloist::set_downloads_base(&base);
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let until = |pred: Box<dyn Fn(&serde_json::Value) -> bool>| {
            let app = app.clone();
            async move {
                let mut last = serde_json::Value::Null;
                for _ in 0..60 {
                    let (_, u) = call(&app, "GET", "/v1/sources/spotify/soloist", None).await;
                    if pred(&u) {
                        return u;
                    }
                    last = u;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                panic!("Soloist status never got there: {last}");
            }
        };
        let (_, u) = call(&app, "GET", "/v1/sources/spotify/soloist", None).await;
        assert_eq!(
            (
                u["supported"].clone(),
                u["enabled"].clone(),
                u["running"].clone(),
                u["soloist"].clone(),
                u["can_download"].clone()
            ),
            (
                serde_json::json!(true),
                serde_json::json!(false),
                serde_json::json!(false),
                serde_json::Value::Null,
                serde_json::json!(true)
            ),
            "{u}"
        );
        assert!(u["note"].as_str().unwrap().contains("not on the node's zones"));
        // Download: the archive unpacks and the executable answers --version
        let (st, u) = call(&app, "POST", "/v1/sources/spotify/soloist/binary", None).await;
        assert_eq!(st, StatusCode::OK, "{u}");
        assert_eq!(u["soloist"]["from"], "downloaded");
        assert_eq!(u["soloist"]["version"], "soloist 1.2.3 (build 2026-10-01)");
        // On without a key: it waits and says for what
        let (_, u) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/soloist",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        assert_eq!(u["enabled"], true);
        assert_eq!(u["running"], false);
        assert!(u["waiting_for"].as_str().unwrap().contains("API key"), "{u}");
        // A bad-looking key is refused before anything runs
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/soloist",
            Some(serde_json::json!({"api_key": "short"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // A key Spotify rejects: Soloist exits, the health says so, and the key is nowhere in the answer
        let (st, u) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/soloist",
            Some(serde_json::json!({"api_key": "rejected-key-0123456789", "device_name": "Test speaker"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{u}");
        assert_eq!(u["key_set"], true);
        assert_eq!(u["device_name"], "Test speaker");
        let u = until(Box::new(|u| u["health"]["ok"] == false)).await;
        assert!(
            u["health"]["detail"]
                .as_str()
                .unwrap()
                .contains("rejected the API key"),
            "{u}"
        );
        assert!(
            !u.to_string().contains("rejected-key-0123456789"),
            "the key never comes back: {u}"
        );
        // The right key: it runs, and nobody has picked the device yet
        let (st, u) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/soloist",
            Some(serde_json::json!({"api_key": "good-key-0123456789abcdef"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{u}");
        let u = until(Box::new(|u| u["running"] == true && u["paired"] == false)).await;
        assert!(u["pid"].as_u64().is_some(), "{u}");
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let sp = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "spotify")
            .unwrap()
            .clone();
        assert!(
            sp["status"]
                .as_str()
                .unwrap()
                .contains("Soloist plays on this machine"),
            "{sp}"
        );
        // Picked in a Spotify app: what plays comes through ctl, and the page can pause it
        let sdir = s.data_dir().join("soloist");
        std::fs::write(sdir.join("paired"), "").unwrap();
        let u = until(Box::new(|u| u["paired"] == true)).await;
        assert_eq!(u["now"]["title"], "Get Lucky", "{u}");
        assert_eq!(u["now"]["artists"][0], "Daft Punk");
        assert_eq!(u["now"]["album"], "Random Access Memories");
        assert_eq!(u["now"]["status"], "playing");
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sources/spotify/soloist/control",
            Some(serde_json::json!({"command": "pause"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            std::fs::read_to_string(sdir.join("commands")).unwrap().trim(),
            "pause"
        );
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sources/spotify/soloist/control",
            Some(serde_json::json!({"command": "eject"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // Off: it stops
        let (_, u) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/soloist",
            Some(serde_json::json!({"enabled": false})),
        )
        .await;
        assert_eq!(u["enabled"], false);
        let u = until(Box::new(|u| u["running"] == false)).await;
        assert!(u["waiting_for"].is_null(), "{u}");
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sources/spotify/soloist/control",
            Some(serde_json::json!({"command": "play"})),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        // An expired build: exit 10, said in plain words, no restart loop
        std::fs::write(sdir.join("expired"), "").unwrap();
        let (_, _) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/soloist",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        let u = until(Box::new(|u| u["health"]["ok"] == false)).await;
        assert!(u["health"]["detail"].as_str().unwrap().contains("expired"), "{u}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let (_, u) = call(&app, "GET", "/v1/sources/spotify/soloist", None).await;
        assert_eq!(u["running"], false, "{u}");
        // Remove the download
        let (_, u) = call(&app, "DELETE", "/v1/sources/spotify/soloist/binary", None).await;
        assert!(u["soloist"].is_null(), "{u}");
        assert!(u["waiting_for"].as_str().unwrap().contains("download"), "{u}");
        // The key is a secret in backups
        assert!(backup::SECRET_SETTINGS
            .iter()
            .any(|(k, _)| *k == soloist::KEY_SETTING));
    }

    /// Bandcamp (unofficial): search, an album page read from its data-tralbum, its stream on a zone
    /// and in the browser, then with the owner's sign-in the collection and a purchase downloaded
    /// (a zip) into `<data_dir>/bandcamp`, which becomes a library folder and is scanned.
    #[tokio::test]
    async fn bandcamp_streams_and_downloads_purchases_into_the_library() {
        use pm_zones::SourceFactory;
        use std::io::Write as _;
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let mut zipped = Vec::new();
        {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut zipped));
            z.start_file(
                "Band One - RAM - 01 One.flac",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            z.write_all(&flac).unwrap();
            z.start_file("../escape.flac", zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(b"no").unwrap();
            z.finish().unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let esc = |v: serde_json::Value| v.to_string().replace('&', "&amp;").replace('"', "&quot;");
        let tralbum = esc(
            serde_json::json!({"artist": "Band One", "art_id": 42, "album_release_date": "16 Jan 2016 00:00:00 GMT",
            "current": {"title": "RAM", "about": "An album"},
            "trackinfo": [{"title": "One", "track_num": 1, "duration": 60.0, "file": {"mp3-128": format!("{base}/stream/1.mp3?token=t")}},
                          {"title": "Two", "track_num": 2, "duration": 60.0, "file": null}]}),
        );
        let album_page = format!("<html><body><script data-tralbum=\"{tralbum}\"></script></body></html>");
        let blob = esc(
            serde_json::json!({"digital_items": [{"title": "RAM", "artist": "Band One", "downloads": {
            "mp3-320": {"url": format!("{base}/download/album/1?fmt=mp3")}, "flac": {"url": format!("{base}/download/album/1?fmt=flac")}}}]}),
        );
        let dl_page = format!("<html><div id=\"pagedata\" data-blob=\"{blob}\"></div></html>");
        let cookie_ok = |h: &axum::http::HeaderMap| {
            h.get("cookie").and_then(|v| v.to_str().ok()) == Some("identity=7%09abc%3D%3D")
        };
        let b2 = base.clone();
        let mock = Router::new()
            .route("/api/bcsearch_public_api/1/autocomplete_elastic", post(move |Json(b): Json<serde_json::Value>| async move {
                assert_eq!(b["search_text"], "ram");
                Json(serde_json::json!({"auto": {"results": [
                    {"type": "b", "name": "Band One", "item_url_path": "https://band1.bandcamp.com"},
                    {"type": "a", "name": "RAM", "band_name": "Band One", "item_url_path": "https://band1.bandcamp.com/album/ram", "img": "https://f4.bcbits.com/img/9_3.jpg"},
                    {"type": "t", "name": "Elsewhere", "band_name": "X", "item_url_path": "https://custom.example/track/x"}]}}))
            }))
            .route("/_band/band1/album/ram", get(move || { let p = album_page.clone(); async move { axum::response::Html(p) } }))
            .route("/_band/band1/album/changed", get(|| async { axum::response::Html("<html>new layout</html>") }))
            .route("/stream/1.mp3", get({ let f = flac.clone(); move || { let f = f.clone(); async move { f } } }))
            .route("/api/fan/2/collection_summary", get(move |h: axum::http::HeaderMap| async move {
                if !cookie_ok(&h) { return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": true}))); }
                (StatusCode::OK, Json(serde_json::json!({"fan_id": 77, "collection_summary": {"fan_id": 77, "username": "listener"}})))
            }))
            .route("/api/fancollection/1/collection_items", post(move |h: axum::http::HeaderMap, Json(b): Json<serde_json::Value>| {
                let base = b2.clone();
                async move {
                    assert!(cookie_ok(&h));
                    assert_eq!(b["fan_id"], 77);
                    let first = b["older_than_token"].as_str().unwrap().ends_with("::a::") && !b["older_than_token"].as_str().unwrap().starts_with("1::");
                    if first {
                        Json(serde_json::json!({"items": [{"item_title": "RAM", "band_name": "Band One", "item_url": "https://band1.bandcamp.com/album/ram", "item_type": "album", "sale_item_type": "p", "sale_item_id": 5, "item_art_id": 42}],
                            "redownload_urls": {"p5": format!("{base}/dl-page?payment_id=5")}, "more_available": true, "last_token": "1::a::"}))
                    } else {
                        Json(serde_json::json!({"items": [{"item_title": "Gift", "band_name": "Other", "item_url": "https://other.bandcamp.com/track/gift", "item_type": "track", "sale_item_type": "r", "sale_item_id": 6}],
                            "redownload_urls": {}, "more_available": false, "last_token": "0::a::"}))
                    }
                }
            }))
            .route("/dl-page", get(move |h: axum::http::HeaderMap| { let p = dl_page.clone(); async move { assert!(cookie_ok(&h)); axum::response::Html(p) } }))
            .route("/statdownload/album/1", get({ let b = base.clone(); move |q: axum::extract::RawQuery| { let b = b.clone(); async move {
                let q = q.0.unwrap_or_default();
                assert!(q.contains("fmt=flac") && q.contains(".vrs=1") && q.contains(".rand="), "{q}");
                format!("if ( window.Downloads ) {{ Downloads.statResult ( {{\"result\":\"ok\",\"download_url\":\"{b}/files/ram.zip\"}} ) }};")
            } } }))
            .route("/files/ram.zip", get(move || { let z = zipped.clone(); async move { z } }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        bandcamp::set_site_base(&base);
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        // Off until switched on
        let (st, _) = call(&app, "GET", "/v1/sources/bandcamp/search?q=ram", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        let (_, u) = call(
            &app,
            "PUT",
            "/v1/sources/bandcamp/unofficial",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        assert_eq!(
            (u["enabled"].clone(), u["signed_in"].clone(), u["format"].clone()),
            (
                serde_json::json!(true),
                serde_json::json!(false),
                serde_json::json!("flac")
            )
        );
        // Search: albums and tracks on bandcamp.com only
        let (st, r) = call(&app, "GET", "/v1/sources/bandcamp/search?q=ram", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let items = r["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{r}");
        assert_eq!(
            (items[0]["id"].as_str(), items[0]["image"].as_str()),
            (
                Some("album:band1.ram"),
                Some("https://f4.bcbits.com/img/9_10.jpg")
            )
        );
        // The album page: the streamable track only
        let (st, it) = call(&app, "GET", "/v1/sources/bandcamp/items/album:band1.ram", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(
            (it["title"].as_str(), it["year"].as_i64()),
            (Some("RAM"), Some(2016))
        );
        assert_eq!(it["tracks"].as_array().unwrap().len(), 1);
        assert_eq!(it["tracks"][0]["url"], "bandcamp:track:band1.a.ram.1");
        assert_eq!(it["url"], "https://band1.bandcamp.com/album/ram");
        // Its stream: in the browser (relayed) and on a zone
        let req = Request::builder()
            .uri("/v1/sources/bandcamp/stream/band1.a.ram.1")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            flac.len()
        );
        let src = s.sources.clone();
        let it = pm_zones::QueueItem::new("bandcamp:track:band1.a.ram.1");
        let r = tokio::task::spawn_blocking(move || src.open(&it).map(|_| ()).map_err(|e| e.to_string()))
            .await
            .unwrap();
        assert!(r.is_ok(), "{r:?}");
        let req = Request::builder()
            .uri("/v1/sources/bandcamp/stream/band1.a.ram.2")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "a track the artist doesn't stream"
        );
        // The sign-in: a wrong cookie is refused; the right one names the fan, and never comes back
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/bandcamp/unofficial",
            Some(serde_json::json!({"identity": "wrong"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, u) = call(
            &app,
            "PUT",
            "/v1/sources/bandcamp/unofficial",
            Some(serde_json::json!({"identity": "identity=7%09abc%3D%3D"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{u}");
        assert_eq!(
            (u["signed_in"].clone(), u["user"].clone()),
            (serde_json::json!(true), serde_json::json!("listener"))
        );
        assert!(!u.to_string().contains("7%09abc"), "{u}");
        // The collection, across pages
        let (st, c) = call(&app, "GET", "/v1/sources/bandcamp/collection", None).await;
        assert_eq!(st, StatusCode::OK, "{c}");
        let c = c.as_array().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(
            (
                c[0]["id"].as_str(),
                c[0]["downloadable"].as_bool(),
                c[0]["item"].as_str()
            ),
            (Some("p5"), Some(true), Some("album:band1.ram"))
        );
        assert_eq!(c[1]["downloadable"], false);
        // Download the purchase: FLAC, unpacked (nothing outside its folder), and scanned into the library
        let (st, j) = call(&app, "POST", "/v1/sources/bandcamp/collection/p5/download", None).await;
        assert_eq!(st, StatusCode::OK, "{j}");
        let mut last = serde_json::Value::Null;
        for _ in 0..100 {
            let (_, d) = call(&app, "GET", "/v1/sources/bandcamp/downloads", None).await;
            last = d[0].clone();
            if last["state"] == "done" || last["state"] == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(last["state"], "done", "{last}");
        assert_eq!(last["format"], "flac");
        let folder = std::path::PathBuf::from(last["folder"].as_str().unwrap());
        assert!(
            folder.join("Band One - RAM - 01 One.flac").is_file(),
            "{}",
            folder.display()
        );
        assert!(
            !folder.parent().unwrap().join("escape.flac").exists(),
            "zip entries stay inside the folder"
        );
        let (mut folders, mut tracks) = (serde_json::Value::Null, serde_json::Value::Null);
        for _ in 0..100 {
            folders = call(&app, "GET", "/v1/library/folders", None).await.1;
            tracks = call(&app, "GET", "/v1/library/tracks", None).await.1;
            if folders.to_string().contains("bandcamp") && tracks.to_string().contains("01 One") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            folders.to_string().contains("bandcamp"),
            "a library folder: {folders}"
        );
        assert!(
            tracks.to_string().contains("01 One"),
            "the purchase is in the library: {tracks}"
        );
        let (_, c) = call(&app, "GET", "/v1/sources/bandcamp/collection", None).await;
        assert_eq!(c[0]["downloaded"], true);
        let (st, _) = call(&app, "POST", "/v1/sources/bandcamp/collection/r6/download", None).await;
        assert_eq!(st, StatusCode::CONFLICT, "no download offered");
        // A page that no longer carries the data switches the mode off
        let (st, _) = call(
            &app,
            "GET",
            "/v1/sources/bandcamp/items/album:band1.changed",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_GATEWAY);
        let (_, u) = call(&app, "GET", "/v1/sources/bandcamp/unofficial", None).await;
        assert_eq!(u["enabled"], false);
        assert!(
            u["health"]["detail"].as_str().unwrap().contains("pages changed"),
            "{u}"
        );
        assert!(backup::SECRET_SETTINGS
            .iter()
            .any(|(k, _)| *k == bandcamp::COOKIE_KEY));
    }

    /// librespot (unofficial) as an external program: a stand-in librespot pipes PCM and reports its
    /// events through the node's `--onevent` script; the chosen zone starts the live item, shows the
    /// track's title, pauses and stops with it; off stops librespot.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn librespot_pipes_into_a_zone_and_the_zone_follows_its_events() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-librespot");
        // --version; otherwise: find --onevent and the pipe backend, report a session and a track,
        // play (2 s of a 440 Hz-ish square wave on stdout), pause, and wait
        std::fs::write(&bin, r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "librespot 0.8.0 test"; exit 0; fi
ev=""; pipe=0; bitrate=""
while [ $# -gt 0 ]; do case "$1" in --onevent) ev="$2"; shift 2;; --backend) [ "$2" = pipe ] && pipe=1; shift 2;; --bitrate) bitrate="$2"; shift 2;; *) shift;; esac; done
[ -n "$ev" ] || { echo "no onevent" >&2; exit 3; }
[ "$bitrate" = 160 ] || { echo "wrong bitrate: $bitrate" >&2; exit 3; }
PLAYER_EVENT=session_connected USER_NAME=listener "$ev"
PLAYER_EVENT=track_changed NAME="Get Lucky" ARTISTS="Daft Punk
Pharrell Williams" ALBUM=RAM DURATION_MS=369000 URI=spotify:track:69kOkLUCkxIZYexIgSG8rq "$ev"
PLAYER_EVENT=playing POSITION_MS=0 "$ev"
if [ "$pipe" = 1 ]; then head -c 352800 /dev/zero | tr '\000' '\100'; fi
sleep 2
PLAYER_EVENT=paused POSITION_MS=2000 "$ev"
exec sleep 300
"#).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        librespot::set_config_path(bin.to_str().unwrap());
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (_, u) = call(&app, "GET", "/v1/sources/spotify/librespot", None).await;
        assert_eq!(
            (u["enabled"].clone(), u["output"].clone(), u["running"].clone()),
            (
                serde_json::json!(false),
                serde_json::json!("local"),
                serde_json::json!(false)
            )
        );
        assert_eq!(u["librespot"]["version"], "librespot 0.8.0 test");
        assert!(u["warning"].as_str().unwrap().contains("not approved by Spotify"));
        // The selected quality must reach both the process and the player's stream metadata.
        let (st, q) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/quality",
            Some(serde_json::json!({"id": "160"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        // Zone output without a zone is refused; with one it runs
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/librespot",
            Some(serde_json::json!({"zone_id": "nope"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, u) = call(&app, "PUT", "/v1/sources/spotify/librespot", Some(serde_json::json!({"output": "zone", "zone_id": zid, "device_name": "Test speaker", "enabled": true}))).await;
        assert_eq!(st, StatusCode::OK, "{u}");
        assert_eq!(
            (u["running"].clone(), u["device_name"].clone()),
            (serde_json::json!(true), serde_json::json!("Test speaker")),
            "{u}"
        );
        // The zone starts the live item and shows the track
        let mut z = serde_json::Value::Null;
        for _ in 0..60 {
            z = call(&app, "GET", &format!("/v1/zones/{zid}"), None).await.1;
            if z["now_playing"]["item"]["uri"] == librespot::LIVE_URI && z["transport"] == "playing" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(z["now_playing"]["item"]["uri"], librespot::LIVE_URI, "{z}");
        assert_eq!(z["transport"], "playing", "{z}");
        assert_eq!(z["now_playing"]["format"]["bitrate_kbps"], 160, "{z}");
        let (_, u) = call(&app, "GET", "/v1/sources/spotify/librespot", None).await;
        assert_eq!(u["user"], "listener");
        assert_eq!(u["now"]["title"], "Get Lucky");
        assert_eq!(
            u["now"]["artists"],
            serde_json::json!(["Daft Punk", "Pharrell Williams"])
        );
        // librespot pauses: so does the zone
        for _ in 0..60 {
            z = call(&app, "GET", &format!("/v1/zones/{zid}"), None).await.1;
            if z["transport"] == "paused" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(z["transport"], "paused", "{z}");
        // Off: librespot stops, and the zone with it
        let (_, u) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/librespot",
            Some(serde_json::json!({"enabled": false})),
        )
        .await;
        assert_eq!(u["running"], false, "{u}");
        let z = call(&app, "GET", &format!("/v1/zones/{zid}"), None).await.1;
        assert_eq!(z["transport"], "stopped", "{z}");
        // The live item refuses to open while librespot isn't piping
        assert!(librespot::open_live().is_err());
    }

    /// Last.fm: the owner's app, the web sign-in through `/lastfm/callback` (a signed getSession),
    /// a play scrobbled when it ends (signed, with the session), a stale session noticed and the
    /// scrobble kept until the next sign-in, similar artists matched to the library; the secret
    /// and the session key never come back.
    #[tokio::test]
    async fn lastfm_scrobbles_and_finds_similar_artists_from_a_mock() {
        let _api_guard = lastfm::API_TEST_LOCK.lock().await;
        const KEY: &str = "0123456789abcdef0123456789abcdef";
        const SECRET: &str = "fedcba9876543210fedcba9876543210";
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        type Call = std::collections::HashMap<String, String>;
        type Calls = Arc<std::sync::Mutex<Vec<Call>>>;
        type Pred = Box<dyn Fn(&[Call]) -> bool + Send>;
        let calls: Calls = Default::default();
        let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mock = Router::new()
            .route(
                "/",
                post({
                    let calls = calls.clone();
                    let refuse = refuse.clone();
                    move |body: String| {
                        let calls = calls.clone();
                        let refuse = refuse.clone();
                        async move {
                            let p: std::collections::HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
                                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                                .collect();
                            // Every signed call: sorted names and values, then the secret
                            let signed: Vec<(String, String)> = p.iter().filter(|(k, _)| *k != "api_sig" && *k != "format").map(|(k, v)| (k.clone(), v.clone())).collect();
                            assert_eq!(p.get("api_sig").map(String::as_str), Some(lastfm::sign(&signed, SECRET).as_str()), "bad signature: {p:?}");
                            assert_eq!(p.get("api_key").map(String::as_str), Some(KEY));
                            let method = p.get("method").cloned().unwrap_or_default();
                            calls.lock().unwrap().push(p.clone());
                            match method.as_str() {
                                "auth.getSession" => {
                                    assert_eq!(p.get("token").map(String::as_str), Some("tok123"));
                                    Json(serde_json::json!({"session": {"name": "listener", "key": "sess-key-xyz", "subscriber": 0}}))
                                }
                                "track.updateNowPlaying" => {
                                    assert_eq!(p.get("sk").map(String::as_str), Some("sess-key-xyz"));
                                    Json(serde_json::json!({"nowplaying": {"artist": {"#text": p.get("artist")}}}))
                                }
                                "user.getInfo" => {
                                    if p.get("sk").map(String::as_str) == Some("offline") {
                                        Json(serde_json::json!({"error": 11, "message": "temporarily offline"}))
                                    } else if p.get("sk").map(String::as_str) == Some("malformed") {
                                        Json(serde_json::json!({"user": {}}))
                                    } else if p.get("sk").map(String::as_str) == Some("sess-key-xyz") {
                                        Json(serde_json::json!({"user": {"name": "listener", "playcount": "1"}}))
                                    } else {
                                        Json(serde_json::json!({"error": 9, "message": "Invalid session key - Please re-authenticate"}))
                                    }
                                }
                                "track.scrobble" => {
                                    assert_eq!(p.get("sk").map(String::as_str), Some("sess-key-xyz"));
                                    if refuse.load(std::sync::atomic::Ordering::SeqCst) {
                                        Json(serde_json::json!({"error": 9, "message": "Invalid session key - Please re-authenticate"}))
                                    } else {
                                        let n = p.keys().filter(|k| k.starts_with("track[")).count();
                                        Json(serde_json::json!({"scrobbles": {"@attr": {"accepted": n, "ignored": 0}}}))
                                    }
                                }
                                _ => Json(serde_json::json!({"error": 3, "message": "Invalid Method"})),
                            }
                        }
                    }
                })
                .get(|q: axum::extract::RawQuery| async move {
                    let p: std::collections::HashMap<String, String> = url::form_urlencoded::parse(q.0.unwrap_or_default().as_bytes())
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    assert_eq!(p.get("method").map(String::as_str), Some("artist.getSimilar"), "{p:?}");
                    assert_eq!(p.get("api_key").map(String::as_str), Some(KEY));
                    assert_eq!(p.get("artist").map(String::as_str), Some("Radiohead"), "no mbid: by name");
                    Json(serde_json::json!({"similarartists": {"artist": [
                        {"name": "Portishead", "mbid": "8f6bd1e4-fbe1-4f50-aa9b-94c450ec0f11", "match": "0.9", "url": "https://www.last.fm/music/Portishead"},
                        {"name": "Thom Yorke", "mbid": "", "match": 0.5, "url": "https://www.last.fm/music/Thom+Yorke"}
                    ], "@attr": {"artist": "Radiohead"}}}))
                }),
            )
            .route("/song.flac", get({ let f = flac.clone(); move || { let f = f.clone(); async move { f } } }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        lastfm::set_api_base(&format!("{base}/"));
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        // Two library artists, one of them among the similar ones
        let aid = {
            let lib = s.library.lock().unwrap();
            let mk = |title: &str, artist: &str| pm_model::Track {
                id: None,
                title: title.into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: artist.into(),
                    mbid: None,
                }],
                album: None,
                track_no: None,
                disc_no: None,
                duration_ms: Some(200_000),
                file: pm_model::AudioFile {
                    path: dir.path().join(format!("{title}.flac")),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(200_000),
                },
                recording_mbid: None,
                isrc: None,
                acoustid: None,
                canonical: None,
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            lib.upsert_track(&mk("Creep", "Radiohead")).unwrap();
            lib.upsert_track(&mk("Glory Box", "Portishead")).unwrap();
            lib.list_artists()
                .unwrap()
                .into_iter()
                .find(|a| a.name == "Radiohead")
                .unwrap()
                .id
        };
        // Not configured
        let (_, lf) = call(&app, "GET", "/v1/lastfm", None).await;
        assert_eq!(
            (lf["configured"].clone(), lf["connected"].clone()),
            (serde_json::json!(false), serde_json::json!(false))
        );
        let (_, sim) = call(&app, "GET", &format!("/v1/library/artists/{aid}/similar"), None).await;
        assert_eq!(sim["note"], "not configured");
        let (st, _) = call(&app, "GET", "/v1/lastfm/login", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/lastfm",
            Some(serde_json::json!({"api_key": "short", "api_secret": SECRET})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, lf) = call(
            &app,
            "PUT",
            "/v1/lastfm",
            Some(serde_json::json!({"api_key": KEY, "api_secret": SECRET})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{lf}");
        assert_eq!(
            (
                lf["configured"].clone(),
                lf["app_from"].clone(),
                lf["api_key"].clone()
            ),
            (
                serde_json::json!(true),
                serde_json::json!("settings"),
                serde_json::json!(KEY)
            )
        );
        assert!(
            !lf.to_string().contains(SECRET),
            "the secret never comes back: {lf}"
        );
        // Sign in: Last.fm's page, then the callback with the token
        let (st, l) = call(&app, "GET", "/v1/lastfm/login", None).await;
        assert_eq!(st, StatusCode::OK, "{l}");
        let url = l["url"].as_str().unwrap();
        assert!(
            url.starts_with("https://www.last.fm/api/auth/?")
                && url.contains(&format!("api_key={KEY}"))
                && url.contains("%2Flastfm%2Fcallback"),
            "{url}"
        );
        let req = Request::builder()
            .uri("/lastfm/callback?token=tok123")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers()["location"], "/?lastfm=signed_in&user=listener");
        let (_, lf) = call(&app, "GET", "/v1/lastfm", None).await;
        assert_eq!(
            (lf["connected"].clone(), lf["user"].clone(), lf["submit"].clone()),
            (
                serde_json::json!(true),
                serde_json::json!("listener"),
                serde_json::json!(true)
            )
        );
        assert!(
            !lf.to_string().contains("sess-key"),
            "the session key never comes back: {lf}"
        );
        // A play to its end: now playing at the start, the scrobble when it ends
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let play = |title: &str| serde_json::json!({"items": [{"uri": format!("{base}/song.flac"), "title": title, "artist": "Someone", "album": "Nights"}], "replace": true, "play": true});
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(play("Night Drive")),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let wait_for = |pred: Pred| {
            let calls = calls.clone();
            async move {
                for _ in 0..100 {
                    if pred(&calls.lock().unwrap()) {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                panic!(
                    "Last.fm never got it: {:?}",
                    calls
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|c| c.get("method").cloned().unwrap_or_default())
                        .collect::<Vec<_>>()
                );
            }
        };
        wait_for(Box::new(|c| {
            c.iter()
                .any(|p| p.get("method").map(String::as_str) == Some("track.scrobble"))
        }))
        .await;
        {
            let c = calls.lock().unwrap();
            let np = c
                .iter()
                .find(|p| p.get("method").map(String::as_str) == Some("track.updateNowPlaying"))
                .expect("now playing");
            assert_eq!(
                (
                    np.get("artist").map(String::as_str),
                    np.get("track").map(String::as_str)
                ),
                (Some("Someone"), Some("Night Drive"))
            );
            assert!(!np.contains_key("chosenByUser"));
            let sc = c
                .iter()
                .find(|p| p.get("method").map(String::as_str) == Some("track.scrobble"))
                .unwrap();
            assert_eq!(sc.get("artist[0]").map(String::as_str), Some("Someone"));
            assert_eq!(sc.get("track[0]").map(String::as_str), Some("Night Drive"));
            assert_eq!(sc.get("album[0]").map(String::as_str), Some("Nights"));
            assert_eq!(sc.get("chosenByUser[0]").map(String::as_str), Some("1"));
            assert!(sc.get("timestamp[0]").is_some_and(|t| t.parse::<i64>().is_ok()));
        }
        // Last.fm drops the session: the scrobble waits and the card says to sign in again
        refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        calls.lock().unwrap().clear();
        let (_, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(play("Second")),
        )
        .await;
        wait_for(Box::new(|c| {
            c.iter()
                .any(|p| p.get("method").map(String::as_str) == Some("track.scrobble"))
        }))
        .await;
        let mut lf = serde_json::Value::Null;
        for _ in 0..50 {
            lf = call(&app, "GET", "/v1/lastfm", None).await.1;
            if lf["pending"] == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(lf["pending"], 1, "{lf}");
        assert!(lf["problem"].as_str().unwrap().contains("sign in again"), "{lf}");
        // Signing in again clears it; the waiting scrobble goes out with the next play
        refuse.store(false, std::sync::atomic::Ordering::SeqCst);
        calls.lock().unwrap().clear();
        let req = Request::builder()
            .uri("/lastfm/callback?token=tok123")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::SEE_OTHER
        );
        let (_, lf) = call(&app, "GET", "/v1/lastfm", None).await;
        assert!(lf["problem"].is_null(), "{lf}");
        let (_, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(play("Third")),
        )
        .await;
        wait_for(Box::new(|c| {
            c.iter().any(|p| {
                p.get("method").map(String::as_str) == Some("track.scrobble") && p.contains_key("track[1]")
            })
        }))
        .await;
        {
            let c = calls.lock().unwrap();
            let sc = c.iter().find(|p| p.contains_key("track[1]")).unwrap();
            let titles: std::collections::HashSet<&str> = [sc["track[0]"].as_str(), sc["track[1]"].as_str()]
                .into_iter()
                .collect();
            assert_eq!(titles, ["Second", "Third"].into_iter().collect());
        }
        let (_, lf) = call(&app, "GET", "/v1/lastfm", None).await;
        assert_eq!(lf["pending"], 0, "{lf}");
        // Similar artists, matched to the library by name
        let (st, sim) = call(&app, "GET", &format!("/v1/library/artists/{aid}/similar"), None).await;
        assert_eq!(st, StatusCode::OK, "{sim}");
        let arts = sim["artists"].as_array().unwrap();
        assert_eq!(arts.len(), 2);
        assert_eq!(arts[0]["name"], "Portishead");
        assert!(arts[0]["library_artist_id"].as_i64().is_some(), "{sim}");
        assert_eq!(arts[0]["score"], 0.9);
        assert!(arts[1]["library_artist_id"].is_null());
        // Sign out: the app stays, the session and the queue go
        let (_, lf) = call(&app, "DELETE", "/v1/lastfm", None).await;
        assert_eq!(
            (
                lf["connected"].clone(),
                lf["configured"].clone(),
                lf["pending"].clone()
            ),
            (
                serde_json::json!(false),
                serde_json::json!(true),
                serde_json::json!(0)
            )
        );
        assert!(
            backup::SECRET_SETTINGS.iter().any(|(k, _)| *k == lastfm::APP_KEY)
                && backup::SECRET_SETTINGS
                    .iter()
                    .any(|(k, _)| *k == lastfm::SESSION_KEY)
        );
        // A sign-in pasted into the Sign in chooser (key, secret, session key): checked with
        // user.getInfo, then kept like the node's own; a dead session key is refused
        call(&app, "DELETE", "/v1/sources/lastfm/signin", None).await;
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/lastfm/signin/import",
            Some(serde_json::json!({"text": format!("{KEY}\n{SECRET}\nsess-key-xyz\n")})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["in_use"]["way"], "paste");
        assert_eq!(r["in_use"]["user"], "listener");
        assert_eq!(r["line"], "Pasted sign-in · listener");
        assert!(
            !r.to_string().contains("sess-key-xyz") && !r.to_string().contains(SECRET),
            "{r}"
        );
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/lastfm/signin/import",
            Some(serde_json::json!({"text": format!("{KEY}\n{SECRET}\ndeadkey\n")})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert!(r["error"].as_str().unwrap().contains("did not accept"), "{r}");
        for key in ["offline", "malformed"] {
            let (st, _) = call(
                &app,
                "POST",
                "/v1/sources/lastfm/signin/import",
                Some(serde_json::json!({"text": format!("{KEY}\n{SECRET}\n{key}\n")})),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST);
            let (_, current) = call(&app, "GET", "/v1/sources/lastfm/signin", None).await;
            assert_eq!(
                current["in_use"]["user"], "listener",
                "failed imports preserve the working sign-in"
            );
        }
    }

    /// Every source declares what it can do and a stage; the Sources list carries both.
    #[tokio::test]
    async fn sources_declare_their_features_and_stage() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, rows) = call(&app, "GET", "/v1/sources", None).await;
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), streaming::services().len());
        for r in rows {
            let feats = r["features"].as_array().unwrap();
            assert!(!feats.is_empty(), "{} declares nothing", r["id"]);
            for f in feats {
                assert!(
                    streaming::FEATURES.contains(&f.as_str().unwrap()),
                    "{}: unknown feature {f}",
                    r["id"]
                );
            }
            assert!(
                streaming::STAGES.contains(&r["stage"].as_str().unwrap()),
                "{}: stage {}",
                r["id"],
                r["stage"]
            );
        }
        let by = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap().clone();
        assert_eq!(by("qobuz")["stage"], "unofficial");
        assert_eq!(by("tidal")["stage"], "stable");
        assert!(by("spotify")["features"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("connect")));
        assert!(!by("spotify")["features"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("play")));
        assert!(by("bandcamp")["features"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("downloads")));
    }

    /// Artist pages fall back to fanart.tv (the project's key) for a portrait and to TheAudioDB for
    /// a portrait and a biography when Wikidata and Wikipedia have none.
    #[tokio::test]
    async fn artist_images_fall_back_to_fanart_and_theaudiodb() {
        let mbid = "056e4f3e-d505-4dad-8ec1-d04f521cbb56";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0, 0, 0, 0, 1, 2, 3];
        let b2 = base.clone();
        let mock = Router::new()
            .route(&format!("/ws/2/artist/{mbid}"), get(|| async { Json(serde_json::json!({"id": "x", "relations": []})) }))
            .route(&format!("/v3/music/{mbid}"), get({ let b = b2.clone(); move |q: axum::extract::RawQuery| { let b = b.clone(); async move {
                assert!(q.0.unwrap_or_default().contains("api_key=fanart-test-key"));
                Json(serde_json::json!({"name": "Daft Punk", "artistthumb": [
                    {"id": "1", "url": format!("{b}/img/less.png"), "likes": "2"},
                    {"id": "2", "url": format!("{b}/img/best.png"), "likes": "7"}]}))
            } } }))
            .route("/api/v1/json/123/artist-mb.php", get(move |q: axum::extract::RawQuery| async move {
                assert!(q.0.unwrap_or_default().contains(&format!("i={mbid}")));
                Json(serde_json::json!({"artists": [{"strArtist": "Daft Punk", "strArtistThumb": format!("{b2}/img/adb.png"), "strBiography": "Daft Punk were a French electronic music duo."}]}))
            }))
            .route("/img/{name}", get({ let png = png.clone(); move |Path(name): Path<String>| { let png = png.clone(); async move {
                assert_eq!(name, "best.png", "the most liked portrait");
                ([(axum::http::header::CONTENT_TYPE, "image/png")], png)
            } } }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        {
            let mut b = s.info_bases.write().unwrap();
            b.musicbrainz = base.clone();
            b.wikidata = "http://127.0.0.1:9".into();
            b.wikipedia = "http://127.0.0.1:9".into();
            b.fanart = base.clone();
            b.audiodb = base.clone();
            b.fanart_key = Some("fanart-test-key".into());
        }
        s.set_contact("tests@example.org").ok();
        let aid = {
            let lib = s.library.lock().unwrap();
            let mut t = untagged_track(dir.path().join("song.flac"), "Song", "Daft Punk");
            t.artists[0].mbid = Some(pm_model::Mbid::parse(mbid).unwrap());
            lib.upsert_track(&t).unwrap();
            lib.list_artists().unwrap()[0].id
        };
        let app = router(s.clone());
        let mut info = serde_json::Value::Null;
        for _ in 0..50 {
            info = call(&app, "GET", &format!("/v1/library/artists/{aid}/info"), None)
                .await
                .1;
            if info["image"] == true || info["bio"].is_string() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(info["image"], true, "{info}");
        assert_eq!(info["image_from"], "fanart.tv");
        assert_eq!(info["bio_from"], "TheAudioDB");
        assert!(
            info["bio"].as_str().unwrap().starts_with("Daft Punk were"),
            "{info}"
        );
        assert!(info["note"].is_null(), "{info}");
        let req = Request::builder()
            .uri(format!("/v1/library/artists/{aid}/image"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            11
        );
        assert_eq!(
            artist_info::fanart_thumb(&serde_json::json!({"artistbackground": [{"url": "u", "likes": "1"}]}))
                .as_deref(),
            Some("u")
        );
        assert!(artist_info::fanart_thumb(&serde_json::json!({})).is_none());
    }

    /// YouTube on the zones (unofficial): yt-dlp downloads from the (stood-in) official release
    /// only when its checksum matches, and a zone then plays a video through it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn youtube_plays_on_zones_through_a_checked_ytdlp() {
        use pm_zones::SourceFactory;
        use sha2::Digest;
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        // A stand-in yt-dlp: its version, and the audio URL for any video
        let fake = format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 2026.10.01; exit 0; fi\nfor a; do last=\"$a\"; done\ncase \"$last\" in *v=broken*) echo 'ERROR: [youtube] broken: Video unavailable' >&2; exit 1;; esac\necho {base}/file.flac\n");
        let asset = if cfg!(target_arch = "aarch64") {
            "yt-dlp_linux_aarch64"
        } else {
            "yt-dlp_linux"
        };
        let sum = Arc::new(std::sync::Mutex::new(format!(
            "{:x}",
            sha2::Sha256::digest(fake.as_bytes())
        )));
        let sum2 = sum.clone();
        let mock = Router::new()
            .route(
                "/SHA2-256SUMS",
                get(move || {
                    let s = sum2.lock().unwrap().clone();
                    async move { format!("0000  yt-dlp.tar.gz\n{s}  {asset}\n") }
                }),
            )
            .route(
                &format!("/{asset}"),
                get({
                    let f = fake.clone();
                    move || {
                        let f = f.clone();
                        async move { f }
                    }
                }),
            )
            .route(
                "/file.flac",
                get({
                    let f = flac.clone();
                    move || {
                        let f = f.clone();
                        async move { f }
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        ytdlp::set_releases_base(&base);
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, u) = call(&app, "GET", "/v1/sources/youtube/unofficial", None).await;
        assert_eq!(
            (u["enabled"].clone(), u["ytdlp"].clone()),
            (serde_json::json!(false), serde_json::Value::Null)
        );
        // A download that doesn't match the release's checksum is not installed
        *sum.lock().unwrap() = "ff".repeat(32);
        let (st, _) = call(&app, "POST", "/v1/sources/youtube/unofficial/ytdlp", None).await;
        assert_eq!(st, StatusCode::BAD_GATEWAY);
        assert!(!dir.path().join("data/bin/yt-dlp").exists() && !dir.path().join("bin/yt-dlp").exists());
        *sum.lock().unwrap() = format!("{:x}", sha2::Sha256::digest(fake.as_bytes()));
        let (st, u) = call(&app, "POST", "/v1/sources/youtube/unofficial/ytdlp", None).await;
        assert_eq!(st, StatusCode::OK, "{u}");
        assert_eq!(u["ytdlp"]["from"], "downloaded");
        assert_eq!(u["ytdlp"]["version"], "2026.10.01");
        // Off: zones still refuse; on: a zone plays the video through yt-dlp
        let item = pm_zones::QueueItem::new("youtube:video:dQw4w9WgXcQ");
        let src = s.sources.clone();
        let it = item.clone();
        assert!(tokio::task::spawn_blocking(move || src.open(&it).is_err())
            .await
            .unwrap());
        let (_, u) = call(
            &app,
            "PUT",
            "/v1/sources/youtube/unofficial",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        assert_eq!(u["enabled"], true);
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let yt = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "youtube")
            .unwrap()
            .clone();
        assert_eq!(yt["status"], "browser + zones");
        let src = s.sources.clone();
        let it = item.clone();
        let r = tokio::task::spawn_blocking(move || src.open(&it).map(|_| ()).map_err(|e| e.to_string()))
            .await
            .unwrap();
        assert!(r.is_ok(), "{r:?}");
        let (_, u) = call(&app, "GET", "/v1/sources/youtube/unofficial", None).await;
        assert_eq!(u["health"]["ok"], true, "{u}");
        // yt-dlp's own error reaches the page
        let src = s.sources.clone();
        let r = tokio::task::spawn_blocking(move || {
            src.open(&pm_zones::QueueItem::new("youtube:video:broken_vid1"))
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap();
        assert!(r.unwrap_err().contains("Video unavailable"));
        let (_, u) = call(&app, "DELETE", "/v1/sources/youtube/unofficial/ytdlp", None).await;
        assert!(u["ytdlp"].is_null());
    }

    #[tokio::test]
    async fn tidal_searches_signs_in_and_lists_albums() {
        use axum::extract::RawQuery;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let tokens = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let tokens2 = tokens.clone();
        let album = serde_json::json!({"id": "20115556", "type": "albums", "attributes": {"title": "Random Access Memories", "releaseDate": "2013-05-17", "mediaTags": ["LOSSLESS"], "barcodeId": "886443927087"},
            "relationships": {"artists": {"data": [{"id": "3996", "type": "artists"}]}, "coverArt": {"data": [{"id": "art1", "type": "artworks"}]},
                              "items": {"data": [{"id": "20115564", "type": "tracks", "meta": {"trackNumber": 8, "volumeNumber": 1}}, {"id": "20115557", "type": "tracks", "meta": {"trackNumber": 1, "volumeNumber": 1}}]}}});
        let artist =
            serde_json::json!({"id": "3996", "type": "artists", "attributes": {"name": "Daft Punk"}});
        let art = serde_json::json!({"id": "art1", "type": "artworks", "attributes": {"files": [{"href": "https://img/160.jpg", "meta": {"width": 160, "height": 160}}, {"href": "https://img/640.jpg", "meta": {"width": 640, "height": 640}}]}});
        let track = |id: &str, title: &str| {
            serde_json::json!({"id": id, "type": "tracks", "attributes": {"title": title, "isrc": "USQX91300108", "duration": "PT6M9S", "mediaTags": ["HIRES_LOSSLESS"]},
            "relationships": {"artists": {"data": [{"id": "3996", "type": "artists"}]}, "albums": {"data": [{"id": "20115556", "type": "albums"}]}}})
        };
        let (t1, t2) = (
            track("20115564", "Get Lucky"),
            track("20115557", "Give Life Back to Music"),
        );
        let mock = Router::new()
            .route(
                "/v1/oauth2/token",
                post(move |body: String| {
                    let tokens = tokens2.clone();
                    async move {
                        let grant = body.split('&').find(|p| p.starts_with("grant_type=")).unwrap_or("").to_string();
                        tokens.lock().unwrap().push(body.clone());
                        if grant.ends_with("client_credentials") {
                            Json(serde_json::json!({"access_token": "app-token", "expires_in": 14400, "token_type": "Bearer"}))
                        } else {
                            assert!(body.contains("code_verifier="), "{body}");
                            assert!(body.contains("code=the-code"), "{body}");
                            Json(serde_json::json!({"access_token": "user-token", "refresh_token": "refresh-1", "expires_in": 3600, "user_id": 4242, "scope": "playback collection.read"}))
                        }
                    }
                }),
            )
            .route("/users/{id}", get(|| async { Json(serde_json::json!({"data": {"id": "4242", "type": "users", "attributes": {"country": "DE"}}})) }))
            .route(
                "/searchResults",
                get({
                    let (album, t1) = (album.clone(), t1.clone());
                    move |RawQuery(q): RawQuery| {
                        let (album, t1) = (album.clone(), t1.clone());
                        async move {
                            let q = q.unwrap_or_default();
                            assert!(q.contains("filter%5Bquery%5D=daft+punk") || q.contains("filter%5Bquery%5D=daft%20punk"), "{q}");
                            if q.contains("include=albums") {
                                Json(serde_json::json!({"data": [{"id": "s", "type": "searchResults", "relationships": {"albums": {"data": [{"id": "20115556", "type": "albums"}]}}}], "included": [album]}))
                            } else if q.contains("include=artists") {
                                Json(serde_json::json!({"data": [{"id": "s", "type": "searchResults", "relationships": {"artists": {"data": [{"id": "3996", "type": "artists"}]}}}]}))
                            } else {
                                Json(serde_json::json!({"data": [{"id": "s", "type": "searchResults", "relationships": {"tracks": {"data": [{"id": "20115564", "type": "tracks"}]}}}], "included": [t1]}))
                            }
                        }
                    }
                }),
            )
            .route("/albums", get({ let (album, artist, art) = (album.clone(), artist.clone(), art.clone()); move || async move { Json(serde_json::json!({"data": [album], "included": [artist, art]})) } }))
            .route("/albums/{id}", get({ let (album, artist, art, t1, t2) = (album.clone(), artist.clone(), art.clone(), t1.clone(), t2.clone()); move || async move { Json(serde_json::json!({"data": album, "included": [artist, art, t1, t2]})) } }))
            .route("/tracks", get({ let (t1, artist, album) = (t1.clone(), artist.clone(), album.clone()); move || async move { Json(serde_json::json!({"data": [t1], "included": [artist, album]})) } }))
            .route("/tracks/{id}", get({ let (t1, artist, album, art) = (t1.clone(), artist.clone(), album.clone(), art.clone()); move || async move { Json(serde_json::json!({"data": t1, "included": [artist, album, art]})) } }))
            .route(
                "/userCollectionTracks/{uid}/relationships/items",
                get(|Path(uid): Path<String>| async move {
                    assert_eq!(uid, "me");
                    Json(serde_json::json!({"data": [{"id": "20115564", "type": "tracks"}]}))
                }),
            )
            .route(
                "/userCollectionAlbums/{uid}/relationships/items",
                get(|| async { Json(serde_json::json!({"data": [{"id": "20115556", "type": "albums"}]})) }),
            )
            .route(
                "/playlists",
                get(|RawQuery(q): RawQuery| async move {
                    assert!(q.unwrap_or_default().contains("owners.id%5D=4242"), "owner filter");
                    Json(serde_json::json!({"data": [{"id": "pl-1", "type": "playlists", "attributes": {"name": "Road trip", "description": "", "numberOfTrackItems": 2}}]}))
                }),
            )
            .route(
                "/playlists/{id}",
                get(|| async { Json(serde_json::json!({"data": {"id": "pl-1", "type": "playlists", "attributes": {"name": "Road trip"}}})) }),
            )
            .route(
                "/playlists/{id}/relationships/items",
                get({
                    let (t1, t2) = (t1.clone(), t2.clone());
                    move || async move {
                        let mut t2 = t2.clone();
                        t2["attributes"]["isrc"] = serde_json::Value::Null;
                        Json(serde_json::json!({"data": [{"id": "20115564", "type": "tracks"}, {"id": "20115557", "type": "tracks"}, {"id": "v1", "type": "videos"}], "included": [t1, t2]}))
                    }
                }),
            )
            .route(
                "/artists",
                get(|| async {
                    Json(serde_json::json!({"data": [{"id": "3996", "type": "artists", "attributes": {"name": "Daft Punk", "numberOfFollowers": 12}, "relationships": {"profileArt": {"data": [{"id": "pa1", "type": "artworks"}]}}}],
                        "included": [{"id": "pa1", "type": "artworks", "attributes": {"files": [{"href": "https://img/artist.jpg", "meta": {"width": 640, "height": 640}}]}}]}))
                }),
            )
            .route(
                "/artists/{id}/relationships/albums",
                get(|| async { Json(serde_json::json!({"data": [{"id": "20115556", "type": "albums"}]})) }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        {
            let mut ep = s.tidal.write().unwrap();
            ep.api_base = base.clone();
            ep.auth_base = base.clone();
            ep.login_base = base.clone();
        }
        let app = router(s.clone());
        // Not configured: the source says so and login refuses.
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let tidal = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "tidal")
            .unwrap();
        assert!(tidal["status"].as_str().unwrap().starts_with("not configured"));
        let (st, _) = call(&app, "GET", "/v1/sources/tidal/login", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        s.tidal.write().unwrap().creds = tidal::TidalCredentials {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            scopes: String::new(),
        };
        // Search with the app token: albums carry artists and art, tracks carry artists.
        let (st, r) = call(
            &app,
            "GET",
            "/v1/sources/tidal/search?q=daft%20punk&limit=10",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["kind"], "artist", "artists come first: {r}");
        assert_eq!(items[0]["id"], "artist:3996");
        assert_eq!(items[0]["image"], "https://img/artist.jpg");
        assert_eq!(items[1]["kind"], "album");
        assert_eq!(items[1]["creator"], "Daft Punk");
        assert_eq!(items[1]["image"], "https://img/640.jpg");
        assert_eq!(items[1]["url"], "https://tidal.com/browse/album/20115556");
        assert_eq!(items[2]["kind"], "track");
        assert_eq!(items[2]["title"], "Get Lucky");
        assert_eq!(items[2]["creator"], "Daft Punk");
        assert_eq!(items[2]["duration_ms"], 369_000);
        // An artist page lists the artist's albums with library matches.
        let (st, a) = call(&app, "GET", "/v1/sources/tidal/artists/artist:3996", None).await;
        assert_eq!(st, StatusCode::OK, "{a}");
        assert_eq!(a["name"], "Daft Punk");
        assert_eq!(a["albums"][0]["id"], "album:20115556");
        let (st, _) = call(&app, "GET", "/v1/sources/archive/artists/x", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert!(tokens
            .lock()
            .unwrap()
            .iter()
            .any(|b| b.contains("client_credentials")));
        // An album item lists its tracks in order with hi-res labels and tidal: URIs.
        let (st, it) = call(&app, "GET", "/v1/sources/tidal/items/album:20115556", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(it["title"], "Random Access Memories");
        assert_eq!(it["creator"], "Daft Punk");
        assert_eq!(it["year"], 2013);
        let tracks = it["tracks"].as_array().unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0]["track_no"], 8);
        assert_eq!(tracks[0]["url"], "tidal:track:20115564");
        assert_eq!(tracks[0]["duration_ms"], 369_000);
        assert_eq!(tracks[0]["format"], "FLAC hi-res (TIDAL)");
        let (st, _) = call(&app, "GET", "/v1/sources/tidal/items/nope", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // Unified search: the library answer carries TIDAL hits, flagged when the
        // library already has the same ISRC or UPC.
        {
            let lib = s.library.lock().unwrap();
            let mut t = pm_model::Track {
                id: None,
                title: "Get Lucky".into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: "Daft Punk".into(),
                    mbid: None,
                }],
                album: Some(pm_model::Album {
                    id: None,
                    title: "Random Access Memories".into(),
                    album_artist: Some("Daft Punk".into()),
                    release_mbid: None,
                    release_group_mbid: None,
                    year: Some(2013),
                    upc: pm_model::Upc::parse("886443927087").ok(),
                    total_discs: None,
                }),
                track_no: Some(8),
                disc_no: None,
                duration_ms: Some(369_000),
                file: pm_model::AudioFile {
                    path: dir.path().join("get-lucky.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(369_000),
                },
                recording_mbid: None,
                isrc: pm_model::Isrc::parse("USQX91300108").ok(),
                acoustid: None,
                canonical: Some((
                    pm_model::MusicUri::Isrc {
                        isrc: pm_model::Isrc::parse("USQX91300108").unwrap(),
                    },
                    pm_model::Confidence::TaggedIsrc,
                )),
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            t.id = None;
            lib.upsert_track(&t).unwrap();
        }
        let (st, r) = call(&app, "GET", "/v1/library/search?q=daft%20punk", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["tracks"][0]["title"], "Get Lucky");
        let src = r["sources"].as_array().unwrap();
        assert_eq!(src.len(), 1, "{r}");
        assert_eq!(src[0]["source"], "tidal");
        let tidal_hits = src[0]["items"].as_array().unwrap();
        let track_hit = tidal_hits.iter().find(|h| h["kind"] == "track").unwrap();
        assert_eq!(track_hit["isrc"], "USQX91300108");
        assert!(track_hit["library_track_id"].is_i64(), "{track_hit}");
        let album_hit = tidal_hits.iter().find(|h| h["kind"] == "album").unwrap();
        assert_eq!(album_hit["upc"], "886443927087");
        assert!(album_hit["library_album_id"].is_i64(), "{album_hit}");
        let (_, r) = call(&app, "GET", "/v1/library/search?q=daft%20punk&sources=0", None).await;
        assert!(r["sources"].as_array().unwrap().is_empty());
        // Versions: the library album and track point back at TIDAL by UPC / ISRC.
        let lid = track_hit["library_track_id"].as_i64().unwrap();
        let aid = album_hit["library_album_id"].as_i64().unwrap();
        let (st, v) = call(&app, "GET", &format!("/v1/library/albums/{aid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v[0]["id"], "album:20115556");
        assert!(v[0]["label"].as_str().unwrap().starts_with("TIDAL"));
        let (st, v) = call(&app, "GET", &format!("/v1/library/tracks/{lid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v[0]["uri"], "tidal:track:20115564");
        // Sign-in: the login URL carries PKCE, the callback exchanges the code and stores the user.
        let (st, l) = call(&app, "GET", "/v1/sources/tidal/login", None).await;
        assert_eq!(st, StatusCode::OK);
        let url = url::Url::parse(l["url"].as_str().unwrap()).unwrap();
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["scope"].contains("playback"));
        assert!(q["redirect_uri"].ends_with("/tidal/callback"));
        let state_param = q["state"].clone();
        let req = Request::builder()
            .uri(format!("/tidal/callback?code=the-code&state={state_param}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER, "callback redirects");
        assert_eq!(resp.headers().get("location").unwrap(), "/?tidal=signed_in");
        let (_, stv) = call(&app, "GET", "/v1/sources/tidal/status", None).await;
        assert_eq!(stv["signed_in"], true, "{stv}");
        assert_eq!(stv["user_id"], "4242");
        assert_eq!(stv["country"], "DE");
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let tidal = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "tidal")
            .unwrap();
        assert_eq!(tidal["status"], "ready");
        // Signed in: the collection lists, and imports land as favourites and a Pubky playlist.
        let (st, c) = call(&app, "GET", "/v1/sources/tidal/collection", None).await;
        assert_eq!(st, StatusCode::OK, "{c}");
        assert_eq!(c["tracks"][0]["title"], "Get Lucky");
        assert_eq!(c["albums"][0]["creator"], "Daft Punk");
        let (st, r) = call(&app, "POST", "/v1/sources/tidal/collection/import", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["imported"], 2);
        let (_, favs) = call(&app, "GET", "/v1/bookmarks", None).await;
        let uris: Vec<&str> = favs["uris"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|u| u.as_str())
            .collect();
        assert!(
            uris.contains(&"https://musicbrainz.org/isrc/USQX91300108"),
            "{uris:?}"
        );
        assert!(
            uris.iter().any(|u| u.contains("tidal.com/album/20115556")),
            "{uris:?}"
        );
        let (st, pls) = call(&app, "GET", "/v1/sources/tidal/playlists", None).await;
        assert_eq!(st, StatusCode::OK, "{pls}");
        assert_eq!(pls[0]["name"], "Road trip");
        assert_eq!(pls[0]["items"], 2);
        let (st, r) = call(&app, "POST", "/v1/sources/tidal/playlists/pl-1/import", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(
            r["imported"], 2,
            "ISRC for one, the TIDAL link for the other; the video is skipped"
        );
        assert_eq!(r["skipped"], 1);
        let items = r["playlist"]["playlist"]["items"].as_array().unwrap();
        assert_eq!(items[0]["uri"], "https://musicbrainz.org/isrc/USQX91300108");
        assert!(items[1]["uri"]
            .as_str()
            .unwrap()
            .contains("tidal.com/track/20115557"));
        assert!(items[0].get("source").is_none(), "Auto unless asked");
        // "Keep TIDAL as the source": every song is pinned to its TIDAL track
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/tidal/playlists/pl-1/import?keep_source=true",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let items = r["playlist"]["playlist"]["items"].as_array().unwrap();
        assert_eq!(
            items[0]["uri"], "https://musicbrainz.org/isrc/USQX91300108",
            "items stay canonical"
        );
        assert_eq!(items[0]["source"]["service"], "tidal");
        assert!(
            items[0]["source"]["uri"].as_str().unwrap().contains("tidal.com/"),
            "{}",
            items[0]
        );
        assert_eq!(items[1]["source"]["service"], "tidal");
        // A stale state is refused; sign-out forgets the tokens.
        let req = Request::builder()
            .uri("/tidal/callback?code=x&state=unknown")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("/?tidal=error"));
        let (_, stv) = call(&app, "DELETE", "/v1/sources/tidal/session", None).await;
        assert_eq!(stv["signed_in"], false);
        let (st, _) = call(&app, "GET", "/v1/sources/tidal/collection", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn tidal_tracks_play_through_the_manifest_and_hls_segments() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let seg_len = flac.len().div_ceil(3);
        let segs: Vec<Vec<u8>> = flac.chunks(seg_len).map(|c| c.to_vec()).collect();
        let manifest_base = base.clone();
        let mock = Router::new()
            .route(
                "/trackManifests/{id}",
                get(move |Path(id): Path<String>, headers: HeaderMap| {
                    let manifest_base = manifest_base.clone();
                    async move {
                        assert_eq!(id, "20115564");
                        assert_eq!(headers.get("authorization").unwrap(), "Bearer user-token");
                        Json(serde_json::json!({"data": {"id": id, "type": "trackManifests", "attributes": {
                            "uri": format!("{manifest_base}/hls/master.m3u8"), "formats": ["FLAC"], "trackPresentation": "FULL", "hash": "h"}}}))
                    }
                }),
            )
            .route("/hls/master.m3u8", get(|| async { "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000\nmedia.m3u8\n" }))
            .route("/hls/media.m3u8", get(|| async { "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:1.0,\nseg/0\n#EXTINF:1.0,\nseg/1\n#EXTINF:1.0,\n/hls/seg/2\n#EXT-X-ENDLIST\n" }))
            .route(
                "/hls/seg/{n}",
                get(move |Path(n): Path<usize>| {
                    let segs = segs.clone();
                    async move { ([(header::CONTENT_TYPE, "application/octet-stream")], segs[n].clone()) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        {
            let mut ep = s.tidal.write().unwrap();
            ep.api_base = base.clone();
            ep.auth_base = base.clone();
            ep.creds = tidal::TidalCredentials {
                client_id: "cid".into(),
                client_secret: "sec".into(),
                scopes: String::new(),
            };
        }
        s.library
            .lock()
            .unwrap()
            .set_setting(
                "tidal.user",
                &serde_json::json!({"access_token": "user-token", "refresh_token": "r", "expires_at": pm_library::db::now_unix() + 3600, "user_id": "4242", "scope": "playback"}).to_string(),
            )
            .unwrap();
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        let mut events = zone.subscribe();
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": "tidal:track:20115564", "title": "Get Lucky"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let mut started = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await {
                Ok(Ok(pm_zones::ZoneEvent::TrackStarted { source_format, .. })) => {
                    started = true;
                    assert_eq!(source_format.rate_hz, 44_100);
                }
                Ok(Ok(pm_zones::ZoneEvent::Error { message })) => panic!("playback error: {message}"),
                Ok(Ok(pm_zones::ZoneEvent::Stopped)) if started => break,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {}
            }
        }
        assert!(started, "the TIDAL track never started");
        let st = zone.wait_stopped().await;
        assert!(st.error.is_none(), "{:?}", st.error);
        // The browser proxy serves the same segments as one body.
        let req = Request::builder()
            .uri("/v1/sources/tidal/stream/20115564")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("audio/mp4"));
        assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.len(), flac.len(), "proxy body is the whole file");
        assert_eq!(&body[..], &flac[..]);
        // Seeking: a byte range comes back as 206 with the right slice.
        let req = Request::builder()
            .uri("/v1/sources/tidal/stream/20115564")
            .header("range", "bytes=100-199")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get("content-range").unwrap(),
            &format!("bytes 100-199/{}", flac.len())
        );
        let part = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&part[..], &flac[100..200]);
        // Without a session the resolver refuses with a clear reason.
        s.library.lock().unwrap().delete_setting("tidal.user").unwrap();
        let (_, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": "tidal:track:20115564"}], "replace": true, "play": true})),
        )
        .await;
        let st = zone.wait_stopped().await;
        assert!(
            st.error.as_deref().is_some_and(|e| e.contains("sign in")),
            "{:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn a_sleep_timer_pauses_the_zone() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("long.wav");
        write_wav(&wav, 441);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/repeat"),
            Some(serde_json::json!({"repeat": "one"})),
        )
        .await;
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(
                serde_json::json!({"items": [{"uri": wav.to_string_lossy()}], "replace": true, "play": true}),
            ),
        )
        .await;
        let (_, v) = call(&app, "GET", &format!("/v1/zones/{zid}/sleep"), None).await;
        assert!(v.get("ends_at_ms").is_none());
        let (st, _) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/sleep"),
            Some(serde_json::json!({"seconds": 0})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/sleep"),
            Some(serde_json::json!({"seconds": 1})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["remaining_ms"].as_u64().unwrap() <= 1000);
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        let mut paused = false;
        for _ in 0..400 {
            if zone.state().transport == pm_zones::Transport::Paused {
                paused = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(paused, "the zone did not pause: {:?}", zone.state().transport);
        let (_, v) = call(&app, "GET", &format!("/v1/zones/{zid}/sleep"), None).await;
        assert!(v.get("ends_at_ms").is_none(), "timer cleared after firing");
        // Cancel: set and clear, nothing pauses later.
        let (st, _) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/sleep"),
            Some(serde_json::json!({"seconds": 3600})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(s.sleep_ends_at(zone.id()).is_some());
        let (_, v) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/sleep"),
            Some(serde_json::json!({"seconds": null})),
        )
        .await;
        assert!(v.get("ends_at_ms").is_none());
        assert!(s.sleep_ends_at(zone.id()).is_none());
    }

    #[tokio::test]
    async fn playback_transfers_between_zones() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("long.wav");
        write_wav(&wav, 44_100 * 30);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let out = Arc::new(pm_zones::outputs::WavFileOutput::new(dir.path().join("sink.wav")));
        s.outputs.register(out.clone());
        let target = s.create_zone("Target", out);
        let tid = target.id().to_string();
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones
            .as_array()
            .unwrap()
            .iter()
            .map(|z| z["id"].as_str().unwrap().to_string())
            .find(|z| *z != tid)
            .unwrap();
        let items: Vec<_> = (0..3)
            .map(|i| serde_json::json!({"uri": wav.to_string_lossy(), "title": format!("t{i}")}))
            .collect();
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": items, "replace": true})),
        )
        .await;
        call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/repeat"),
            Some(serde_json::json!({"repeat": "all"})),
        )
        .await;
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transport"),
            Some(serde_json::json!({"action": "play", "index": 1})),
        )
        .await;
        let source = s.zones.get(&ZoneId::new(&zid)).unwrap();
        for _ in 0..500 {
            if source.state().now_playing.as_ref().map(|n| n.index) == Some(1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // The null output does not pace, so pause right away to hold a position.
        source.pause();
        for _ in 0..500 {
            if source.state().transport == pm_zones::Transport::Paused {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transfer"),
            Some(serde_json::json!({"to": zid})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, z) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transfer"),
            Some(serde_json::json!({"to": tid})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{z}");
        assert_eq!(z["id"], tid);
        assert_eq!(z["queue"].as_array().unwrap().len(), 3);
        assert_eq!(z["queue"][1]["title"], "t1");
        assert_eq!(z["repeat"], "all");
        assert_eq!(z["now_playing"]["index"], 1);
        assert_eq!(z["transport"], "paused");
        let src = source.state();
        assert!(src.queue.is_empty());
        assert_eq!(src.transport, pm_zones::Transport::Stopped);
        // Nothing left to move.
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transfer"),
            Some(serde_json::json!({"to": tid})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn shuffle_reorders_only_the_upcoming_items() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("short.wav");
        write_wav(&wav, 441);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let items: Vec<_> = (0..8)
            .map(|_| serde_json::json!({"uri": wav.to_string_lossy()}))
            .collect();
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": items, "replace": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        // Loop the third item so the "current" index holds still on the null output.
        call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/repeat"),
            Some(serde_json::json!({"repeat": "one"})),
        )
        .await;
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transport"),
            Some(serde_json::json!({"action": "play", "index": 2})),
        )
        .await;
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        for _ in 0..500 {
            if zone.state().queue_index == Some(2) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(zone.state().queue_index, Some(2));
        // The playing item and the two before it must stay put.
        let before: Vec<u64> = zone.queue().iter().map(|i| i.id).collect();
        let mut changed = false;
        for _ in 0..5 {
            let (st, _) = call(&app, "POST", &format!("/v1/zones/{zid}/shuffle"), None).await;
            assert_eq!(st, StatusCode::NO_CONTENT);
            let after: Vec<u64> = zone.queue().iter().map(|i| i.id).collect();
            assert_eq!(&after[..3], &before[..3]);
            let mut rest = after[3..].to_vec();
            rest.sort_unstable();
            assert_eq!(rest, before[3..]);
            if after[3..] != before[3..] {
                changed = true;
                break;
            }
        }
        assert!(changed, "five shuffles of five items never moved anything");
        let (_, z) = call(&app, "GET", &format!("/v1/zones/{zid}"), None).await;
        assert_eq!(z["queue_index"], 2);
        let (st, _) = call(&app, "POST", "/v1/zones/nope/shuffle", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn library_radio_continues_after_the_last_track() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/one.wav"), 4410);
        write_wav(&dir.path().join("music/two.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        let first = tracks["tracks"][0]["id"].as_i64().unwrap();
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, _) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/radio"),
            Some(serde_json::json!({"on": true})),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, z) = call(&app, "GET", &format!("/v1/zones/{zid}"), None).await;
        assert_eq!(z["radio"], true);
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"track_ids": [first], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        let mut grew = false;
        for _ in 0..500 {
            if zone.queue().len() >= 2 {
                grew = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(grew, "radio did not add tracks");
        assert!(zone.queue().len() <= 2, "only the one other track exists");
        zone.set_radio(false);
        assert_eq!(zone.wait_stopped().await.error, None);
    }

    #[tokio::test]
    async fn web_ui_is_served_without_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), Some("s3cret")).await;
        let app = router(s);
        for (path, needle) in [
            ("/", "<title>Pubky Music</title>"),
            ("/app", "<title>Pubky Music</title>"),
            ("/app/", "<title>Pubky Music</title>"),
            ("/app.css", ".player"),
            ("/app.js", "/v1/events"),
            ("/manifest.webmanifest", "\"display\": \"standalone\""),
            ("/icon.svg", "<svg"),
        ] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert!(std::str::from_utf8(&body).unwrap().contains(needle), "{path}");
        }
    }

    #[tokio::test]
    async fn bearer_token_guards_every_route() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), Some("s3cret")).await;
        let app = router(s);
        let (st, _) = call(&app, "GET", "/v1/info", None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let req = Request::builder()
            .uri("/v1/info")
            .header("authorization", "Bearer s3cret")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
        let (st, _) = call(&app, "GET", "/v1/info?token=s3cret", None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn zones_can_be_created_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let mut events = s.events.subscribe();
        let (st, z) = call(
            &app,
            "POST",
            "/v1/zones",
            Some(serde_json::json!({"name": "Kitchen", "output": "null"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{z}");
        assert_eq!(z["id"], "kitchen");
        let (st, _) = call(
            &app,
            "POST",
            "/v1/zones",
            Some(serde_json::json!({"name": "X", "output": "nope"})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = call(&app, "DELETE", "/v1/zones/kitchen", None).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let mut removed = false;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev.event, ApiEvent::ZoneRemoved { ref zone } if zone.0 == "kitchen") {
                removed = true;
            }
        }
        assert!(removed);
        assert_eq!(s.zones.list().len(), 1);
    }

    /// Announcements: a chime and a clip play instead of the music, then the zone goes back to the
    /// same track at the same position, playing or paused as it was, with the queue as it was.
    #[tokio::test]
    async fn an_announcement_interrupts_and_the_zone_comes_back() {
        use pm_zones::outputs::NullOutput;
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let mock = Router::new().route(
            "/song.flac",
            get({
                let f = flac.clone();
                move || {
                    let f = f.clone();
                    async move { f }
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        // A zone on a real-time null output, so positions mean something
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lib.sqlite");
        let lib = Library::open(&db).unwrap();
        let outputs = OutputRegistry::new();
        outputs.register(Arc::new(NullOutput::realtime()));
        let s = NodeState::new(
            NodeSettings {
                name: "test".into(),
                version: "0".into(),
                db_path: db,
                scan_folders: vec![dir.path().join("music")],
                art_dir: dir.path().join("art"),
                mirror_dir: dir.path().join("homeserver"),
                pipeline: PipelineConfig::default(),
                token: None,
                subsonic_user: "pubky".into(),
                pubky: Default::default(),
                public_url: "http://127.0.0.1:7788".into(),
                upnp: false,
                cast: false,
                slimproto_port: 0,
                session_secret_path: None,
                tidal: Default::default(),
                spotify: Default::default(),
                acoustid_key: String::new(),
                jamendo: Default::default(),
            },
            lib,
            outputs,
        );
        *s.deezer_base.write().unwrap() = "http://127.0.0.1:9".into();
        s.zones_for_all_outputs();
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let zone = |app: &Router| {
            let app = app.clone();
            let zid = zid.clone();
            async move { call(&app, "GET", &format!("/v1/zones/{zid}"), None).await.1 }
        };
        // Two songs, the first playing for a while
        let (st, _) = call(&app, "POST", &format!("/v1/zones/{zid}/queue"), Some(serde_json::json!({"items": [
            {"uri": format!("{base}/song.flac"), "title": "A", "artist": "Band"},
            {"uri": format!("{base}/song.flac"), "title": "B", "artist": "Band"}], "replace": true, "play": true}))).await;
        assert_eq!(st, StatusCode::OK);
        let mut z = serde_json::Value::Null;
        for _ in 0..60 {
            z = zone(&app).await;
            if z["transport"] == "playing" && z["now_playing"]["position_ms"].as_u64().unwrap_or(0) >= 1200 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let saved = z["now_playing"]["position_ms"].as_u64().unwrap();
        assert!(saved >= 1200, "{z}");
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/announce"),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "needs a uri or a chime");
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/announce"),
            Some(serde_json::json!({"uri": "ftp://x"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // The announcement: a chime, then the clip
        let (st, a) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/announce"),
            Some(
                serde_json::json!({"uri": format!("{base}/song.flac"), "chime": true, "volume_percent": 60}),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{a}");
        assert_eq!(
            (
                a["resumes"].clone(),
                a["items"].clone(),
                a["volume_applied"].clone()
            ),
            (
                serde_json::json!(true),
                serde_json::json!(2),
                serde_json::json!(false)
            ),
            "no hardware volume on a null output: {a}"
        );
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/announce"),
            Some(serde_json::json!({"chime": true})),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "one at a time");
        let mut seen_chime = false;
        let mut seen_clip = false;
        for _ in 0..80 {
            z = zone(&app).await;
            let t = z["now_playing"]["item"]["title"].as_str().unwrap_or("");
            seen_chime |= t == "Chime";
            seen_clip |= t == "Announcement";
            if seen_clip && t != "Announcement" && t != "Chime" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            seen_chime && seen_clip,
            "chime {seen_chime}, clip {seen_clip}: {z}"
        );
        // Back on A, near where it was, playing; the queue as before
        let mut back = serde_json::Value::Null;
        for _ in 0..40 {
            back = zone(&app).await;
            if back["now_playing"]["item"]["title"] == "A"
                && back["transport"] == "playing"
                && back["now_playing"]["position_ms"].as_u64().unwrap_or(0) >= saved
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(back["now_playing"]["item"]["title"], "A", "{back}");
        assert_eq!(back["transport"], "playing");
        let pos = back["now_playing"]["position_ms"].as_u64().unwrap();
        assert!(
            pos >= saved && pos < saved + 2500,
            "position {pos} vs saved {saved}"
        );
        assert_eq!(
            back["queue"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["title"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );
        // Paused before: paused again after
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transport"),
            Some(serde_json::json!({"action": "pause"})),
        )
        .await;
        let (st, a) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/announce"),
            Some(serde_json::json!({"uri": format!("{base}/song.flac"), "title": "Doorbell"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{a}");
        let mut seen = false;
        for _ in 0..80 {
            z = zone(&app).await;
            let t = z["now_playing"]["item"]["title"].as_str().unwrap_or("");
            seen |= t == "Doorbell";
            if seen && t != "Doorbell" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(seen, "{z}");
        for _ in 0..30 {
            back = zone(&app).await;
            if back["transport"] == "paused" && back["now_playing"]["item"]["title"] == "A" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(
            (
                back["transport"].clone(),
                back["now_playing"]["item"]["title"].clone()
            ),
            (serde_json::json!("paused"), serde_json::json!("A")),
            "{back}"
        );
        assert_eq!(back["queue"].as_array().unwrap().len(), 2);
        // The chime file is a WAV in the data dir
        assert!(s.data_dir().join("announce/chime.wav").is_file());
    }

    /// Sources › TIDAL › Sign in: the ways side by side, the picked one remembered, what is in
    /// use (user and expiry, never a token) and one line for the overview
    /// (STEWARD-CREDENTIALS-REUSE.md).
    #[tokio::test]
    async fn tidal_sign_in_lists_the_ways_and_remembers_the_choice() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        s.tidal.write().unwrap().creds = tidal::TidalCredentials {
            client_id: "own-app".into(),
            client_secret: String::new(),
            scopes: String::new(),
        };
        let app = router(s.clone());
        let signin = "/v1/sources/tidal/signin";
        // A test build has no built-in device client: the way is listed, not available
        let (st, si) = call(&app, "GET", signin, None).await;
        assert_eq!(st, StatusCode::OK, "{si}");
        let ids: Vec<&str> = si["ways"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["app", "import", "device", "own", "paste"]);
        assert_eq!(si["ways"][0]["recommended"], true, "{si}");
        assert_eq!(
            si["ways"][0]["label"], "Your own TIDAL app",
            "no project app in a test build"
        );
        assert_eq!(si["ways"][0]["available"], true);
        assert_eq!(si["ways"][2]["available"], false);
        assert!(
            si["ways"][2]["reason"]
                .as_str()
                .unwrap()
                .contains("device client"),
            "{si}"
        );
        assert_eq!(si["ways"][3]["available"], true);
        assert!(si["in_use"].is_null(), "{si}");
        assert_eq!(si["line"], "Not signed in");
        assert_eq!(si["details"]["unofficial"]["enabled"], false);
        assert_eq!(si["details"]["status"]["signed_in"], false);
        // A way that is not available, or not a way, is refused in plain words
        let (st, r) = call(&app, "PUT", signin, Some(serde_json::json!({"way": "device"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert!(r["error"].as_str().unwrap().contains("device client"), "{r}");
        let (st, r) = call(&app, "PUT", signin, Some(serde_json::json!({"way": "nope"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert!(
            r["error"]
                .as_str()
                .unwrap()
                .contains("one of app, import, device, own, paste"),
            "{r}"
        );
        // Your own device client makes the device way available, labelled as yours; picking it is
        // what the Full tracks switch used to do
        call(
            &app,
            "PUT",
            "/v1/sources/tidal/unofficial",
            Some(serde_json::json!({"client_id": "dev-1", "client_secret": "s"})),
        )
        .await;
        let (st, r) = call(&app, "PUT", signin, Some(serde_json::json!({"way": "device"}))).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["chosen"], "device");
        assert_eq!(r["ways"][2]["label"], "Your own device client");
        assert_eq!(r["details"]["unofficial"]["enabled"], true, "{r}");
        assert!(r["in_use"].is_null(), "picked, not signed in yet: {r}");
        // The approval stores a session: it is in use, shown by user and expiry, never the token
        {
            let lib = s.library.lock().unwrap();
            let mut m = tidal_unofficial::load(&lib);
            m.session = Some(tidal_unofficial::Session {
                access_token: "secret-token".into(),
                refresh_token: Some("secret-refresh".into()),
                expires_at: 4_000_000_000,
                user_id: Some("4242".into()),
                country: "DE".into(),
                session_id: None,
                from: None,
            });
            lib.set_setting(tidal_unofficial::STATE_KEY, &serde_json::to_string(&m).unwrap())
                .unwrap();
        }
        let (_, r) = call(&app, "GET", signin, None).await;
        assert_eq!(r["in_use"]["way"], "device", "{r}");
        assert_eq!(r["in_use"]["user"], "4242");
        assert_eq!(r["in_use"]["expires_at"], 4_000_000_000i64);
        assert_eq!(r["in_use"]["gives"], "full tracks");
        assert_eq!(r["line"], "Your own device client · full tracks");
        assert!(!r.to_string().contains("secret-"), "{r}");
        // The Sources overview carries the same line; a source without a sign-in has none
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let row = |id: &str| {
            srcs.as_array()
                .unwrap()
                .iter()
                .find(|x| x["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(row("tidal")["sign_in"], "Your own device client · full tracks");
        assert_eq!(
            row("tidal")["status"],
            "ready",
            "the device sign-in counts: {srcs}"
        );
        assert!(row("deezer").get("sign_in").is_none(), "{srcs}");
        // Picking the app way turns the device way off (its session is kept until Sign out)
        let (st, r) = call(&app, "PUT", signin, Some(serde_json::json!({"way": "app"}))).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["details"]["unofficial"]["enabled"], false);
        assert!(r["in_use"].is_null(), "{r}");
        assert_eq!(r["line"], "Not signed in");
        // The official sign-in on a third-party tier app: previews
        {
            let lib = s.library.lock().unwrap();
            let u = tidal::UserSession {
                access_token: "secret-official".into(),
                refresh_token: None,
                expires_at: 4_000_000_000,
                user_id: Some("7".into()),
                country: Some("DE".into()),
                scope: String::new(),
                access_tier: Some("THIRD_PARTY".into()),
            };
            lib.set_setting("tidal.user", &serde_json::to_string(&u).unwrap())
                .unwrap();
        }
        let (_, r) = call(&app, "GET", signin, None).await;
        assert_eq!(r["in_use"]["way"], "app", "{r}");
        assert_eq!(r["in_use"]["gives"], "30-second previews, your collection");
        assert_eq!(r["line"], "Your own TIDAL app · previews");
        assert!(!r.to_string().contains("secret-"), "{r}");
        // Sign out of the way in use; the picked way stays remembered
        let (st, r) = call(&app, "DELETE", signin, None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert!(r["in_use"].is_null(), "{r}");
        assert_eq!(r["chosen"], "app");
        assert_eq!(r["line"], "Not signed in");
        assert_eq!(r["details"]["status"]["signed_in"], false);
        // Sources without ways, and unknown ones
        let (st, _) = call(&app, "GET", "/v1/sources/deezer/signin", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = call(&app, "GET", "/v1/sources/nope/signin", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// One front for the quality choice (STEWARD-UX-PLAYER-SOURCES.md §2): the options, the pick
    /// kept in the source's own state, no choice where the service sends one format; and
    /// `healthy` on the Sources list.
    #[tokio::test]
    async fn quality_is_one_front_per_source_and_sources_say_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let ids = |q: &serde_json::Value| {
            q["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let (st, q) = call(&app, "GET", "/v1/sources/tidal/quality", None).await;
        assert_eq!(st, StatusCode::OK, "{q}");
        assert_eq!(ids(&q), ["max", "high"]);
        assert_eq!(q["current"], "max");
        assert_eq!(q["applies"], "next_track");
        assert!(q["note"].as_str().unwrap().contains("device sign-in"), "{q}");
        let (st, r) = call(
            &app,
            "PUT",
            "/v1/sources/tidal/quality",
            Some(serde_json::json!({"id": "high"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["current"], "high");
        let (_, u) = call(&app, "GET", "/v1/sources/tidal/unofficial", None).await;
        assert_eq!(u["quality"], "high", "the source's own state follows");
        let (st, r) = call(
            &app,
            "PUT",
            "/v1/sources/tidal/quality",
            Some(serde_json::json!({"id": "lossy"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert!(r["error"].as_str().unwrap().contains("max, high"), "{r}");
        let (_, q) = call(&app, "GET", "/v1/sources/qobuz/quality", None).await;
        assert_eq!(ids(&q), ["max", "cd", "mp3"]);
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/qobuz/quality",
            Some(serde_json::json!({"id": "mp3"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (_, u) = call(&app, "GET", "/v1/sources/qobuz/unofficial", None).await;
        assert_eq!(u["quality"], "mp3");
        let (_, q) = call(&app, "GET", "/v1/sources/spotify/quality", None).await;
        assert_eq!(ids(&q), ["320", "160", "96"]);
        assert_eq!(q["current"], "320");
        assert_eq!(q["applies"], "now");
        let (st, r) = call(
            &app,
            "PUT",
            "/v1/sources/spotify/quality",
            Some(serde_json::json!({"id": "160"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["current"], "160");
        let (_, l) = call(&app, "GET", "/v1/sources/spotify/librespot", None).await;
        assert_eq!(l["bitrate"], "160");
        let (st, q) = call(&app, "GET", "/v1/sources/deezer/quality", None).await;
        assert_eq!(st, StatusCode::OK, "{q}");
        assert!(q["options"].as_array().unwrap().is_empty());
        assert!(q["note"].as_str().unwrap().contains("previews"), "{q}");
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/sources/deezer/quality",
            Some(serde_json::json!({"id": "x"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(&app, "GET", "/v1/sources/nope/quality", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        // healthy: on and working
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let row = |srcs: &serde_json::Value, id: &str| {
            srcs.as_array()
                .unwrap()
                .iter()
                .find(|x| x["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(row(&srcs, "deezer")["healthy"], true, "{srcs}");
        assert_eq!(row(&srcs, "tidal")["healthy"], false, "not signed in: {srcs}");
        call(
            &app,
            "PUT",
            "/v1/sources/deezer/enabled",
            Some(serde_json::json!({"enabled": false})),
        )
        .await;
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        assert_eq!(row(&srcs, "deezer")["healthy"], false, "off: {srcs}");
    }

    /// Sources › TIDAL › Sign in › from another app on this machine: the known places are read,
    /// what was found is listed by user and expiry (never a token), a usable one is imported on
    /// a click, an unusable one is refused in plain words, the other app's file is untouched,
    /// and a pasted token works too (STEWARD-CREDENTIALS-REUSE.md step 2).
    #[tokio::test]
    async fn tidal_sign_ins_from_other_apps_are_listed_imported_or_refused() {
        let _home_guard = imports::HOME_TEST_LOCK.lock().await;
        use base64::Engine;
        let jwt = |uid: i64, exp: i64| {
            let b = |v: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
            format!(
                "{}.{}.{}",
                b(r#"{"alg":"RS256"}"#),
                b(&format!(r#"{{"uid":{uid},"cc":"DE","exp":{exp}}}"#)),
                b("s")
            )
        };
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/tidal_dl_ng")).unwrap();
        std::fs::write(
            home.path().join(".config/tidal_dl_ng/token.json"),
            format!(
                r#"{{"token_type": {{"data": "Bearer"}}, "access_token": {{"data": "{}"}}, "refresh_token": {{"data": "secret-refresh"}}, "is_pkce": {{"data": false}}, "expiry_time": {{"data": "2096-01-01T00:00:00"}}}}"#,
                jwt(4242, 4_000_000_000)
            ),
        )
        .unwrap();
        std::fs::write(
            home.path().join(".tidal-dl.token.json"),
            base64::engine::general_purpose::STANDARD.encode(format!(
                r#"{{"userid": 7, "countryCode": "US", "accessToken": "{}", "refreshToken": "r", "expiresAfter": 1000}}"#,
                jwt(7, 1000)
            )),
        )
        .unwrap();
        *imports::HOME_OVERRIDE.write().unwrap() = Some(home.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let signin = "/v1/sources/tidal/signin";
        let import = "/v1/sources/tidal/signin/import";
        let (st, si) = call(&app, "GET", signin, None).await;
        assert_eq!(st, StatusCode::OK, "{si}");
        let found = si["imports"].as_array().unwrap();
        assert_eq!(found.len(), 2, "{si}");
        assert_eq!(found[0]["id"], "tidal-dl-ng");
        assert_eq!(found[0]["user"], "4242");
        assert_eq!(found[0]["usable"], true);
        assert_eq!(found[0]["renews"], true, "python-tidal's client is ours");
        assert_eq!(found[1]["id"], "tidal-dl");
        assert_eq!(found[1]["usable"], false);
        assert!(found[1]["reason"].as_str().unwrap().contains("expired"), "{si}");
        assert!(
            si["places"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_str().unwrap().starts_with("Music Assistant: not read")),
            "{si}"
        );
        assert_eq!(si["ways"][1]["id"], "import");
        assert_eq!(si["ways"][1]["available"], true);
        assert!(
            si["ways"][1]["note"]
                .as_str()
                .unwrap()
                .contains("tidal-dl-ng (user 4242)"),
            "{si}"
        );
        let text = si.to_string();
        assert!(
            !text.contains("secret-refresh") && !text.contains(&jwt(4242, 4_000_000_000)),
            "{si}"
        );
        // Refused in plain words; nothing changes
        let (st, r) = call(&app, "POST", import, Some(serde_json::json!({"id": "tidal-dl"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert!(r["error"].as_str().unwrap().contains("expired"), "{r}");
        let (st, _) = call(&app, "POST", import, Some(serde_json::json!({"id": "nope"}))).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, r) = call(&app, "POST", import, Some(serde_json::json!({"text": "hello"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert!(r["error"].as_str().unwrap().starts_with("Not usable"), "{r}");
        let (_, r) = call(&app, "GET", signin, None).await;
        assert!(r["in_use"].is_null(), "{r}");
        // Used on a click: the device-way session, named after the app
        let (st, r) = call(
            &app,
            "POST",
            import,
            Some(serde_json::json!({"id": "tidal-dl-ng"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["in_use"]["way"], "import");
        assert_eq!(r["in_use"]["label"], "tidal-dl-ng's sign-in");
        assert_eq!(r["in_use"]["user"], "4242");
        assert_eq!(r["chosen"], "import");
        assert_eq!(r["line"], "tidal-dl-ng's sign-in · full tracks");
        assert_eq!(r["details"]["unofficial"]["enabled"], true);
        assert_eq!(r["details"]["unofficial"]["from"], "tidal-dl-ng");
        assert!(!r.to_string().contains("secret-refresh"), "{r}");
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let t = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "tidal")
            .unwrap();
        assert_eq!(t["sign_in"], "tidal-dl-ng's sign-in · full tracks");
        assert_eq!(t["status"], "ready");
        assert!(
            std::fs::read_to_string(home.path().join(".config/tidal_dl_ng/token.json"))
                .unwrap()
                .contains("secret-refresh"),
            "the other app's file is only read"
        );
        // Sign out, then a pasted token with its refresh token on the next line
        let (_, r) = call(&app, "DELETE", signin, None).await;
        assert!(r["in_use"].is_null(), "{r}");
        let (st, r) = call(
            &app,
            "POST",
            import,
            Some(serde_json::json!({"text": format!("{}\nmy-refresh", jwt(5, 4_000_000_000))})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["in_use"]["way"], "paste");
        assert_eq!(r["in_use"]["label"], "Pasted token");
        assert_eq!(r["in_use"]["user"], "5");
        assert_eq!(r["chosen"], "paste");
        assert!(!r.to_string().contains("my-refresh"), "{r}");
        *imports::HOME_OVERRIDE.write().unwrap() = None;
    }

    /// Bookmarks (social v1 naming, 2026-10-26): `/v1/favourites` answers the same as
    /// `/v1/bookmarks` for one release, marked deprecated.
    #[tokio::test]
    async fn favourites_is_a_deprecated_name_for_bookmarks() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path(), None).await);
        let uri = "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69";
        let (st, r) = call(
            &app,
            "PUT",
            "/v1/favourites",
            Some(serde_json::json!({"uri": uri})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(
            (r["bookmarked"].as_bool(), r["favourite"].as_bool()),
            (Some(true), Some(true))
        );
        let (_, new) = call(&app, "GET", "/v1/bookmarks", None).await;
        let (_, old) = call(&app, "GET", "/v1/favourites", None).await;
        assert_eq!(new, old);
        assert_eq!(new["uris"], serde_json::json!([uri]));
        let get = |path: &'static str| Request::builder().uri(path).body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(get("/v1/favourites")).await.unwrap();
        assert_eq!(resp.headers()["deprecation"], "true");
        assert!(resp.headers()["link"].to_str().unwrap().contains("/v1/bookmarks"));
        let resp = app.clone().oneshot(get("/v1/bookmarks")).await.unwrap();
        assert!(resp.headers().get("deprecation").is_none());
        let (st, r) = call(&app, "DELETE", &format!("/v1/bookmarks?uri={uri}"), None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["bookmarked"], false);
    }

    /// Sources › Qobuz, YouTube, Spotify and Settings › Last.fm have the same Sign in chooser
    #[tokio::test]
    async fn imported_sign_ins_win_over_a_synced_sessions_refresh() {
        use vault_cache::{account, carried, decide, hash, Do, Side, Synced, VaultSide, Wipe, BRIDGES};

        let _qobuz_guard = qobuz::API_TEST_LOCK.lock().await;
        let _lastfm_guard = lastfm::API_TEST_LOCK.lock().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        qobuz::set_api_base(&base);
        lastfm::set_api_base(&format!("{base}/lastfm"));
        let mock = Router::new()
            .route("/user/get", get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(headers["x-app-id"], "100000000");
                assert_eq!(headers["x-user-auth-token"], "imported-session");
                Json(serde_json::json!({"id": 42, "credential": {"parameters": {"hires_streaming": true}}}))
            }))
            .route("/lastfm", post(|body: String| async move {
                let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes()).collect();
                assert_eq!(form["method"], "user.getInfo");
                assert_eq!(form["sk"], "imported-session");
                Json(serde_json::json!({"user": {"name": "listener"}}))
            }));
        let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let mut decisions = Vec::new();
        for (service, app_key, session_key, app, session, text) in [
            (
                "qobuz", qobuz::APP_KEY, qobuz::STATE_KEY,
                serde_json::json!({"id": "100000000", "secret": "s3cret", "from": "settings"}),
                serde_json::json!({"session": {"token": "old-session", "user_id": "42", "hires": true}}),
                "[qobuz]\nuse_auth_token = true\nemail_or_userid = \"42\"\npassword_or_token = \"imported-session\"\napp_id = \"100000000\"\nsecrets = [\"s3cret\"]\n",
            ),
            (
                "lastfm", lastfm::APP_KEY, lastfm::SESSION_KEY,
                serde_json::json!({"api_key": "0123456789abcdef0123456789abcdef", "api_secret": "fedcba9876543210fedcba9876543210", "from": "settings"}),
                serde_json::json!({"key": "old-session", "user": "listener", "submit": true}),
                "0123456789abcdef0123456789abcdef\nfedcba9876543210fedcba9876543210\nimported-session\n",
            ),
        ] {
            let bridge = BRIDGES.iter().find(|b| b.service == service).unwrap();
            let lineage_key = format!("vault.lineage.{service}");
            let seen = {
                let lib = s.library.lock().unwrap();
                lib.set_setting(app_key, &app.to_string()).unwrap();
                lib.set_setting(session_key, &session.to_string()).unwrap();
                vault_cache::new_sign_in(&lib, service);
                let travels = carried(&lib, bridge).unwrap().unwrap();
                Synced {
                    revision: 1,
                    hash: hash(&travels),
                    account: account(&travels),
                    lineage: lib.get_setting(&lineage_key).unwrap(),
                    ..Default::default()
                }
            };
            // Another node refreshed the sign-in we last synced. Importing even the same
            // account must start a new sign-in, not lose to that refresh on the next pass.
            let vault = VaultSide {
                side: Side {
                    hash: "refreshed-in-the-vault".into(),
                    account: seen.account.clone(),
                    lineage: seen.lineage.clone(),
                    ..Default::default()
                },
                from_a_node: true,
                our_app: true,
            };
            signin::source(service).unwrap().import(&s, signin::ImportBody {
                text: Some(text.into()),
                ..Default::default()
            }).await.unwrap();
            let lib = s.library.lock().unwrap();
            let travels = carried(&lib, bridge).unwrap().unwrap();
            let imported = Side {
                hash: hash(&travels),
                account: account(&travels),
                lineage: lib.get_setting(&lineage_key).unwrap(),
                ..Default::default()
            };
            assert_ne!(imported.hash, seen.hash, "{service}: the new session was saved");
            decisions.push((service, decide(Some(&seen), Some(&imported), Some(&vault), &Wipe::default())));
        }
        server.abort();
        assert_eq!(decisions, [("qobuz", Do::Push), ("lastfm", Do::Push)]);
    }

    /// (STEWARD-CREDENTIALS-REUSE.md step 3): other apps' own session files are listed from the
    /// known places; a cookies file or a librespot credentials.json is only ever picked or pasted,
    /// kept readable by the node only, and gone with Sign out; nothing answers with a token.
    #[tokio::test]
    async fn the_other_sources_have_sign_in_ways_and_picked_files_stay_private() {
        let _home_guard = imports::HOME_TEST_LOCK.lock().await;
        let _api_guard = qobuz::API_TEST_LOCK.lock().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        qobuz::set_api_base(&format!("http://{}", listener.local_addr().unwrap()));
        let mock = Router::new().route("/user/get", get(|h: axum::http::HeaderMap| async move {
            assert_eq!(h.get("x-app-id").unwrap(), "100000000");
            if h.get("x-user-auth-token").unwrap() != "qobuz-token-secret" {
                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"message": "bad token"})));
            }
            (StatusCode::OK, Json(serde_json::json!({"id": 42, "display_name": "Listener", "credential": {"parameters": {"hires_streaming": true}}})))
        }));
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/streamrip")).unwrap();
        std::fs::write(
            home.path().join(".config/streamrip/config.toml"),
            "[qobuz]\nuse_auth_token = true\nemail_or_userid = \"42\"\npassword_or_token = \"qobuz-token-secret\"\napp_id = \"100000000\"\nsecrets = [\"s3cret\"]\n[tidal]\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.path().join(".config/rescrobbled")).unwrap();
        std::fs::write(
            home.path().join(".config/rescrobbled/config.toml"),
            "lastfm-key = \"0123456789abcdef0123456789abcdef\"\nlastfm-secret = \"fedcba9876543210fedcba9876543210\"\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join(".config/rescrobbled/session"),
            "sessKeyRescrobbled\n",
        )
        .unwrap();
        *imports::HOME_OVERRIDE.write().unwrap() = Some(home.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let ids = |si: &serde_json::Value| {
            si["ways"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let row = |srcs: &serde_json::Value, id: &str| {
            srcs.as_array()
                .unwrap()
                .iter()
                .find(|x| x["id"] == id)
                .unwrap()
                .clone()
        };
        // Qobuz: streamrip's token sign-in is listed and used on a click
        let (st, q) = call(&app, "GET", "/v1/sources/qobuz/signin", None).await;
        assert_eq!(st, StatusCode::OK, "{q}");
        assert_eq!(ids(&q), ["own", "import", "paste"]);
        assert_eq!(q["imports"][0]["id"], "streamrip");
        assert_eq!(q["imports"][0]["user"], "42");
        assert_eq!(q["imports"][0]["usable"], true, "{q}");
        assert_eq!(q["ways"][1]["available"], true);
        assert!(
            !q.to_string().contains("qobuz-token-secret") && !q.to_string().contains("s3cret"),
            "{q}"
        );
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/qobuz/signin/import",
            Some(serde_json::json!({"id": "streamrip"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["in_use"]["way"], "import");
        assert_eq!(r["in_use"]["label"], "streamrip's sign-in");
        assert_eq!(r["line"], "streamrip's sign-in · hi-res");
        assert_eq!(r["details"]["unofficial"]["signed_in"], true);
        assert_eq!(r["details"]["unofficial"]["app_from"], "import");
        assert!(!r.to_string().contains("qobuz-token-secret"), "{r}");
        let original = std::fs::read_to_string(home.path().join(".config/streamrip/config.toml")).unwrap();
        let (st, refused) = call(
            &app,
            "POST",
            "/v1/sources/qobuz/signin/import",
            Some(serde_json::json!({"text": original.replace("qobuz-token-secret", "expired-token")})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{refused}");
        assert!(!refused.to_string().contains("expired-token"));
        let (_, current) = call(&app, "GET", "/v1/sources/qobuz/signin", None).await;
        assert_eq!(current["in_use"]["user"], "Listener");
        assert_eq!(current["in_use"]["way"], "import");
        assert_eq!(
            std::fs::read_to_string(home.path().join(".config/streamrip/config.toml")).unwrap(),
            original
        );
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        assert_eq!(row(&srcs, "qobuz")["sign_in"], "streamrip's sign-in · hi-res");
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/qobuz/signin/import",
            Some(serde_json::json!({"text": "garbage"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        assert!(r["error"].as_str().unwrap().starts_with("Not usable"), "{r}");
        let (_, r) = call(&app, "DELETE", "/v1/sources/qobuz/signin", None).await;
        assert!(r["in_use"].is_null(), "{r}");
        assert_eq!(r["line"], "Not signed in");
        // Last.fm: rescrobbled's files are listed (the import itself is in the Last.fm mock test)
        let (st, l) = call(&app, "GET", "/v1/sources/lastfm/signin", None).await;
        assert_eq!(st, StatusCode::OK, "{l}");
        assert_eq!(ids(&l), ["own", "import", "paste"]);
        assert_eq!(l["imports"][0]["id"], "rescrobbled");
        assert_eq!(l["imports"][0]["usable"], true, "{l}");
        assert!(
            !l.to_string().contains("sessKeyRescrobbled") && !l.to_string().contains("fedcba98"),
            "{l}"
        );
        // YouTube: a cookies file is picked, never looked for; the copy is the node's own, 0600
        let (_, y) = call(&app, "GET", "/v1/sources/youtube/signin", None).await;
        assert_eq!(ids(&y), ["key", "cookies"]);
        assert!(y["imports"].as_array().is_none_or(|a| a.is_empty()), "{y}");
        assert!(y["places"][0].as_str().unwrap().contains("not looked for"), "{y}");
        assert_eq!(y["line"], "Not signed in");
        let cookies =
            "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t2000000000\tSID\tcookie-secret\n";
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/youtube/signin/import",
            Some(serde_json::json!({"text": cookies})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["in_use"]["way"], "cookies");
        assert_eq!(r["line"], "yt-dlp cookies · signed-in playback");
        assert!(!r.to_string().contains("cookie-secret"), "{r}");
        let file = s.data_dir().join("youtube").join("cookies.txt");
        assert!(file.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        assert_eq!(
            row(&srcs, "youtube")["sign_in"],
            "yt-dlp cookies · signed-in playback"
        );
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/youtube/signin/import",
            Some(serde_json::json!({"text": "junk"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        let (_, r) = call(&app, "DELETE", "/v1/sources/youtube/signin", None).await;
        assert_eq!(r["line"], "Not signed in");
        assert!(!file.exists(), "the copy goes with Sign out");
        // Spotify: a librespot credentials.json is picked; librespot itself may be missing here
        let (_, sp) = call(&app, "GET", "/v1/sources/spotify/signin", None).await;
        assert_eq!(ids(&sp), ["app", "librespot", "librespot-file"]);
        assert!(
            sp["places"][0].as_str().unwrap().contains("not looked for"),
            "{sp}"
        );
        let creds = r#"{"username": "alice", "auth_type": 1, "auth_data": "QUJD"}"#;
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/spotify/signin/import",
            Some(serde_json::json!({"text": creds})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["in_use"]["way"], "librespot-file");
        assert_eq!(r["in_use"]["user"], "alice");
        assert_eq!(r["line"], "librespot, from a credentials file · alice");
        assert!(!r.to_string().contains("auth_data"), "{r}");
        let file = s.data_dir().join("librespot").join("credentials.json");
        assert!(file.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sources/spotify/signin/import",
            Some(serde_json::json!({"text": "{\"username\": \"x\"}"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
        let (_, r) = call(&app, "DELETE", "/v1/sources/spotify/signin", None).await;
        assert_eq!(r["line"], "Not signed in");
        assert!(!file.exists(), "the copy goes with Sign out");
        *imports::HOME_OVERRIDE.write().unwrap() = None;
    }

    /// A node at `public_url`, which the binary guessed (none configured) or the owner set.
    async fn state_at(dir: &std::path::Path, public_url: &str, guessed: bool) -> Arc<NodeState> {
        let db = dir.join("lib.sqlite");
        let s = NodeState::new(
            NodeSettings {
                name: "test".into(),
                version: "0".into(),
                db_path: db.clone(),
                scan_folders: vec![],
                art_dir: dir.join("art"),
                mirror_dir: dir.join("homeserver"),
                pipeline: PipelineConfig::default(),
                token: None,
                subsonic_user: "pubky".into(),
                pubky: Default::default(),
                public_url: public_url.into(),
                upnp: false,
                cast: false,
                slimproto_port: 0,
                session_secret_path: None,
                tidal: Default::default(),
                spotify: Default::default(),
                acoustid_key: String::new(),
                jamendo: Default::default(),
            },
            Library::open(&db).unwrap(),
            OutputRegistry::new(),
        );
        s.set_public_url_guessed(guessed);
        s.tidal.write().unwrap().creds = tidal::TidalCredentials {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            scopes: String::new(),
        };
        s
    }

    async fn tidal_redirect(s: Arc<NodeState>, host: &str) -> String {
        let req = Request::builder()
            .uri("/v1/sources/tidal/status")
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        let resp = router(s).oneshot(req).await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["redirect_uri"].as_str().unwrap().to_string()
    }

    /// The LAN address the node picks for speakers when public_url is empty is not where the
    /// browser is: sign-in callbacks follow the request (localhost, umbrel.local, a proxy). A
    /// public_url the owner set is used as it is.
    #[tokio::test]
    async fn a_guessed_public_url_leaves_sign_in_callbacks_to_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let lan = "http://192.168.1.10:7788";
        for host in ["localhost:7788", "umbrel.local:7788"] {
            let s = state_at(dir.path(), lan, true).await;
            assert_eq!(
                tidal_redirect(s, host).await,
                format!("http://{host}/tidal/callback")
            );
        }
        let s = state_at(dir.path(), lan, false).await;
        assert_eq!(
            tidal_redirect(s, "localhost:7788").await,
            "http://192.168.1.10:7788/tidal/callback"
        );
    }

    /// The network check says when an address the node picked for itself looks wrong for
    /// speakers (Docker's bridge, a VPN); never about one the owner set.
    #[tokio::test]
    async fn the_network_check_doubts_a_guessed_bridge_or_vpn_address() {
        let dir = tempfile::tempdir().unwrap();
        let notes = |s: Arc<NodeState>| async move {
            let (_, r) = call(&router(s), "GET", "/v1/outputs/discovery?quick=1", None).await;
            r["notes"].to_string()
        };
        let docker = notes(state_at(dir.path(), "http://172.17.0.2:7788", true).await).await;
        assert!(docker.contains("Docker's bridge network"), "{docker}");
        let vpn = notes(state_at(dir.path(), "http://100.101.2.3:7788", true).await).await;
        assert!(vpn.contains("a VPN?"), "{vpn}");
        let home = notes(state_at(dir.path(), "http://192.168.1.10:7788", true).await).await;
        assert!(!home.contains("picked"), "{home}");
        let set = notes(state_at(dir.path(), "http://172.17.0.2:7788", false).await).await;
        assert!(!set.contains("picked"), "{set}");
    }

    /// Library matching (F1): an album is matched by title, artist and song count, a tagged
    /// release by its id, and each artist by its albums' credits; an artist with nothing but a
    /// name that several MusicBrainz artists share stays unmatched. Pages carry the id and where
    /// it came from, and a second pass asks nothing.
    #[tokio::test]
    async fn library_artists_and_albums_are_matched_without_trusting_a_name_alone() {
        const RG_TAXI: &str = "0b5d3e2b-4b0c-4d5f-9d4b-1e9a2b3c4d5e";
        const TAXI: &str = "2d7f5a4d-6d2e-4f7b-9f6d-3a1c4d5e6f70";
        const RELEASE: &str = "4f9b7c6f-8f4a-4b9d-9b8f-5c3e6f708192";
        const RG_SOLO: &str = "5a0c8d7a-9a5b-4c0e-8c9a-6d4f708192a3";
        const SOLO: &str = "6b1d9e8b-0b6c-4d1f-9d0b-7e5a8192a3b4";
        const SMITH_1: &str = "7c2e0f9c-1c7d-4e2a-8e1c-8f6b92a3b4c5";
        const SMITH_2: &str = "8d3f1a0d-2d8e-4f3b-9f2d-9a7ca3b4c5d6";
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let credit = |name: &str, id: &str| serde_json::json!([{"name": name, "joinphrase": "", "artist": {"id": id, "name": name}}]);
        let (c1, c2, c3) = (calls.clone(), calls.clone(), calls.clone());
        let search = {
            let taxi = credit("7 Dollar Taxi", TAXI);
            let smith = credit("John Smith", SMITH_1);
            move |Query(q): Query<std::collections::HashMap<String, String>>| {
                c1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let query = q.get("query").cloned().unwrap_or_default();
                let releases = if query.contains("Zahltag") {
                    serde_json::json!([
                        {"title": "Zahltag", "track-count": 5, "release-group": {"id": RG_SOLO}, "artist-credit": taxi},
                        {"title": "Zahltag", "track-count": 2, "release-group": {"id": RG_TAXI}, "artist-credit": taxi}
                    ])
                } else {
                    // John Smith's "Hits" has more songs than the library's copy
                    serde_json::json!([{"title": "Hits", "track-count": 9, "release-group": {"id": RG_TAXI}, "artist-credit": smith}])
                };
                async move { Json(serde_json::json!({"releases": releases})) }
            }
        };
        let mock = Router::new()
            .route("/ws/2/release", get(search))
            .route(
                &format!("/ws/2/release/{RELEASE}"),
                get(move || {
                    c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        Json(serde_json::json!({"id": RELEASE, "release-group": {"id": RG_SOLO},
                            "artist-credit": [{"name": "Solo", "joinphrase": "", "artist": {"id": SOLO, "name": "Solo"}}]}))
                    }
                }),
            )
            .route(
                "/ws/2/recording",
                get(move || {
                    c3.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Two artists called John Smith with a song of that title and length
                    async move {
                        Json(serde_json::json!({"recordings": [
                            {"title": "Morning", "length": 180_000, "artist-credit": [{"name": "John Smith", "artist": {"id": SMITH_1, "name": "John Smith"}}]},
                            {"title": "Morning", "length": 181_000, "artist-credit": [{"name": "John Smith", "artist": {"id": SMITH_2, "name": "John Smith"}}]}
                        ]}))
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        s.set_contact("tests@example.org").unwrap();
        *s.discovery.write().unwrap() = None;
        *s.mb_base.write().unwrap() = format!("http://127.0.0.1:{port}/ws/2");
        {
            let mut b = s.info_bases.write().unwrap();
            b.musicbrainz = "http://127.0.0.1:9".into();
            b.fanart_key = None;
            b.audiodb = "http://127.0.0.1:9".into();
        }
        let song = |file: &str, title: &str, artist: &str, album: Option<(&str, Option<&str>)>, ms: u64| {
            let mut t = untagged_track(dir.path().join(file), title, artist);
            t.duration_ms = Some(ms);
            t.album = album.map(|(name, release)| pm_model::Album {
                id: None,
                title: name.into(),
                album_artist: Some(artist.into()),
                release_mbid: release.map(|r| pm_model::Mbid::parse(r).unwrap()),
                release_group_mbid: None,
                year: None,
                upc: None,
                total_discs: None,
            });
            t
        };
        {
            let lib = s.library.lock().unwrap();
            for t in [
                song(
                    "1.flac",
                    "Taxi",
                    "7 Dollar Taxi",
                    Some(("Zahltag (Deluxe)", None)),
                    200_000,
                ),
                song(
                    "2.flac",
                    "Zahltag",
                    "7 Dollar Taxi",
                    Some(("Zahltag (Deluxe)", None)),
                    210_000,
                ),
                song(
                    "3.flac",
                    "Alone",
                    "Solo",
                    Some(("Solo Album", Some(RELEASE))),
                    190_000,
                ),
                song("4.flac", "Morning", "John Smith", Some(("Hits", None)), 180_500),
            ] {
                lib.upsert_track(&t).unwrap();
            }
        }
        let report = library_match::pass(&s, std::time::Duration::ZERO).await;
        assert_eq!(report.failed, 0, "{report:?}");
        // Three albums, then John Smith's song (his album didn't match)
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4, "{report:?}");
        let app = router(s.clone());
        let (_, m) = call(&app, "GET", "/v1/library/matches", None).await;
        assert_eq!(m["albums"]["lookup"], 1, "{m}");
        assert_eq!(m["albums"]["tags"], 1, "{m}");
        assert_eq!(m["albums"]["unmatched"], 1, "{m}");
        assert_eq!(m["artists"]["lookup"], 2, "{m}");
        assert_eq!(m["artists"]["unmatched"], 1, "{m}");
        assert_eq!(m["artists"]["pending"], 0, "{m}");
        let artists = s.library.lock().unwrap().list_artists().unwrap();
        let id_of = |name: &str| artists.iter().find(|a| a.name == name).unwrap().id;
        let (_, p) = call(
            &app,
            "GET",
            &format!("/v1/item/artist/lib-{}", id_of("7 Dollar Taxi")),
            None,
        )
        .await;
        assert_eq!(p["mb_match"]["mbid"], TAXI, "{p}");
        assert_eq!(p["mb_match"]["source"], "lookup");
        assert_eq!(p["uri"], format!("https://musicbrainz.org/artist/{TAXI}"));
        let (_, p) = call(
            &app,
            "GET",
            &format!("/v1/item/artist/lib-{}", id_of("Solo")),
            None,
        )
        .await;
        assert_eq!(p["mb_match"]["mbid"], SOLO, "{p}");
        let (_, p) = call(
            &app,
            "GET",
            &format!("/v1/item/artist/lib-{}", id_of("John Smith")),
            None,
        )
        .await;
        assert!(
            p["mb_match"].is_null(),
            "a name and a song two artists share: {p}"
        );
        let albums = s.library.lock().unwrap().list_albums(None, 100).unwrap();
        let album_of = |t: &str| albums.iter().find(|a| a.title == t).unwrap().id.unwrap();
        let (_, p) = call(
            &app,
            "GET",
            &format!("/v1/item/album/lib-{}", album_of("Zahltag (Deluxe)")),
            None,
        )
        .await;
        assert_eq!(
            p["mb_match"],
            serde_json::json!({"mbid": RG_TAXI, "source": "lookup"}),
            "{p}"
        );
        let (_, p) = call(
            &app,
            "GET",
            &format!("/v1/item/album/lib-{}", album_of("Solo Album")),
            None,
        )
        .await;
        assert_eq!(
            p["mb_match"],
            serde_json::json!({"mbid": RG_SOLO, "source": "tags"}),
            "{p}"
        );
        assert_eq!(
            p["uri"],
            format!("https://musicbrainz.org/release-group/{RG_SOLO}")
        );
        // Answered, sure or not: nothing is asked again
        let again = library_match::pass(&s, std::time::Duration::ZERO).await;
        assert_eq!(again, library_match::PassReport::default());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    /// About from open sources (F2–F5): the artist's "Also on" from MusicBrainz's links, its
    /// Commons picture with author and licence, Wikidata facts with labels and the Wikipedia
    /// summary in the reader's language; the album's label, release date and the English article
    /// when there is none in that language; the song's work and its article. Cached per language.
    #[tokio::test]
    async fn library_pages_say_what_open_sources_know_in_the_readers_language() {
        const ARTIST: &str = "2d7f5a4d-6d2e-4f7b-9f6d-3a1c4d5e6f70";
        const GROUP: &str = "0b5d3e2b-4b0c-4d5f-9d4b-1e9a2b3c4d5e";
        const RELEASE: &str = "4f9b7c6f-8f4a-4b9d-9b8f-5c3e6f708192";
        const RECORDING: &str = "5a0c8d7a-9a5b-4c0e-8c9a-6d4f708192a3";
        const WORK: &str = "6b1d9e8b-0b6c-4d1f-9d0b-7e5a8192a3b4";
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let rel = |kind: &str, url: &str| serde_json::json!({"type": kind, "url": {"resource": url}});
        let claim = |v: serde_json::Value| serde_json::json!([{"rank": "normal", "mainsnak": {"datavalue": {"value": v}}}]);
        let item = |q: &str| claim(serde_json::json!({"id": q}));
        let mut answers: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        answers.insert(
            format!("/ws/2/artist/{ARTIST}"),
            serde_json::json!({"relations": [
                rel("wikidata", "https://www.wikidata.org/wiki/Q1"),
                rel("official homepage", "https://7dollartaxi.example/"),
                rel("free streaming", "https://open.spotify.com/artist/abc"),
                rel("streaming", "https://tidal.com/browse/artist/42"),
            ]}),
        );
        answers.insert("/wiki/Special:EntityData/Q1.json".into(), serde_json::json!({"entities": {"Q1": {
            "claims": {
                "P31": item("Q215380"), "P740": item("Q72"), "P136": item("Q11399"),
                "P527": [{"mainsnak": {"datavalue": {"value": {"id": "Q100"}}}}, {"mainsnak": {"datavalue": {"value": {"id": "Q101"}}}}],
                "P571": claim(serde_json::json!({"time": "+1994-00-00T00:00:00Z"})),
                "P18": claim(serde_json::json!("Taxi Band.jpg"))
            },
            "sitelinks": {"dewiki": {"title": "7 Dollar Taxi (Band)"}, "enwiki": {"title": "7 Dollar Taxi"}}
        }}}));
        answers.insert(
            format!("/ws/2/release-group/{GROUP}"),
            serde_json::json!({
                "first-release-date": "2001-05-04",
                "releases": [{"id": RELEASE, "status": "Official", "date": "2001-05-04"}],
                "relations": [rel("wikidata", "https://www.wikidata.org/wiki/Q2")]
            }),
        );
        answers.insert(
            format!("/ws/2/release/{RELEASE}"),
            serde_json::json!({
                "label-info": [{"label": {"name": "Sound Service"}}],
                "relations": [rel("purchase for download", "https://7dollartaxi.bandcamp.com/album/zahltag"),
                              rel("free streaming", "https://www.deezer.com/album/77")]
            }),
        );
        answers.insert(
            "/wiki/Special:EntityData/Q2.json".into(),
            serde_json::json!({"entities": {"Q2": {
            "claims": {}, "sitelinks": {"enwiki": {"title": "Zahltag (album)"}}}}}),
        );
        answers.insert(
            format!("/ws/2/recording/{RECORDING}"),
            serde_json::json!({"relations": [
            {"type": "performance", "target-type": "work", "work": {"id": WORK, "title": "Taxi"}}]}),
        );
        answers.insert(
            format!("/ws/2/work/{WORK}"),
            serde_json::json!({"relations": [
            rel("wikidata", "https://www.wikidata.org/wiki/Q3")]}),
        );
        answers.insert(
            "/wiki/Special:EntityData/Q3.json".into(),
            serde_json::json!({"entities": {"Q3": {
            "claims": {}, "sitelinks": {"dewiki": {"title": "Taxi (Lied)"}}}}}),
        );
        let answers = Arc::new(answers);
        let b2 = base.clone();
        let c2 = calls.clone();
        let mock = Router::new()
            .route("/w/api.php", get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
                let b = b2.clone();
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if q.get("action").map(String::as_str) == Some("wbgetentities") {
                        assert_eq!(q["languages"], "de|en");
                        let de = |n: &str| serde_json::json!({"labels": {"de": {"value": n}}});
                        return Json(serde_json::json!({"entities": {
                            "Q72": de("Zürich"), "Q11399": de("Rockmusik"), "Q100": de("Anna"),
                            "Q101": {"labels": {"en": {"value": "Ben"}}}}}));
                    }
                    assert_eq!(q["titles"], "File:Taxi_Band.jpg");
                    Json(serde_json::json!({"query": {"pages": {"7": {"imageinfo": [{
                        "thumburl": format!("{b}/img/taxi.jpg"),
                        "descriptionurl": "https://commons.wikimedia.org/wiki/File:Taxi_Band.jpg",
                        "extmetadata": {
                            "Artist": {"value": "<a href=\"//commons.wikimedia.org/wiki/User:Jane\">Jane Doe</a>"},
                            "LicenseShortName": {"value": "CC BY-SA 4.0"},
                            "LicenseUrl": {"value": "https://creativecommons.org/licenses/by-sa/4.0"}
                        }}]}}}}))
                }
            }))
            .route("/api/rest_v1/page/summary/{title}", get(|Path(title): Path<String>| async move {
                let text = match title.as_str() {
                    "7_Dollar_Taxi_(Band)" => "7 Dollar Taxi ist eine Band aus Zürich.",
                    "Zahltag_(album)" => "Zahltag is the first album by 7 Dollar Taxi.",
                    "Taxi_(Lied)" => "Taxi ist ein Lied von 7 Dollar Taxi.",
                    other => panic!("no article {other}"),
                };
                Json(serde_json::json!({"title": title.replace('_', " "), "extract": text,
                    "content_urls": {"desktop": {"page": format!("https://de.wikipedia.org/wiki/{title}")}}}))
            }))
            .route("/img/taxi.jpg", get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], vec![0xffu8, 0xd8, 0xff, 1, 2, 3]) }))
            .fallback({
                let c = calls.clone();
                move |uri: axum::http::Uri| {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let a = answers.clone();
                    async move {
                        match a.get(uri.path()) {
                            Some(v) => Json(v.clone()).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                }
            });
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        {
            let mut b = s.info_bases.write().unwrap();
            b.musicbrainz = base.clone();
            b.wikidata = base.clone();
            b.wikipedia = base.clone();
            b.commons = base.clone();
            b.fanart_key = None;
            b.audiodb = "http://127.0.0.1:9".into();
            b.wikimedia_interval = std::time::Duration::ZERO;
        }
        let (artist, album, track) = {
            let lib = s.library.lock().unwrap();
            let mut t = untagged_track(dir.path().join("taxi.flac"), "Taxi", "7 Dollar Taxi");
            t.artists[0].mbid = Some(pm_model::Mbid::parse(ARTIST).unwrap());
            t.recording_mbid = Some(pm_model::Mbid::parse(RECORDING).unwrap());
            t.album = Some(pm_model::Album {
                id: None,
                title: "Zahltag".into(),
                album_artist: Some("7 Dollar Taxi".into()),
                release_mbid: Some(pm_model::Mbid::parse(RELEASE).unwrap()),
                release_group_mbid: Some(pm_model::Mbid::parse(GROUP).unwrap()),
                year: Some(2001),
                upc: None,
                total_discs: None,
            });
            let tid = lib.upsert_track(&t).unwrap();
            let t = lib.get_track(tid).unwrap().unwrap();
            (t.artists[0].id.unwrap(), t.album.unwrap().id.unwrap(), tid.0)
        };
        let app = router(s.clone());
        let fact = |p: &serde_json::Value, label: &str| {
            p["about"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["label"] == label)
                .map(|f| f["value"].clone())
        };
        let (st, p) = call(
            &app,
            "GET",
            &format!("/v1/item/artist/lib-{artist}?lang=de-CH"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{p}");
        assert_eq!(
            p["about_text"]["text"], "7 Dollar Taxi ist eine Band aus Zürich.",
            "{p}"
        );
        assert_eq!(p["about_text"]["lang"], "de");
        assert_eq!(p["about_text"]["licence"], "CC BY-SA 4.0");
        assert_eq!(p["image"], format!("/v1/library/artists/{artist}/image"));
        assert_eq!(p["image_credit"]["author"], "Jane Doe", "{p}");
        assert_eq!(p["image_credit"]["licence"], "CC BY-SA 4.0");
        assert_eq!(
            p["image_credit"]["file_page"],
            "https://commons.wikimedia.org/wiki/File:Taxi_Band.jpg"
        );
        assert_eq!(fact(&p, "Origin"), Some("Zürich".into()), "{p}");
        assert_eq!(fact(&p, "Active"), Some("since 1994".into()));
        assert_eq!(fact(&p, "Genres"), Some("Rockmusik".into()));
        assert_eq!(
            fact(&p, "Members"),
            Some("Anna, Ben".into()),
            "English where German has no label"
        );
        // In the play-from order, as every page's sources
        let sources = |p: &serde_json::Value| {
            let mut v: Vec<String> = p["also_on"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["source"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(sources(&p), ["homepage", "spotify", "tidal"], "{p}");
        let tidal = p["also_on"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["source"] == "tidal")
            .unwrap();
        assert_eq!(tidal["id"], "artist:42");
        // The album: no German article, so the English one; label and date from the release
        let req = Request::builder()
            .uri(format!("/v1/item/album/lib-{album}"))
            .header(header::ACCEPT_LANGUAGE, "de-DE,de;q=0.9")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let p: serde_json::Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(p["about_text"]["lang"], "en", "{p}");
        assert_eq!(p["blurb"], "Zahltag is the first album by 7 Dollar Taxi.");
        assert_eq!(fact(&p, "Released"), Some("2001-05-04".into()), "{p}");
        assert_eq!(fact(&p, "Label"), Some("Sound Service".into()));
        assert_eq!(sources(&p), ["bandcamp", "deezer"], "{p}");
        // The song: its work's article
        let (_, p) = call(&app, "GET", &format!("/v1/item/song/lib-{track}?lang=de"), None).await;
        assert_eq!(
            p["about_text"]["text"], "Taxi ist ein Lied von 7 Dollar Taxi.",
            "{p}"
        );
        assert_eq!(fact(&p, "Work"), Some("Taxi".into()));
        // Cached per language: the same pages again ask nothing
        let asked = calls.load(std::sync::atomic::Ordering::SeqCst);
        call(
            &app,
            "GET",
            &format!("/v1/item/artist/lib-{artist}?lang=de"),
            None,
        )
        .await;
        call(&app, "GET", &format!("/v1/item/song/lib-{track}?lang=de"), None).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), asked);
    }

    #[tokio::test]
    async fn load_fills_a_zone_at_a_position() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("long.wav");
        write_wav(&wav, 44_100 * 30);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let items: Vec<_> = (0..3)
            .map(|i| serde_json::json!({"uri": wav.to_string_lossy(), "title": format!("t{i}")}))
            .collect();
        // Nothing to move
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/load"),
            Some(serde_json::json!({"items": []})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/load"),
            Some(serde_json::json!({"items": [{"uri": " "}]})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // A paused browser queue: the second song at 5 s, repeat all
        let (st, z) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/load"),
            Some(serde_json::json!({"items": items, "index": 1, "position_ms": 5000, "transport": "paused", "repeat": "all"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{z}");
        assert_eq!(z["queue"].as_array().unwrap().len(), 3);
        assert_eq!(z["queue"][1]["title"], "t1");
        assert_eq!(z["repeat"], "all");
        assert_eq!(z["now_playing"]["index"], 1);
        assert_eq!(z["transport"], "paused");
        assert!(z["now_playing"]["position_ms"].as_u64().unwrap() >= 4_000, "{z}");
        // Stopped: loaded, nothing plays
        let one = vec![serde_json::json!({"uri": wav.to_string_lossy(), "title": "only"})];
        let (st, z) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/load"),
            Some(serde_json::json!({"items": one})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{z}");
        assert_eq!(z["queue"].as_array().unwrap().len(), 1);
        assert_eq!(z["transport"], "stopped");
        assert_eq!(z["repeat"], "off");
    }
}
