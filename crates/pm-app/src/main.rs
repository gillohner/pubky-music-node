//! `pm-app`: the native controller and player.
//!
//! Layout (Roon-like): a left navigation rail (Albums, Tracks, Queue,
//! Settings), a search field on top, the content in the middle and a
//! transport bar at the bottom with the now-playing item, position,
//! transport buttons, volume, the zone picker and the signal-path badge
//! that opens the full chain. Zones are the node's zones plus "This
//! computer", a local zone that plays library tracks through this
//! machine's DAC.

mod backend;
mod local;
mod theme;

use backend::{Backend, Cmd, DiscoverKind, Msg};
use local::{LocalPlayer, LOCAL_ZONE_ID};
use pm_api::sources::NodeSources;
use pm_api::types::{
    ArtistDetail, DiscoverPage, FavouriteRef, Info, PlaylistDetail, PlaylistSummary, QueueAt, TransportAction,
};
use pm_model::{Album, Quality, SignalPath, Track, Volume};
use pm_zones::{Insert, QueueItem, Transport, ZoneState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pubky Music")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("Pubky Music", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    node_url: String,
    token: String,
    /// Exported Pubky session secret; lets the app find the node via the homeserver (ADR 0008).
    #[serde(default)]
    session_secret: String,
    /// Last selected zone id; `local` for this computer.
    zone: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            node_url: std::env::var("PM_NODE").unwrap_or_else(|_| "http://127.0.0.1:7788".into()),
            token: std::env::var("PM_TOKEN").unwrap_or_default(),
            session_secret: std::env::var("PM_SESSION_SECRET").unwrap_or_default(),
            zone: LOCAL_ZONE_ID.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    Home,
    NowPlaying,
    Favourites,
    Radio,
    Sources,
    Albums,
    Album(i64),
    Artists,
    Artist(i64),
    Genres,
    Genre(i64),
    Playlists,
    Playlist(String),
    Discover,
    Tracks,
    Search,
    Queue,
    Settings,
}

#[derive(Debug, Clone)]
enum Conn {
    Connecting,
    Connected(Info),
    Failed(String),
}

struct App {
    cover_ctx: egui::Context,
    settings: Settings,
    backend: Backend,
    local: LocalPlayer,
    conn: Conn,
    zones: BTreeMap<String, ZoneState>,
    outputs: Vec<pm_zones::OutputInfo>,
    albums: Vec<Album>,
    home: Option<pm_api::types::HomePage>,
    favourites: Option<pm_api::types::Favourites>,
    /// My tags per canonical URL (album and artist pages).
    tags: HashMap<String, Vec<pm_api::types::TagEntry>>,
    tag_input: String,
    /// My review per canonical URL, and the draft being edited (url, rating, text).
    reviews: HashMap<String, Option<pm_api::types::ReviewEntry>>,
    /// Pubky's view per canonical URL; `None` once the node said discovery is off.
    on_pubky: HashMap<String, pm_api::types::OnPubky>,
    discovery_off: bool,
    review_draft: Option<(String, Option<u8>, String)>,
    folders: Vec<(String, bool)>,
    folder_input: String,
    node_settings: Option<pm_api::types::NodeSettingsView>,
    contact_input: String,
    radio_query: String,
    radio_results: Option<(String, Vec<pm_client::RadioStation>)>,
    radio_searching: bool,
    stations: Vec<pm_client::RadioStation>,
    sources: Vec<pm_api::types::SourceInfo>,
    source_id: String,
    archive_query: String,
    archive_results: Option<(String, Vec<pm_api::types::SourceHit>)>,
    archive_searching: bool,
    archive_item: Option<pm_api::types::SourceItem>,
    archive_loading: Option<String>,
    /// Canonical URLs the user has hearted.
    fav_uris: std::collections::HashSet<String>,
    /// Tracks the app has seen, by library id, to find covers for queue items.
    known_tracks: HashMap<i64, Track>,
    /// Last local-zone item reported to the node's history (queue item id, uri).
    last_local_play: Option<(u64, String)>,
    /// Where to return from the Now Playing screen.
    prev_view: Option<View>,
    /// An album whose tracks were requested in order to play it.
    pending_play_album: Option<i64>,
    album_tracks: HashMap<i64, Vec<Track>>,
    tracks: Vec<Track>,
    search: String,
    results: Option<(String, pm_api::types::SearchResults)>,
    queues: HashMap<String, Vec<QueueItem>>,
    /// Lyrics by track id; `None` once the node said there are none.
    lyrics: HashMap<i64, Option<Box<pm_api::types::Lyrics>>>,
    lyrics_requested: HashSet<i64>,
    album_formats: HashMap<i64, pm_library::AlbumFormat>,
    album_focus: Option<pm_library::Focus>,
    /// Private files on the homeserver as last listed (`Err` = not signed in / unreachable).
    pubky_files: Option<Result<Vec<pm_pubky::homeserver::RemoteFile>, String>>,
    /// Sleep timer for the local player (node zones keep theirs on the node).
    local_sleep_at: Option<Instant>,
    new_releases: Option<pm_api::types::NewReleasesPage>,
    playlists: Vec<PlaylistSummary>,
    playlist_details: HashMap<String, PlaylistDetail>,
    artists: Vec<pm_library::ArtistSummary>,
    genres: Vec<pm_library::GenreSummary>,
    genre_details: HashMap<i64, pm_api::types::GenreDetail>,
    artist_infos: HashMap<i64, (pm_api::types::ArtistInfo, Option<egui::TextureHandle>)>,
    artist_details: HashMap<i64, ArtistDetail>,
    discover_kind: DiscoverKind,
    discover: Option<(DiscoverKind, DiscoverPage)>,
    discover_loading: bool,
    friends_viewer: String,
    new_zone_name: String,
    new_zone_outputs: Vec<String>,
    show_queue: bool,
    show_dsp: bool,
    /// Pubky Ring sign-in on the node: the URL to show and its QR texture.
    login: Option<(String, Option<egui::TextureHandle>, std::time::Instant)>,
    /// DSP editor state: zone id it belongs to and the config being edited.
    dsp_edit: Option<(String, pm_audio::PipelineConfig)>,
    /// Last queue version seen per remote zone, to refetch the queue on change.
    queue_versions: HashMap<String, u64>,
    new_playlist_name: String,
    import_path: String,
    /// Album id → texture (`None` once the node said there is no cover).
    covers: HashMap<i64, Option<egui::TextureHandle>>,
    cover_requests: std::collections::HashSet<i64>,
    view: View,
    show_path: bool,
    volume_db: f32,
    status: Option<(String, std::time::Instant)>,
    scanning: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, "settings"))
            .unwrap_or_default();
        let ctx = cc.egui_ctx.clone();
        let backend = Backend::start(move || ctx.request_repaint());
        let local = LocalPlayer::new(backend.client.clone(), backend.handle.clone(), cache_dir());
        backend.send(Cmd::Connect {
            base: settings.node_url.clone(),
            token: settings.token.clone(),
        });
        Self {
            cover_ctx: cc.egui_ctx.clone(),
            settings,
            backend,
            local,
            conn: Conn::Connecting,
            zones: BTreeMap::new(),
            outputs: Vec::new(),
            albums: Vec::new(),
            home: None,
            favourites: None,
            tags: HashMap::new(),
            tag_input: String::new(),
            reviews: HashMap::new(),
            on_pubky: HashMap::new(),
            discovery_off: false,
            review_draft: None,
            folders: Vec::new(),
            folder_input: String::new(),
            node_settings: None,
            contact_input: String::new(),
            radio_query: String::new(),
            radio_results: None,
            radio_searching: false,
            stations: Vec::new(),
            sources: Vec::new(),
            source_id: "archive".into(),
            archive_query: String::new(),
            archive_results: None,
            archive_searching: false,
            archive_item: None,
            archive_loading: None,
            fav_uris: std::collections::HashSet::new(),
            known_tracks: HashMap::new(),
            last_local_play: None,
            prev_view: None,
            pending_play_album: None,
            album_tracks: HashMap::new(),
            tracks: Vec::new(),
            search: String::new(),
            results: None,
            queues: HashMap::new(),
            lyrics: HashMap::new(),
            lyrics_requested: HashSet::new(),
            album_formats: HashMap::new(),
            album_focus: None,
            pubky_files: None,
            local_sleep_at: None,
            new_releases: None,
            playlists: Vec::new(),
            playlist_details: HashMap::new(),
            artists: Vec::new(),
            genres: Vec::new(),
            genre_details: HashMap::new(),
            artist_infos: HashMap::new(),
            artist_details: HashMap::new(),
            discover_kind: DiscoverKind::Trends,
            discover: None,
            discover_loading: false,
            friends_viewer: String::new(),
            new_zone_name: String::new(),
            new_zone_outputs: Vec::new(),
            show_queue: true,
            show_dsp: false,
            login: None,
            dsp_edit: None,
            queue_versions: HashMap::new(),
            new_playlist_name: String::new(),
            import_path: String::new(),
            covers: HashMap::new(),
            cover_requests: std::collections::HashSet::new(),
            view: View::Home,
            show_path: false,
            volume_db: -20.0,
            status: None,
            scanning: false,
        }
    }

    fn connect(&mut self) {
        self.backend.send(Cmd::Connect {
            base: self.settings.node_url.clone(),
            token: self.settings.token.clone(),
        });
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), std::time::Instant::now()));
    }

    fn drain_backend(&mut self) {
        for m in self.backend.poll() {
            match m {
                Msg::Connecting => self.conn = Conn::Connecting,
                Msg::Connected(info) => self.conn = Conn::Connected(info),
                Msg::ResolvedNode(url, token) => {
                    self.settings.node_url = url;
                    self.settings.token = token;
                }
                Msg::ConnectFailed(e) => self.conn = Conn::Failed(e),
                Msg::Disconnected(e) => self.toast(format!("event stream: {e}")),
                Msg::Zones(zs) => {
                    self.zones = zs.into_iter().map(|z| (z.id.0.clone(), z)).collect();
                }
                Msg::Zone(z) => {
                    let id = z.id.0.clone();
                    let changed =
                        self.queue_versions.insert(id.clone(), z.queue_version) != Some(z.queue_version);
                    self.zones.insert(id.clone(), *z);
                    if changed && id == self.settings.zone {
                        self.backend.send(Cmd::Queue(id));
                    }
                }
                Msg::ZoneRemoved(id) => {
                    self.zones.remove(&id.0);
                }
                Msg::Outputs(o) => self.outputs = o,
                Msg::Albums(a) => self.albums = a,
                Msg::RadioResults(q, r) => {
                    self.radio_searching = false;
                    self.radio_results = Some((q, r));
                }
                Msg::Stations(st) => self.stations = st,
                Msg::Sources(v) => {
                    if !v.iter().any(|x| x.id == self.source_id) {
                        if let Some(first) = v.first() {
                            self.source_id = first.id.clone();
                        }
                    }
                    self.sources = v;
                }
                Msg::SourceResults(q, r) => {
                    self.archive_searching = false;
                    self.archive_results = Some((q, r));
                }
                Msg::SourceItem(it) => {
                    self.archive_loading = None;
                    self.archive_item = Some(*it);
                }
                Msg::Folders(f) => self.folders = f,
                Msg::NodeSettings(v) => {
                    self.contact_input = v.contact.clone();
                    if v.discovery {
                        self.discovery_off = false;
                    }
                    self.node_settings = Some(v);
                }
                Msg::OnPubky(None) => self.discovery_off = true,
                Msg::OnPubky(Some(v)) => {
                    self.on_pubky.insert(v.uri.clone(), *v);
                }
                Msg::Review(r) => {
                    if let Some(uri) = r.uri.clone() {
                        self.reviews.insert(uri, r.reviews.into_iter().next());
                    }
                }
                Msg::Tags(t) => {
                    if let Some(uri) = t.uri.clone() {
                        self.tags.insert(uri, t.tags);
                    }
                }
                Msg::Favourites(f) => {
                    self.fav_uris = f.uris.iter().cloned().collect();
                    self.remember_tracks(f.tracks.iter());
                    self.favourites = Some(*f);
                }
                Msg::Home(h) => {
                    self.remember_tracks(h.most_played.iter().map(|p| &p.track));
                    self.remember_tracks(h.history.iter().map(|p| &p.track));
                    self.home = Some(*h);
                }
                Msg::AlbumTracks(id, t) => {
                    self.remember_tracks(t.iter());
                    self.album_tracks.insert(id, t.clone());
                    if self.pending_play_album == Some(id) {
                        self.pending_play_album = None;
                        self.play_tracks(&t, true, QueueAt::End);
                    }
                }
                Msg::ArtistTracks(t) => {
                    self.remember_tracks(t.iter());
                    self.play_tracks(&t, true, QueueAt::End);
                }
                Msg::Tracks(t) => {
                    self.remember_tracks(t.iter());
                    self.tracks = t;
                }
                Msg::SearchResults(q, r) => {
                    self.remember_tracks(r.tracks.iter());
                    self.results = Some((q, *r));
                }
                Msg::Queue(z, q) => {
                    self.queues.insert(z, q);
                }
                Msg::LibraryChanged => {
                    self.scanning = false;
                    self.backend.send(Cmd::LoadAlbums);
                    self.backend.send(Cmd::LoadHome);
                    self.backend.send(Cmd::LoadFavourites);
                    if self.view == View::Tracks {
                        self.backend.send(Cmd::LoadTracks);
                    }
                    self.toast("library updated");
                }
                Msg::ScanStarted => {
                    self.scanning = true;
                    self.toast("scan started");
                }
                Msg::Artists(a) => self.artists = a,
                Msg::Genres(g) => self.genres = g,
                Msg::PubkyFiles(r) => self.pubky_files = Some(r),
                Msg::NewReleases(nr) => self.new_releases = Some(*nr),
                Msg::AlbumFormats(f) => {
                    self.album_formats = f.into_iter().map(|x| (x.album_id, x)).collect();
                }
                Msg::Lyrics(id, l) => {
                    self.lyrics.insert(id, l);
                }
                Msg::ArtistInfo(id, info, img) => {
                    let tex = img.map(|(w, h, rgba)| {
                        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                        self.cover_ctx.load_texture(
                            format!("artist-{id}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        )
                    });
                    self.artist_infos.insert(id, (*info, tex));
                }
                Msg::Genre(d) => {
                    self.remember_tracks(d.tracks.iter());
                    self.genre_details.insert(d.genre.id, *d);
                }
                Msg::PubkyLoginUrl(url) => {
                    let tex = qr_texture(&self.cover_ctx, &url);
                    self.login = Some((url, tex, std::time::Instant::now()));
                }
                Msg::PubkyLoginDone(user) => {
                    self.login = None;
                    if let Some(u) = &user {
                        if self.friends_viewer.trim().is_empty() {
                            self.friends_viewer = u.clone();
                        }
                    }
                    self.toast(format!("node signed in as {}", user.unwrap_or_default()));
                }
                Msg::PubkyUser(user) => {
                    if let Some(u) = user {
                        if self.friends_viewer.trim().is_empty() {
                            self.friends_viewer = u;
                        }
                    }
                }
                Msg::Dsp(z, cfg) => {
                    if z == self.settings.zone {
                        self.dsp_edit = Some((z, *cfg));
                    }
                }
                Msg::Discovered(kind, page) => {
                    self.discover_loading = false;
                    self.discover = Some((kind, *page));
                }
                Msg::Artist(d) => {
                    if d.artist.mbid.is_some() {
                        self.backend
                            .send(Cmd::LoadTags(FavouriteRef::artist(d.artist.id)));
                    }
                    if !self.artist_infos.contains_key(&d.artist.id) {
                        self.backend.send(Cmd::LoadArtistInfo(d.artist.id));
                    }
                    self.artist_details.insert(d.artist.id, *d);
                }
                Msg::Playlists(p) => self.playlists = p,
                Msg::Playlist(d) => {
                    self.playlist_details.insert(d.summary.id.clone(), *d);
                }
                Msg::PlaylistDeleted(id) => {
                    self.playlist_details.remove(&id);
                    if self.view == View::Playlist(id) {
                        self.view = View::Playlists;
                    }
                }
                Msg::Notice(n) => self.toast(n),
                Msg::Cover(id, img) => {
                    let tex = img.map(|(w, h, rgba)| {
                        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                        self.cover_ctx.load_texture(
                            format!("cover-{id}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        )
                    });
                    self.covers.insert(id, tex);
                }
                Msg::Error(e) => {
                    self.discover_loading = false;
                    self.toast(e)
                }
            }
        }
    }

    // ---- zone helpers ----

    fn is_local(&self) -> bool {
        self.settings.zone == LOCAL_ZONE_ID
    }

    fn current_zone(&self) -> ZoneState {
        if self.is_local() {
            self.local.zone.state()
        } else {
            self.zones
                .get(&self.settings.zone)
                .cloned()
                .unwrap_or_else(|| self.local.zone.state())
        }
    }

    fn zone_name(&self, id: &str) -> String {
        if id == LOCAL_ZONE_ID {
            format!("This computer ({})", self.local.output_name)
        } else {
            self.zones
                .get(id)
                .map(|z| z.name.clone())
                .unwrap_or_else(|| id.to_string())
        }
    }

    fn remember_tracks<'a>(&mut self, tracks: impl Iterator<Item = &'a Track>) {
        for t in tracks {
            if let Some(id) = t.id {
                self.known_tracks.insert(id.0, t.clone());
            }
        }
    }

    /// The album a queue item belongs to: by its library id when the app
    /// has seen the track, else by album title and artist.
    fn album_of_item(&self, item: &QueueItem) -> Option<Album> {
        if let Some(t) = item
            .uri
            .strip_prefix("track:")
            .and_then(|s| s.parse::<i64>().ok())
            .and_then(|id| self.known_tracks.get(&id))
        {
            if let Some(a) = &t.album {
                if let Some(found) = self.albums.iter().find(|x| x.id == a.id) {
                    return Some(found.clone());
                }
                return Some(a.clone());
            }
        }
        let title = item.album.as_deref()?;
        self.albums
            .iter()
            .find(|a| {
                a.title == title
                    && (a.album_artist.is_none() || a.album_artist.as_deref() == item.artist.as_deref())
            })
            .or_else(|| self.albums.iter().find(|a| a.title == title))
            .cloned()
    }

    /// The local zone plays by itself; tell the node when a library track starts.
    fn report_local_play(&mut self) {
        let state = self.local.zone.state();
        let Some(np) = state.now_playing else {
            return;
        };
        let key = (np.item.id, np.item.uri.clone());
        if self.last_local_play.as_ref() == Some(&key) {
            return;
        }
        self.last_local_play = Some(key);
        if let Some(id) = np
            .item
            .uri
            .strip_prefix("track:")
            .and_then(|s| s.parse::<i64>().ok())
        {
            self.backend.send(Cmd::RecordPlay(id));
        }
    }

    fn is_favourite_uri(&self, uri: Option<&pm_model::MusicUri>) -> bool {
        uri.is_some_and(|u| self.fav_uris.contains(&u.to_url()))
    }

    /// A heart button; `uri` is the object's canonical URL when it has one.
    /// Returns `Some(on)` when clicked. Objects without an identity get a
    /// disabled heart that explains why.
    fn heart(&mut self, ui: &mut egui::Ui, uri: Option<pm_model::MusicUri>, what: FavouriteRef) {
        match uri {
            Some(u) => {
                let on = self.fav_uris.contains(&u.to_url());
                let label = if on { "♥" } else { "♡" };
                let btn = egui::Button::new(egui::RichText::new(label).size(16.0).color(if on {
                    theme::HEART
                } else {
                    ui.visuals().weak_text_color()
                }))
                .frame(false);
                if ui
                    .add(btn)
                    .on_hover_text(if on {
                        "Remove from favourites"
                    } else {
                        "Add to favourites"
                    })
                    .clicked()
                {
                    self.backend.send(Cmd::SetFavourite { what, on: !on });
                }
            }
            None => {
                ui.add_enabled(false, egui::Button::new("♡").frame(false))
                    .on_disabled_hover_text("No MusicBrainz id yet, so this cannot be a favourite");
            }
        }
    }

    fn open_now_playing(&mut self) {
        if self.view != View::NowPlaying {
            self.prev_view = Some(self.view.clone());
            self.view = View::NowPlaying;
        }
    }

    fn close_now_playing(&mut self) {
        self.view = self.prev_view.take().unwrap_or(View::Home);
    }

    /// Play a whole album: at once when its tracks are known, else after they load.
    fn play_album(&mut self, id: i64) {
        if let Some(tracks) = self.album_tracks.get(&id).cloned() {
            self.play_tracks(&tracks, true, QueueAt::End);
        } else {
            self.pending_play_album = Some(id);
            self.backend.send(Cmd::LoadAlbumTracks(id));
        }
    }

    fn open_album(&mut self, id: i64) {
        if !self.album_tracks.contains_key(&id) {
            self.backend.send(Cmd::LoadAlbumTracks(id));
        }
        if self
            .albums
            .iter()
            .any(|a| a.id == Some(id) && a.release_mbid.is_some())
        {
            self.backend.send(Cmd::LoadTags(FavouriteRef::album(id)));
            self.backend.send(Cmd::LoadReview(FavouriteRef::album(id)));
            if !self.discovery_off {
                self.backend.send(Cmd::LoadOnPubky(FavouriteRef::album(id)));
            }
        }
        self.view = View::Album(id);
    }

    /// What Pubky knows about the object: tag labels with counts, people.
    fn on_pubky_row(&mut self, ui: &mut egui::Ui, uri: Option<pm_model::MusicUri>) {
        let Some(uri) = uri else {
            return;
        };
        let Some(v) = self.on_pubky.get(&uri.to_url()).cloned() else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("On Pubky")
                    .size(12.0)
                    .strong()
                    .color(theme::MUTED),
            );
            if !v.indexed {
                ui.weak("not indexed yet");
                return;
            }
            if v.taggers > 0 {
                ui.weak(format!(
                    "{} {}",
                    v.taggers,
                    if v.taggers == 1 { "person" } else { "people" }
                ));
            }
            for (label, n) in v.tags.iter().take(12) {
                theme::pill(ui, Some(theme::ACCENT), &format!("#{label} · {n}"));
            }
            if v.tags.is_empty() {
                ui.weak("no tags yet");
            }
        });
    }

    /// Rating stars and notes for an object; private v1 review posts.
    fn review_row(&mut self, ui: &mut egui::Ui, uri: Option<pm_model::MusicUri>, what: FavouriteRef) {
        let Some(uri) = uri else {
            return;
        };
        let url = uri.to_url();
        let saved = self.reviews.get(&url).cloned().flatten();
        let editing = self.review_draft.as_ref().is_some_and(|(u, _, _)| *u == url);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("My rating")
                    .size(12.0)
                    .strong()
                    .color(theme::MUTED),
            );
            let current = if editing {
                self.review_draft.as_ref().and_then(|d| d.1)
            } else {
                saved.as_ref().and_then(|r| r.rating)
            };
            let mut clicked = None;
            for n in 1..=5u8 {
                let on = current.is_some_and(|c| c >= n);
                let star = egui::RichText::new(if on { "★" } else { "☆" })
                    .size(18.0)
                    .color(if on { theme::WARN } else { theme::MUTED });
                if ui
                    .add(egui::Button::new(star).frame(false))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    clicked = Some(n);
                }
            }
            if let Some(n) = clicked {
                let rating = if current == Some(n) { None } else { Some(n) };
                let text = saved.as_ref().map(|r| r.text.clone()).unwrap_or_default();
                if rating.is_none() && text.is_empty() {
                    self.backend.send(Cmd::DeleteReview(what.clone()));
                } else {
                    self.backend.send(Cmd::SetReview {
                        what: what.clone(),
                        rating,
                        text,
                    });
                }
            }
            if !editing {
                let label = if saved.as_ref().is_some_and(|r| !r.text.is_empty()) {
                    "Edit notes"
                } else {
                    "Add notes"
                };
                if ui.add(theme::secondary(label)).clicked() {
                    self.review_draft = Some((
                        url.clone(),
                        saved.as_ref().and_then(|r| r.rating),
                        saved.as_ref().map(|r| r.text.clone()).unwrap_or_default(),
                    ));
                }
            }
        });
        if editing {
            let mut save = false;
            let mut cancel = false;
            if let Some((_, _, text)) = self.review_draft.as_mut() {
                ui.add(
                    egui::TextEdit::multiline(text)
                        .hint_text("Notes, kept privately on your homeserver")
                        .desired_rows(3)
                        .desired_width(520.0),
                );
            }
            ui.horizontal(|ui| {
                if ui.add(theme::primary("Save")).clicked() {
                    save = true;
                }
                if ui.add(theme::secondary("Cancel")).clicked() {
                    cancel = true;
                }
            });
            if save {
                if let Some((_, rating, text)) = self.review_draft.take() {
                    let rating = rating.or(saved.as_ref().and_then(|r| r.rating));
                    if rating.is_none() && text.trim().is_empty() {
                        self.backend.send(Cmd::DeleteReview(what.clone()));
                    } else {
                        self.backend.send(Cmd::SetReview {
                            what: what.clone(),
                            rating,
                            text: text.trim().to_string(),
                        });
                    }
                }
            }
            if cancel {
                self.review_draft = None;
            }
        } else if let Some(r) = saved.as_ref().filter(|r| !r.text.is_empty()) {
            ui.label(egui::RichText::new(&r.text).color(theme::FG_2));
        }
    }

    /// Tag chips for an object plus an "Add a tag" field. Objects without a
    /// canonical identity show nothing: a tag needs a URL to point at.
    fn tag_row(&mut self, ui: &mut egui::Ui, uri: Option<pm_model::MusicUri>, what: FavouriteRef) {
        let Some(uri) = uri else {
            return;
        };
        let url = uri.to_url();
        let tags = self.tags.get(&url).cloned().unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("Tags")
                    .size(12.0)
                    .strong()
                    .color(theme::MUTED),
            );
            for t in &tags {
                let chip = egui::Button::new(
                    egui::RichText::new(format!("#{}  ✕", t.label))
                        .size(12.0)
                        .color(theme::FG_2),
                )
                .fill(theme::SURFACE_2)
                .stroke(egui::Stroke::new(1.0, theme::LINE))
                .corner_radius(egui::CornerRadius::same(12));
                if ui
                    .add(chip)
                    .on_hover_text(if t.synced {
                        "Public on your homeserver · click to remove"
                    } else {
                        "Not pushed yet · click to remove"
                    })
                    .clicked()
                {
                    self.backend.send(Cmd::SetTag {
                        what: what.clone(),
                        label: t.label.clone(),
                        on: false,
                    });
                }
            }
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.tag_input)
                    .hint_text("Add a tag…")
                    .desired_width(140.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter && !self.tag_input.trim().is_empty() {
                self.backend.send(Cmd::SetTag {
                    what: what.clone(),
                    label: self.tag_input.trim().to_string(),
                    on: true,
                });
                self.tag_input.clear();
            }
        });
        ui.small(
            egui::RichText::new(
                "Tags are public pubky.app tags on the canonical MusicBrainz URL; Discover reads them back through Pinakes.",
            )
            .weak(),
        );
    }

    fn play_tracks(&mut self, tracks: &[Track], replace: bool, at: QueueAt) {
        self.remember_tracks(tracks.iter());
        let ids: Vec<i64> = tracks.iter().filter_map(|t| t.id.map(|i| i.0)).collect();
        if ids.is_empty() {
            return;
        }
        if self.is_local() {
            let items: Vec<QueueItem> = tracks.iter().filter_map(NodeSources::item_for).collect();
            if replace {
                self.local.zone.stop();
                self.local.zone.clear();
            }
            self.local.zone.enqueue(
                items,
                match at {
                    QueueAt::End => Insert::End,
                    QueueAt::Next => Insert::Next,
                },
            );
            if replace {
                self.local.zone.play_index(0);
            }
        } else {
            self.backend.send(Cmd::Enqueue {
                zone: self.settings.zone.clone(),
                track_ids: ids,
                at,
                replace,
                play: replace,
            });
        }
    }

    fn transport(&mut self, action: TransportAction) {
        if self.is_local() {
            let z = &self.local.zone;
            match action {
                TransportAction::Play { index: Some(i) } => z.play_index(i),
                TransportAction::Play { index: None } => z.play(),
                TransportAction::Pause => z.pause(),
                TransportAction::Toggle => z.toggle_pause(),
                TransportAction::Stop => z.stop(),
                TransportAction::Next => z.next(),
                TransportAction::Previous => z.previous(),
                TransportAction::Seek { position_ms } => z.seek_ms(position_ms),
            }
        } else {
            self.backend.send(Cmd::Transport {
                zone: self.settings.zone.clone(),
                action,
            });
        }
    }

    fn set_volume(&mut self, db: f32) {
        self.volume_db = db;
        if self.is_local() {
            let zone = self.local.zone.clone();
            self.backend.handle.spawn(async move {
                let _ = zone.set_volume(Volume::Db(db)).await;
            });
        } else {
            self.backend.send(Cmd::Volume {
                zone: self.settings.zone.clone(),
                volume: Volume::Db(db),
            });
        }
    }

    fn queue_items(&self) -> Vec<QueueItem> {
        if self.is_local() {
            self.local.zone.queue()
        } else {
            self.queues.get(&self.settings.zone).cloned().unwrap_or_default()
        }
    }

    fn refresh_queue(&mut self) {
        if !self.is_local() {
            self.backend.send(Cmd::Queue(self.settings.zone.clone()));
        }
    }

    /// The album's cover texture, requesting it from the node on first use.
    fn cover(&mut self, album_id: Option<i64>) -> Option<egui::TextureHandle> {
        let id = album_id?;
        match self.covers.get(&id) {
            Some(t) => t.clone(),
            None => {
                if self.cover_requests.insert(id) {
                    self.backend.send(Cmd::LoadCover(id));
                }
                None
            }
        }
    }

    // ---- UI ----

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 11.0, theme::ACCENT);
            ui.painter().circle_filled(r.center(), 5.5, theme::SURFACE);
            ui.label(egui::RichText::new("Pubky Music").size(15.0).strong());
        });
        ui.add_space(14.0);
        theme::eyebrow(ui, "Library");
        ui.add_space(2.0);
        for (label, view) in [
            ("Home", View::Home),
            ("Favourites", View::Favourites),
            ("Albums", View::Albums),
            ("Artists", View::Artists),
            ("Genres", View::Genres),
            ("Tracks", View::Tracks),
            ("Playlists", View::Playlists),
            ("Radio", View::Radio),
            ("Sources", View::Sources),
        ] {
            let selected = self.view == view
                || matches!((&self.view, &view), (View::Album(_), View::Albums))
                || matches!((&self.view, &view), (View::Artist(_), View::Artists))
                || matches!((&self.view, &view), (View::Genre(_), View::Genres))
                || matches!((&self.view, &view), (View::Playlist(_), View::Playlists));
            if theme::nav_item(ui, selected, label).clicked() {
                if view == View::Tracks && self.tracks.is_empty() {
                    self.backend.send(Cmd::LoadTracks);
                }
                if view == View::Playlists {
                    self.backend.send(Cmd::LoadPlaylists);
                }
                if view == View::Artists {
                    self.backend.send(Cmd::LoadArtists);
                }
                if view == View::Genres {
                    self.backend.send(Cmd::LoadGenres);
                }
                if view == View::Home {
                    self.backend.send(Cmd::LoadHome);
                }
                if view == View::Favourites {
                    self.backend.send(Cmd::LoadFavourites);
                }
                if view == View::Radio {
                    self.backend.send(Cmd::LoadStations);
                }
                if view == View::Sources && self.sources.is_empty() {
                    self.backend.send(Cmd::LoadSources);
                }
                self.view = view;
            }
        }
        ui.add_space(12.0);
        theme::eyebrow(ui, "Pubky");
        ui.add_space(2.0);
        if theme::nav_item(ui, self.view == View::Discover, "Discover").clicked() {
            self.view = View::Discover;
            if self.discover.is_none() && !self.discover_loading {
                self.load_discover(DiscoverKind::Trends);
            }
        }
        ui.add_space(12.0);
        theme::eyebrow(ui, "Playing");
        ui.add_space(2.0);
        if theme::nav_item(ui, self.view == View::Queue, "Queue").clicked() {
            self.view = View::Queue;
            self.refresh_queue();
        }
        ui.add_space(12.0);
        theme::eyebrow(ui, "Node");
        ui.add_space(2.0);
        if theme::nav_item(ui, self.view == View::Settings, "Settings").clicked() {
            self.view = View::Settings;
            self.backend.send(Cmd::LoadFolders);
            self.backend.send(Cmd::LoadNodeSettings);
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            match &self.conn {
                Conn::Connected(info) => {
                    ui.small(format!("{} tracks", info.library.tracks));
                    ui.small(egui::RichText::new(format!("● {}", info.name)).color(theme::OK));
                }
                Conn::Connecting => {
                    ui.small(egui::RichText::new("○ connecting…").weak());
                }
                Conn::Failed(e) => {
                    ui.small(egui::RichText::new("● not connected").color(theme::BAD))
                        .on_hover_text(e);
                }
            }
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search artists, albums, tracks")
                    .desired_width(360.0),
            );
            let submitted = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if submitted && !self.search.trim().is_empty() {
                self.backend.send(Cmd::Search(self.search.trim().to_string()));
                self.view = View::Search;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(self.show_queue, "☰ Queue")
                    .on_hover_text("Toggle the queue panel (Q)")
                    .clicked()
                {
                    self.show_queue = !self.show_queue;
                }
                if let Some((msg, at)) = &self.status {
                    if at.elapsed() < Duration::from_secs(6) {
                        ui.label(egui::RichText::new(msg).weak());
                    }
                }
                if self.scanning {
                    ui.spinner();
                    ui.label(egui::RichText::new("scanning").weak());
                }
            });
        });
    }

    fn content(&mut self, ui: &mut egui::Ui) {
        match self.view.clone() {
            View::Home => self.home_view(ui),
            View::NowPlaying => self.now_playing_view(ui),
            View::Favourites => self.favourites_view(ui),
            View::Radio => self.radio_view(ui),
            View::Sources => self.archive_view(ui),
            View::Albums => self.albums_view(ui),
            View::Album(id) => self.album_view(ui, id),
            View::Artists => self.artists_view(ui),
            View::Artist(id) => self.artist_view(ui, id),
            View::Genres => self.genres_view(ui),
            View::Genre(id) => self.genre_view(ui, id),
            View::Discover => self.discover_view(ui),
            View::Playlists => self.playlists_view(ui),
            View::Playlist(id) => self.playlist_view(ui, &id),
            View::Tracks => {
                let tracks = self.tracks.clone();
                ui.heading("Tracks");
                self.track_table(ui, &tracks, true);
            }
            View::Search => {
                let Some((q, r)) = self.results.clone() else {
                    ui.weak("Type a query and press Enter.");
                    return;
                };
                ui.heading(format!("Results for “{q}”"));
                if r.tracks.is_empty() && r.albums.is_empty() && r.artists.is_empty() && r.genres.is_empty() {
                    ui.weak("Nothing matched. Search looks at titles, artists, albums and genres.");
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    if !r.albums.is_empty() {
                        self.album_row(ui, "Albums", &r.albums, None);
                    }
                    if !r.artists.is_empty() || !r.genres.is_empty() {
                        ui.add_space(14.0);
                        ui.horizontal_wrapped(|ui| {
                            for a in &r.artists {
                                if ui.add(theme::secondary(&a.name)).clicked() {
                                    if !self.artist_details.contains_key(&a.id) {
                                        self.backend.send(Cmd::LoadArtist(a.id));
                                    }
                                    self.view = View::Artist(a.id);
                                }
                            }
                            for g in &r.genres {
                                if ui.add(theme::secondary(&format!("#{}", g.name))).clicked() {
                                    if !self.genre_details.contains_key(&g.id) {
                                        self.backend.send(Cmd::LoadGenre(g.id));
                                    }
                                    self.view = View::Genre(g.id);
                                }
                            }
                        });
                    }
                    if !r.tracks.is_empty() {
                        ui.add_space(14.0);
                        ui.label(egui::RichText::new("Tracks").strong().size(16.0));
                        self.track_table(ui, &r.tracks, true);
                    }
                });
            }
            View::Queue => self.queue_view(ui),
            View::Settings => self.settings_view(ui),
        }
    }

    fn home_view(&mut self, ui: &mut egui::Ui) {
        let Some(home) = self.home.clone() else {
            if matches!(self.conn, Conn::Connected(_)) {
                ui.weak("Loading…");
            } else {
                ui.weak("Connect to a node from Settings to see your library here.");
            }
            return;
        };
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Home");
                ui.weak(format!(
                    "{} albums · {} tracks · {} artists",
                    home.stats.albums, home.stats.tracks, home.stats.artists
                ));
            });
            if home.stats.tracks == 0 {
                ui.add_space(8.0);
                ui.weak("The library is empty. Add folders to the node config and run a scan from Settings.");
                return;
            }
            if !home.favourite_albums.is_empty() {
                self.album_row(ui, "Favourites", &home.favourite_albums, Some(View::Favourites));
            }
            if !home.recently_added.is_empty() {
                self.album_row(ui, "Recently added", &home.recently_added, Some(View::Albums));
            }
            if !home.recently_played.is_empty() {
                self.album_row(ui, "Recently played", &home.recently_played, None);
            }
            if !home.most_played_albums.is_empty() {
                self.album_row(ui, "Most played albums", &home.most_played_albums, None);
            }
            if let Some(nr) = self.new_releases.clone() {
                if nr.artists_total > 0 {
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("New releases from your artists")
                                .strong()
                                .size(16.0),
                        );
                        ui.weak(if nr.refreshing {
                            format!("checking {} of {} artists…", nr.artists_checked, nr.artists_total)
                        } else {
                            format!(
                                "{} of {} tagged artists checked",
                                nr.artists_checked, nr.artists_total
                            )
                        });
                        if !nr.refreshing && ui.small_button("Refresh").clicked() {
                            self.backend.send(Cmd::LoadNewReleases);
                        }
                    });
                    if nr.items.is_empty() {
                        ui.weak("Nothing from the last year yet.");
                    }
                    for r in nr.items.iter().take(12) {
                        ui.horizontal(|ui| {
                            ui.label(&r.title);
                            ui.weak(format!("{} · {} · {}", r.kind, r.artist, r.first_release_date));
                            if let Some(a) = &r.in_library {
                                if let Some(id) = a.id {
                                    if ui.small_button("In library").clicked() {
                                        self.open_album(id);
                                    }
                                }
                            } else {
                                ui.hyperlink_to("MusicBrainz", &r.url);
                            }
                        });
                    }
                }
            }
            if !home.most_played.is_empty() {
                ui.add_space(14.0);
                ui.label(egui::RichText::new("Most played tracks").strong().size(16.0));
                let tracks: Vec<Track> = home.most_played.iter().map(|p| p.track.clone()).collect();
                self.track_table(ui, &tracks, true);
            }
            if !home.history.is_empty() {
                ui.add_space(14.0);
                ui.label(egui::RichText::new("History").strong().size(16.0));
                egui::Grid::new("history")
                    .num_columns(4)
                    .spacing([16.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        let mut to_play = None;
                        for p in &home.history {
                            if ui.small_button("▶").clicked() {
                                to_play = Some(p.track.clone());
                            }
                            ui.label(&p.track.title);
                            ui.weak(
                                p.track
                                    .artists
                                    .iter()
                                    .map(|a| a.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            );
                            ui.weak(format!(
                                "{} · {}",
                                self.zone_name(p.zone.as_deref().unwrap_or("")),
                                fmt_ago(p.played_at)
                            ));
                            ui.end_row();
                        }
                        if let Some(t) = to_play {
                            self.play_tracks(&[t], true, QueueAt::End);
                        }
                    });
            } else {
                ui.add_space(14.0);
                ui.weak("Nothing played yet. Plays on every zone show up here.");
            }
        });
    }

    /// One horizontally scrolling row of album tiles with a heading.
    fn album_row(&mut self, ui: &mut egui::Ui, title: &str, albums: &[Album], more: Option<View>) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).strong().size(16.0));
            if let Some(v) = more {
                if ui.small_button("All").clicked() {
                    self.view = v;
                }
            }
        });
        let mut clicked = None;
        let mut play_album: Option<i64> = None;
        egui::ScrollArea::horizontal()
            .id_salt(title)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for a in albums {
                        let tex = self.cover(a.id);
                        let (resp, play) = album_tile(ui, a, 148.0, tex.as_ref());
                        if play {
                            play_album = a.id;
                        } else if resp.clicked() {
                            clicked = a.id;
                        }
                    }
                });
            });
        if let Some(id) = clicked {
            self.open_album(id);
        }
        if let Some(id) = play_album {
            self.play_album(id);
        }
    }

    /// Roon-style full-screen view of the current track: large art, the
    /// signal path, and what comes next.
    fn now_playing_view(&mut self, ui: &mut egui::Ui) {
        let zone = self.current_zone();
        ui.horizontal(|ui| {
            if ui.button("‹ Back").clicked() {
                self.close_now_playing();
            }
            ui.weak(format!("Now playing on {}", self.zone_name(&self.settings.zone)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("Sleep timer", |ui| {
                    for m in [15u64, 30, 45, 60, 90] {
                        if ui.button(format!("{m} minutes")).clicked() {
                            self.set_sleep(Some(m * 60));
                            ui.close();
                        }
                    }
                    if ui.button("Off").clicked() {
                        self.set_sleep(None);
                        ui.close();
                    }
                });
                if self.is_local() {
                    if let Some(at) = self.local_sleep_at {
                        let left = at.saturating_duration_since(Instant::now()).as_secs();
                        ui.weak(format!("pausing in {} min", left.div_ceil(60)));
                    }
                }
            });
        });
        let Some(np) = zone.now_playing.clone() else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.weak("Nothing playing on this zone."));
            return;
        };
        let album = self.album_of_item(&np.item);
        let tex = album.as_ref().and_then(|a| self.cover(a.id));
        let station = np.item.title.clone().unwrap_or_else(|| np.item.uri.clone());
        let title = np.stream_title.clone().unwrap_or_else(|| station.clone());
        let artist_line = if np.stream_title.is_some() {
            station.clone()
        } else {
            np.item.artist.clone().unwrap_or_default()
        };
        let queue = self.queue_items();
        let up_next: Vec<QueueItem> = queue
            .iter()
            .skip_while(|i| i.id != np.item.id)
            .skip(1)
            .take(6)
            .cloned()
            .collect();
        let avail = ui.available_size();
        let art_size = (avail.y - 40.0).min(avail.x * 0.45).clamp(160.0, 560.0);
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(art_size, art_size), egui::Sense::click());
            theme::cover(ui, rect, tex.as_ref(), &title, false);
            if let Some(a) = &album {
                if resp
                    .on_hover_text("Open album")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    if let Some(id) = a.id {
                        self.open_album(id);
                    }
                }
            }
            ui.add_space(24.0);
            ui.vertical(|ui| {
                ui.add_space(art_size * 0.08);
                ui.label(egui::RichText::new(&title).strong().size(28.0));
                ui.label(egui::RichText::new(artist_line).size(18.0));
                let album_line = match (&np.item.album, album.as_ref().and_then(|a| a.year)) {
                    (Some(a), Some(y)) => format!("{a} · {y}"),
                    (Some(a), None) => a.clone(),
                    _ => String::new(),
                };
                ui.weak(album_line);
                ui.add_space(10.0);
                let (pos, dur) = (np.position_ms, np.duration_ms.unwrap_or(0));
                let mut p = pos as f64;
                let slider = egui::Slider::new(&mut p, 0.0..=dur.max(1) as f64)
                    .show_value(false)
                    .trailing_fill(true);
                let r = ui.add_sized([ui.available_width().min(520.0), 18.0], slider);
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    self.transport(TransportAction::Seek {
                        position_ms: p as u64,
                    });
                }
                ui.weak(format!("{} / {}", fmt_duration(pos), fmt_duration(dur)));
                ui.add_space(14.0);
                let headline = np.signal_path.headline();
                let (label, color) = match headline {
                    Quality::BitPerfect => ("Bit-perfect", theme::Q_BIT_PERFECT),
                    Quality::Enhanced => ("Enhanced", theme::Q_ENHANCED),
                    Quality::Lossless => ("Lossless", theme::Q_LOSSLESS),
                    Quality::Lossy => ("Lossy", theme::Q_LOSSY),
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, "●");
                    ui.label(egui::RichText::new(label).strong());
                    ui.weak(format!("{} → {}", np.source_format, np.output_format));
                });
                egui::CollapsingHeader::new("Signal path")
                    .default_open(true)
                    .show(ui, |ui| signal_path_ui(ui, &np.signal_path, &zone));
                if !up_next.is_empty() {
                    ui.add_space(14.0);
                    ui.label(egui::RichText::new("Up next").strong());
                    for it in &up_next {
                        ui.horizontal(|ui| {
                            ui.label(it.title.clone().unwrap_or_else(|| it.uri.clone()));
                            ui.weak(it.artist.clone().unwrap_or_default());
                        });
                    }
                }
                if let Some(tid) = np
                    .item
                    .uri
                    .strip_prefix("track:")
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    if !self.is_local() && self.lyrics_requested.insert(tid) {
                        self.backend.send(Cmd::LoadLyrics(tid));
                    }
                    if let Some(Some(l)) = self.lyrics.get(&tid) {
                        ui.add_space(14.0);
                        let title = if l.synced { "Lyrics · synced" } else { "Lyrics" };
                        egui::CollapsingHeader::new(title)
                            .default_open(true)
                            .show(ui, |ui| {
                                let current = l
                                    .lines
                                    .iter()
                                    .rposition(|line| line.ms.is_some_and(|ms| ms <= pos));
                                egui::ScrollArea::vertical()
                                    .id_salt("lyrics")
                                    .max_height(280.0)
                                    .show(ui, |ui| {
                                        for (i, line) in l.lines.iter().enumerate() {
                                            let on = current == Some(i);
                                            let text = if line.text.is_empty() { "♪" } else { &line.text };
                                            let r = if on {
                                                ui.label(egui::RichText::new(text).strong().size(17.0))
                                            } else {
                                                ui.weak(text)
                                            };
                                            if on && l.synced {
                                                r.scroll_to_me(Some(egui::Align::Center));
                                            }
                                        }
                                    });
                            });
                    }
                }
            });
        });
    }

    /// Play a station on the current zone: node zones through the API, the
    /// local zone through its own HTTP stream source.
    fn play_station(&mut self, st: &pm_client::RadioStation) {
        if self.is_local() {
            self.local.zone.stop();
            self.local.zone.clear();
            self.local.zone.enqueue(vec![st.queue_item()], Insert::End);
            self.local.zone.play_index(0);
        } else {
            self.backend.send(Cmd::PlayStation {
                zone: self.settings.zone.clone(),
                station: Box::new(st.clone()),
            });
        }
        self.toast(format!("Tuning to {}", st.name));
    }

    fn station_row(&mut self, ui: &mut egui::Ui, st: &pm_client::RadioStation, saved: bool) {
        ui.horizontal(|ui| {
            if theme::icon_button(ui, 30.0, "▶", "Play").clicked() {
                self.play_station(st);
            }
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(&st.name).strong());
                let mut facts = Vec::new();
                if let Some(c) = &st.country {
                    facts.push(c.clone());
                }
                if let Some(c) = &st.codec {
                    facts.push(match st.bitrate {
                        Some(b) => format!("{c} {b} kbps"),
                        None => c.clone(),
                    });
                }
                if !st.tags.is_empty() {
                    facts.push(st.tags.iter().take(4).cloned().collect::<Vec<_>>().join(", "));
                }
                ui.small(egui::RichText::new(facts.join(" · ")).weak());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if saved {
                    if ui.add(theme::secondary("Remove")).clicked() {
                        self.backend.send(Cmd::DeleteStation(st.id.clone()));
                    }
                } else if self.stations.iter().any(|x| x.url == st.url) {
                    ui.weak("saved");
                } else if ui.add(theme::secondary("Save")).clicked() {
                    self.backend.send(Cmd::SaveStation(Box::new(st.clone())));
                }
            });
        });
    }

    /// Play Internet Archive tracks: queue items with their stream URLs.
    fn play_archive(&mut self, item: &pm_api::types::SourceItem, from: usize, replace: bool) {
        let items: Vec<QueueItem> = item
            .tracks
            .iter()
            .skip(from)
            .map(|t| t.queue_item(item))
            .collect();
        if items.is_empty() {
            return;
        }
        if self.is_local() {
            if replace {
                self.local.zone.stop();
                self.local.zone.clear();
            }
            self.local.zone.enqueue(items, Insert::End);
            if replace {
                self.local.zone.play_index(0);
            }
        } else {
            self.backend.send(Cmd::EnqueueItems {
                zone: self.settings.zone.clone(),
                items,
                replace,
            });
        }
        self.toast(if replace {
            "Playing from the source"
        } else {
            "Added to queue"
        });
    }

    fn archive_view(&mut self, ui: &mut egui::Ui) {
        if let Some(item) = self.archive_item.clone() {
            ui.horizontal(|ui| {
                if ui.button("‹ Results").clicked() {
                    self.archive_item = None;
                }
            });
            ui.add_space(6.0);
            theme::eyebrow(
                ui,
                &self
                    .sources
                    .iter()
                    .find(|x| x.id == item.source)
                    .map(|x| x.name.clone())
                    .unwrap_or_else(|| item.source.clone()),
            );
            ui.heading(egui::RichText::new(&item.title).size(26.0).strong());
            let mut facts = Vec::new();
            if let Some(c) = &item.creator {
                facts.push(c.clone());
            }
            if let Some(y) = item.year {
                facts.push(y.to_string());
            }
            facts.push(format!("{} tracks", item.tracks.len()));
            ui.weak(facts.join(" · "));
            ui.horizontal(|ui| {
                if ui.add(theme::primary("▶  Play")).clicked() {
                    self.play_archive(&item, 0, true);
                }
                if ui.add(theme::secondary("Add to queue")).clicked() {
                    self.play_archive(&item, 0, false);
                }
                self.heart(
                    ui,
                    pm_model::MusicUri::parse(&item.url).ok(),
                    FavouriteRef::uri(&item.url),
                );
                let playlists = self.playlists.clone();
                ui.menu_button("Add to playlist", |ui| {
                    if playlists.is_empty() {
                        ui.weak("No playlists yet");
                    }
                    for p in &playlists {
                        if ui.button(&p.name).clicked() {
                            self.backend.send(Cmd::AddUrisToPlaylist {
                                id: p.id.clone(),
                                uris: item.tracks.iter().map(|t| t.url.clone()).collect(),
                            });
                            ui.close();
                        }
                    }
                });
                ui.hyperlink_to("open ↗", &item.url);
                if let Some(l) = &item.licence {
                    ui.hyperlink_to("licence", l);
                }
            });
            if let Some(d) = &item.description {
                ui.add(egui::Label::new(egui::RichText::new(truncate(d, 400)).color(theme::FG_2)).wrap());
            }
            ui.add_space(8.0);
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                egui::Grid::new("archive-tracks")
                    .num_columns(4)
                    .striped(true)
                    .show(ui, |ui| {
                        let mut play_from = None;
                        for (i, t) in item.tracks.iter().enumerate() {
                            if theme::icon_button(ui, 28.0, "▶", "Play from here").clicked() {
                                play_from = Some(i);
                            }
                            ui.weak(t.track_no.map(|n| n.to_string()).unwrap_or_default());
                            ui.label(&t.title);
                            ui.horizontal(|ui| {
                                ui.weak(t.duration_ms.map(fmt_duration).unwrap_or_default());
                                ui.small(egui::RichText::new(&t.format).weak());
                            });
                            ui.end_row();
                        }
                        if let Some(i) = play_from {
                            self.play_archive(&item, i, true);
                        }
                    });
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.heading("Internet Archive");
            ui.weak("live recordings, netlabels, public domain · no account");
        });
        ui.small(
            egui::RichText::new(
                "Streams play on any zone; FLAC when the item has it, else MP3 or Ogg (shown as Lossy).",
            )
            .weak(),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.archive_query)
                    .hint_text("Search this source…")
                    .desired_width(360.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add(theme::primary("Search")).clicked() || enter) && !self.archive_query.trim().is_empty()
            {
                self.archive_searching = true;
                self.backend.send(Cmd::SourceSearch {
                    source: self.source_id.clone(),
                    q: self.archive_query.trim().to_string(),
                });
            }
            if self.archive_searching || self.archive_loading.is_some() {
                ui.spinner();
            }
        });
        let Some((q, hits)) = self.archive_results.clone() else {
            ui.add_space(20.0);
            ui.weak("Search above.");
            return;
        };
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(format!("Results for “{q}”"))
                .strong()
                .size(16.0),
        );
        if hits.is_empty() {
            ui.weak("Nothing matched.");
        }
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for h in &hits {
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(egui::RichText::new(&h.title).strong()).frame(false))
                        .clicked()
                    {
                        self.archive_loading = Some(h.id.clone());
                        self.backend.send(Cmd::LoadSourceItem {
                            source: self.source_id.clone(),
                            id: h.id.clone(),
                        });
                    }
                    let mut facts = Vec::new();
                    if let Some(c) = &h.creator {
                        facts.push(c.clone());
                    }
                    if let Some(y) = h.year {
                        facts.push(y.to_string());
                    }
                    if let Some(d) = h.downloads {
                        facts.push(format!("{d} downloads"));
                    }
                    ui.weak(facts.join(" · "));
                });
            }
        });
    }

    fn radio_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Radio");
            ui.weak("internet radio via radio-browser.info");
        });
        ui.small(
            egui::RichText::new("Streams play on any zone; lossy codecs show as Lossy in the signal path.")
                .weak(),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.radio_query)
                    .hint_text("Search stations by name (jazz, FIP, BBC 6…)")
                    .desired_width(360.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add(theme::primary("Search")).clicked() || enter) && !self.radio_query.trim().is_empty() {
                self.radio_searching = true;
                self.backend
                    .send(Cmd::RadioSearch(self.radio_query.trim().to_string()));
            }
            if self.radio_searching {
                ui.spinner();
            }
        });
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            if !self.stations.is_empty() {
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Saved stations").strong().size(16.0));
                let saved = self.stations.clone();
                for st in &saved {
                    self.station_row(ui, st, true);
                }
            }
            if let Some((q, results)) = self.radio_results.clone() {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(format!("Results for “{q}”"))
                        .strong()
                        .size(16.0),
                );
                if results.is_empty() {
                    ui.weak("No stations matched.");
                }
                for st in &results {
                    self.station_row(ui, st, false);
                }
            } else if self.stations.is_empty() {
                ui.add_space(20.0);
                ui.weak("Search for a station above, or save one to see it here.");
            }
        });
    }

    fn genres_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Genres");
            ui.weak(format!("{}", self.genres.len()));
        });
        if self.genres.is_empty() {
            ui.weak("No genres yet. Genres come from the files' tags.");
            return;
        }
        let genres = self.genres.clone();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                for g in &genres {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(0.0, 0.0), egui::Sense::hover());
                    let _ = r;
                    let label = egui::RichText::new(format!("{}  ", g.name)).size(15.0).strong();
                    let sub = egui::RichText::new(format!(
                        "{} album{} · {} track{}",
                        g.albums,
                        if g.albums == 1 { "" } else { "s" },
                        g.tracks,
                        if g.tracks == 1 { "" } else { "s" }
                    ))
                    .size(12.0)
                    .color(theme::MUTED);
                    let resp = egui::Frame::new()
                        .fill(theme::SURFACE)
                        .stroke(egui::Stroke::new(1.0, theme::LINE))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(egui::Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            ui.set_min_width(180.0);
                            ui.vertical(|ui| {
                                ui.label(label);
                                ui.label(sub);
                            });
                        })
                        .response
                        .interact(egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        if !self.genre_details.contains_key(&g.id) {
                            self.backend.send(Cmd::LoadGenre(g.id));
                        }
                        self.view = View::Genre(g.id);
                    }
                }
            });
        });
    }

    fn genre_view(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.horizontal(|ui| {
            if ui.button("‹ Genres").clicked() {
                self.view = View::Genres;
            }
        });
        let Some(d) = self.genre_details.get(&id).cloned() else {
            ui.spinner();
            return;
        };
        ui.add_space(6.0);
        theme::eyebrow(ui, "Genre");
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new(&d.genre.name).size(26.0).strong());
            if ui.add(theme::primary("▶  Play all")).clicked() {
                self.play_tracks(&d.tracks, true, QueueAt::End);
            }
        });
        ui.weak(format!("{} albums · {} tracks", d.genre.albums, d.genre.tracks));
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            if !d.albums.is_empty() {
                self.album_row(ui, "Albums", &d.albums, None);
            }
            ui.add_space(14.0);
            ui.label(egui::RichText::new("Tracks").strong().size(16.0));
            self.track_table(ui, &d.tracks, true);
        });
    }

    fn favourites_view(&mut self, ui: &mut egui::Ui) {
        let Some(f) = self.favourites.clone() else {
            ui.weak("Loading…");
            return;
        };
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Favourites");
                ui.weak(format!(
                    "{} albums · {} tracks · {} artists",
                    f.albums.len(),
                    f.tracks.len(),
                    f.artists.len()
                ));
            });
            ui.small(
                egui::RichText::new(
                    "Hearts are v1 Pubky bookmarks of canonical music URLs, kept in your private homeserver folder.",
                )
                .weak(),
            );
            if f.albums.is_empty() && f.tracks.is_empty() && f.artists.is_empty() && f.other.is_empty() {
                ui.add_space(10.0);
                ui.weak("Nothing hearted yet. Use ♡ on a track, album or artist.");
                return;
            }
            if !f.albums.is_empty() {
                self.album_row(ui, "Albums", &f.albums, None);
            }
            if !f.artists.is_empty() {
                ui.add_space(14.0);
                ui.label(egui::RichText::new("Artists").strong().size(16.0));
                ui.horizontal_wrapped(|ui| {
                    for a in &f.artists {
                        if ui.button(&a.name).clicked() {
                            if !self.artist_details.contains_key(&a.id) {
                                self.backend.send(Cmd::LoadArtist(a.id));
                            }
                            self.view = View::Artist(a.id);
                        }
                    }
                });
            }
            if !f.tracks.is_empty() {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Tracks").strong().size(16.0));
                    if ui.small_button("▶ Play all").clicked() {
                        self.play_tracks(&f.tracks, true, QueueAt::End);
                    }
                });
                self.track_table(ui, &f.tracks, true);
            }
            if !f.other.is_empty() {
                ui.add_space(14.0);
                ui.label(egui::RichText::new("Not in this library").strong().size(16.0));
                for u in &f.other {
                    ui.weak(short_uri(u));
                }
            }
        });
    }

    fn album_in_focus(&self, id: Option<i64>, focus: pm_library::Focus) -> bool {
        use pm_library::Focus;
        let Some(f) = id.and_then(|id| self.album_formats.get(&id)) else {
            return false;
        };
        match focus {
            Focus::HiRes => f.hi_res,
            Focus::Dsd => f.dsd,
            Focus::Cd => f.lossless && !f.hi_res && !f.dsd,
            Focus::Lossless => f.lossless,
            Focus::Lossy => !f.lossless,
        }
    }

    fn albums_view(&mut self, ui: &mut egui::Ui) {
        use pm_library::Focus;
        let albums: Vec<Album> = match self.album_focus {
            Some(f) => self
                .albums
                .iter()
                .filter(|a| self.album_in_focus(a.id, f))
                .cloned()
                .collect(),
            None => self.albums.clone(),
        };
        ui.horizontal(|ui| {
            ui.heading("Albums");
            if self.album_focus.is_some() {
                ui.weak(format!("{} of {}", albums.len(), self.albums.len()));
            } else {
                ui.weak(format!("{}", self.albums.len()));
            }
        });
        if self.albums.is_empty() {
            ui.weak("No albums yet. Add folders to the node config and run a scan from Settings.");
            return;
        }
        if !self.album_formats.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.weak("Focus");
                if ui.selectable_label(self.album_focus.is_none(), "All").clicked() {
                    self.album_focus = None;
                }
                for (f, label) in [
                    (Focus::HiRes, "Hi-res"),
                    (Focus::Dsd, "DSD"),
                    (Focus::Cd, "CD quality"),
                    (Focus::Lossless, "Lossless"),
                    (Focus::Lossy, "Lossy"),
                ] {
                    let n = self
                        .albums
                        .iter()
                        .filter(|a| self.album_in_focus(a.id, f))
                        .count();
                    if n == 0 {
                        continue;
                    }
                    if ui
                        .selectable_label(self.album_focus == Some(f), format!("{label} {n}"))
                        .clicked()
                    {
                        self.album_focus = Some(f);
                    }
                }
            });
            ui.add_space(6.0);
        }
        if albums.is_empty() {
            ui.weak("No album has a file of that quality.");
            return;
        }
        let tile = 168.0;
        let gap = 14.0;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let cols = ((ui.available_width() + gap) / (tile + gap)).floor().max(1.0) as usize;
            let mut clicked = None;
            let mut play_album: Option<i64> = None;
            for row in albums.chunks(cols) {
                ui.horizontal(|ui| {
                    for a in row {
                        let tex = self.cover(a.id);
                        let badge =
                            a.id.and_then(|id| self.album_formats.get(&id))
                                .map(|f| f.label.clone());
                        let (resp, play) = album_tile_badged(ui, a, tile, tex.as_ref(), badge.as_deref());
                        if play {
                            play_album = a.id;
                        } else if resp.clicked() {
                            clicked = a.id;
                        }
                    }
                });
                ui.add_space(gap - 6.0);
            }
            if let Some(id) = clicked {
                self.open_album(id);
            }
            if let Some(id) = play_album {
                self.play_album(id);
            }
        });
    }

    fn album_view(&mut self, ui: &mut egui::Ui, id: i64) {
        let Some(album) = self.albums.iter().find(|a| a.id == Some(id)).cloned() else {
            ui.weak("Album not found.");
            return;
        };
        let tracks = self.album_tracks.get(&id).cloned().unwrap_or_default();
        ui.horizontal(|ui| {
            if ui.button("‹ Albums").clicked() {
                self.view = View::Albums;
            }
        });
        ui.add_space(6.0);
        let tex = self.cover(album.id);
        ui.horizontal(|ui| {
            album_art(ui, &album, 200.0, tex.as_ref());
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(12.0);
                theme::eyebrow(ui, "Album");
                ui.heading(egui::RichText::new(&album.title).size(28.0).strong());
                ui.label(
                    egui::RichText::new(
                        album
                            .album_artist
                            .clone()
                            .unwrap_or_else(|| "Unknown artist".into()),
                    )
                    .size(16.0)
                    .color(theme::FG_2),
                );
                let mut facts = Vec::new();
                if let Some(y) = album.year {
                    facts.push(y.to_string());
                }
                facts.push(format!("{} tracks", tracks.len()));
                let total: u64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
                if total > 0 {
                    facts.push(fmt_duration(total));
                }
                if let Some(f) = tracks.first() {
                    facts.push(f.file.format.to_string());
                }
                ui.weak(facts.join(" · "));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add(theme::primary("▶  Play")).clicked() {
                        self.play_tracks(&tracks, true, QueueAt::End);
                    }
                    if ui.add(theme::secondary("Play next")).clicked() {
                        self.play_tracks(&tracks, false, QueueAt::Next);
                    }
                    if ui.add(theme::secondary("Add to queue")).clicked() {
                        self.play_tracks(&tracks, false, QueueAt::End);
                    }
                    self.heart(
                        ui,
                        album
                            .release_mbid
                            .map(|mbid| pm_model::MusicUri::Release { mbid }),
                        FavouriteRef::album(id),
                    );
                });
                ui.add_space(6.0);
                self.tag_row(
                    ui,
                    album
                        .release_mbid
                        .map(|mbid| pm_model::MusicUri::Release { mbid }),
                    FavouriteRef::album(id),
                );
                ui.add_space(4.0);
                self.review_row(
                    ui,
                    album
                        .release_mbid
                        .map(|mbid| pm_model::MusicUri::Release { mbid }),
                    FavouriteRef::album(id),
                );
                self.on_pubky_row(
                    ui,
                    album
                        .release_mbid
                        .map(|mbid| pm_model::MusicUri::Release { mbid }),
                );
            });
        });
        ui.add_space(10.0);
        if tracks.is_empty() {
            ui.spinner();
        } else {
            self.track_table(ui, &tracks, false);
        }
    }

    fn track_table(&mut self, ui: &mut egui::Ui, tracks: &[Track], show_album: bool) {
        let playlists = self.playlists.clone();
        let local = self.is_local();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let mut action: Option<(usize, &str)> = None;
            let mut playlist_add: Option<(usize, usize)> = None;
            let mut fav_toggle: Option<(i64, bool)> = None;
            egui::Grid::new(("tracks", show_album))
                .num_columns(if show_album { 5 } else { 4 })
                .striped(true)
                .min_col_width(40.0)
                .show(ui, |ui| {
                    for (i, t) in tracks.iter().enumerate() {
                        let no = t.track_no.map(|n| n.to_string()).unwrap_or_default();
                        if theme::icon_button(ui, 28.0, "▶", "Play from here").clicked() {
                            action = Some((i, "play"));
                        }
                        ui.weak(no);
                        ui.vertical(|ui| {
                            ui.label(&t.title);
                            let by = t
                                .artists
                                .iter()
                                .map(|a| a.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            if !by.is_empty() {
                                ui.small(egui::RichText::new(by).weak());
                            }
                        });
                        if show_album {
                            ui.weak(t.album.as_ref().map(|a| a.title.as_str()).unwrap_or(""));
                        }
                        ui.horizontal(|ui| {
                            ui.weak(t.duration_ms.map(fmt_duration).unwrap_or_default());
                            ui.small(egui::RichText::new(t.file.format.to_string()).weak());
                            if let Some(id) = t.id {
                                let uri = t.publishable_uri().cloned();
                                let on = self.is_favourite_uri(uri.as_ref());
                                let label = if on { "♥" } else { "♡" };
                                let resp = ui.add_enabled(
                                    uri.is_some(),
                                    egui::Button::new(egui::RichText::new(label).color(if on {
                                        theme::HEART
                                    } else {
                                        ui.visuals().weak_text_color()
                                    }))
                                    .frame(false),
                                );
                                if resp
                                    .on_hover_text(if on {
                                        "Remove from favourites"
                                    } else {
                                        "Add to favourites"
                                    })
                                    .on_disabled_hover_text("No MusicBrainz id or ISRC yet")
                                    .clicked()
                                {
                                    fav_toggle = Some((id.0, !on));
                                }
                            }
                            ui.menu_button("⋯", |ui| {
                                if ui.button("Play next").clicked() {
                                    action = Some((i, "next"));
                                    ui.close();
                                }
                                if ui.button("Add to queue").clicked() {
                                    action = Some((i, "add"));
                                    ui.close();
                                }
                                if ui
                                    .button("Upload to homeserver")
                                    .on_hover_text("Copy this file to your Pubky homeserver's private storage (needs a session on the node)")
                                    .clicked()
                                {
                                    action = Some((i, "upload"));
                                    ui.close();
                                }
                                if !local
                                    && ui
                                        .button("Start radio")
                                        .on_hover_text("Play this track, then keep going with similar music from the library")
                                        .clicked()
                                {
                                    action = Some((i, "radio"));
                                    ui.close();
                                }
                                ui.separator();
                                for (pi, p) in playlists.iter().enumerate() {
                                    if ui.button(format!("Add to “{}”", p.name)).clicked() {
                                        playlist_add = Some((i, pi));
                                        ui.close();
                                    }
                                }
                                if playlists.is_empty() {
                                    ui.weak("No playlists yet");
                                }
                            });
                        });
                        ui.end_row();
                    }
                });
            if let Some((i, what)) = action {
                match what {
                    "play" => self.play_tracks(&tracks[i..], true, QueueAt::End),
                    "next" => self.play_tracks(&tracks[i..=i], false, QueueAt::Next),
                    "upload" => {
                        if let Some(id) = tracks[i].id {
                            self.backend.send(Cmd::PubkyUpload(id.0));
                        }
                    }
                    "radio" => {
                        self.play_tracks(&tracks[i..=i], true, QueueAt::End);
                        self.backend.send(Cmd::SetRadio {
                            zone: self.settings.zone.clone(),
                            on: true,
                        });
                    }
                    _ => self.play_tracks(&tracks[i..=i], false, QueueAt::End),
                }
            }
            if let Some((tid, on)) = fav_toggle {
                self.backend.send(Cmd::SetFavourite {
                    what: FavouriteRef::track(tid),
                    on,
                });
            }
            if let Some((i, pi)) = playlist_add {
                if let Some(tid) = tracks[i].id {
                    self.backend.send(Cmd::AddToPlaylist {
                        id: playlists[pi].id.clone(),
                        track_ids: vec![tid.0],
                    });
                }
            }
        });
    }

    fn artists_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Artists");
            ui.weak(format!("{}", self.artists.len()));
        });
        if self.artists.is_empty() {
            ui.weak("No artists yet.");
            return;
        }
        let artists = self.artists.clone();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("artists")
                .num_columns(3)
                .striped(true)
                .show(ui, |ui| {
                    for a in &artists {
                        if ui
                            .add(egui::Button::new(egui::RichText::new(&a.name).strong()).frame(false))
                            .clicked()
                        {
                            if !self.artist_details.contains_key(&a.id) {
                                self.backend.send(Cmd::LoadArtist(a.id));
                            }
                            self.view = View::Artist(a.id);
                        }
                        ui.weak(format!("{} albums", a.albums));
                        ui.weak(format!("{} tracks", a.tracks));
                        ui.end_row();
                    }
                });
        });
    }

    fn artist_view(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.horizontal(|ui| {
            if ui.button("‹ Artists").clicked() {
                self.view = View::Artists;
            }
        });
        let Some(d) = self.artist_details.get(&id).cloned() else {
            ui.spinner();
            return;
        };
        ui.add_space(6.0);
        let info = self.artist_infos.get(&id).cloned();
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 120.0), egui::Sense::hover());
            match info.as_ref().and_then(|(_, t)| t.clone()) {
                Some(t) => {
                    let img = egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), rect.size()))
                        .corner_radius(egui::CornerRadius::same(60));
                    ui.put(rect, img);
                }
                None => {
                    ui.painter().circle_filled(rect.center(), 60.0, theme::SURFACE_3);
                    let initials: String = d
                        .artist
                        .name
                        .split_whitespace()
                        .filter_map(|w| w.chars().next())
                        .take(2)
                        .collect::<String>()
                        .to_uppercase();
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        initials,
                        egui::FontId::proportional(36.0),
                        theme::FG_2,
                    );
                }
            }
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(14.0);
                theme::eyebrow(ui, "Artist");
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new(&d.artist.name).size(28.0).strong());
                    self.heart(
                        ui,
                        d.artist.mbid.map(|mbid| pm_model::MusicUri::Artist { mbid }),
                        FavouriteRef::artist(id),
                    );
                    if ui.add(theme::primary("▶  Play all")).clicked() {
                        self.backend.send(Cmd::LoadArtistTracks(id));
                    }
                    if !self.is_local()
                        && ui
                            .add(theme::secondary("Start radio"))
                            .on_hover_text(
                                "A random track by this artist, then similar music from the library",
                            )
                            .clicked()
                    {
                        self.backend.send(Cmd::ArtistRadio {
                            artist: id,
                            zone: self.settings.zone.clone(),
                        });
                    }
                });
                ui.weak(format!("{} albums · {} tracks", d.artist.albums, d.artist.tracks));
                if let Some((i, _)) = info.as_ref() {
                    ui.horizontal_wrapped(|ui| {
                        for (label, url) in [
                            ("Wikipedia", &i.wikipedia),
                            ("MusicBrainz", &i.musicbrainz),
                            ("Homepage", &i.homepage),
                        ] {
                            if let Some(u) = url {
                                ui.hyperlink_to(label, u);
                            }
                        }
                    });
                }
            });
        });
        if let Some((i, _)) = info.as_ref() {
            if let Some(bio) = &i.bio {
                ui.add_space(8.0);
                ui.add(egui::Label::new(egui::RichText::new(bio).color(theme::FG_2)).wrap());
            } else if let Some(n) = &i.note {
                ui.small(egui::RichText::new(n).weak());
            }
        }
        self.tag_row(
            ui,
            d.artist.mbid.map(|mbid| pm_model::MusicUri::Artist { mbid }),
            FavouriteRef::artist(id),
        );
        ui.add_space(8.0);
        let tile = 168.0;
        let gap = 14.0;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let cols = ((ui.available_width() + gap) / (tile + gap)).floor().max(1.0) as usize;
            let mut clicked = None;
            let mut play_album: Option<i64> = None;
            for row in d.albums.chunks(cols) {
                ui.horizontal(|ui| {
                    for a in row {
                        let tex = self.cover(a.id);
                        let (resp, play) = album_tile(ui, a, tile, tex.as_ref());
                        if play {
                            play_album = a.id;
                        } else if resp.clicked() {
                            clicked = a.id;
                        }
                    }
                });
                ui.add_space(gap - 6.0);
            }
            if let Some(aid) = clicked {
                if !self.albums.iter().any(|a| a.id == Some(aid)) {
                    self.albums
                        .extend(d.albums.iter().filter(|a| a.id == Some(aid)).cloned());
                }
                if !self.album_tracks.contains_key(&aid) {
                    self.backend.send(Cmd::LoadAlbumTracks(aid));
                }
                self.view = View::Album(aid);
            }
        });
    }

    fn load_discover(&mut self, kind: DiscoverKind) {
        self.discover_kind = kind.clone();
        self.discover_loading = true;
        self.backend.send(Cmd::Discover(kind));
    }

    fn discover_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Discover");
            ui.weak("what people on Pubky tag and play, via Pinakes");
        });
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.discover_kind == DiscoverKind::Trends, "Trending")
                .clicked()
            {
                self.load_discover(DiscoverKind::Trends);
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.friends_viewer)
                    .hint_text("your pubky (z32); filled in when the node is signed in")
                    .desired_width(300.0),
            );
            if ui.button("Friends").clicked() && self.friends_viewer.trim().len() == 52 {
                let v = self.friends_viewer.trim().to_string();
                self.load_discover(DiscoverKind::Friends(v));
            }
            if self.discover_loading {
                ui.spinner();
            }
        });
        let Some((kind, page)) = self.discover.clone() else {
            if !self.discover_loading {
                ui.weak("Nothing loaded. Discovery needs `contact` set in the node config.");
            }
            return;
        };
        if let DiscoverKind::Tag(t) = &kind {
            ui.label(egui::RichText::new(format!("#{t}")).strong());
        }
        if !page.facets.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for (label, count) in page.facets.iter().take(30) {
                    if ui.small_button(format!("#{label} {count}")).clicked() {
                        self.load_discover(DiscoverKind::Tag(label.clone()));
                    }
                }
            });
        }
        ui.add_space(6.0);
        if page.items.is_empty() {
            ui.weak("No music items here yet.");
            return;
        }
        let mut play: Option<Track> = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("discover")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    for it in &page.items {
                        let title = it
                            .title
                            .clone()
                            .or_else(|| it.in_library.as_ref().map(|t| t.title.clone()))
                            .unwrap_or_else(|| short_uri(&it.uri));
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(title).strong());
                            ui.small(egui::RichText::new(it.artist.clone().unwrap_or_default()).weak());
                        });
                        ui.weak(it.taggers.map(|n| format!("{n} taggers")).unwrap_or_default());
                        match &it.in_library {
                            Some(t) => {
                                if ui.button("▶ in library").clicked() {
                                    play = Some(t.clone());
                                }
                            }
                            None => {
                                ui.weak("not in library");
                            }
                        }
                        if ui.small_button("open ↗").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(&it.uri));
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some(t) = play {
            self.play_tracks(&[t], true, QueueAt::End);
        }
    }

    fn playlists_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Playlists");
            ui.weak(format!("{}", self.playlists.len()));
        });
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.new_playlist_name)
                    .hint_text("New playlist name")
                    .desired_width(260.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Create").clicked() || enter) && !self.new_playlist_name.trim().is_empty() {
                self.backend.send(Cmd::CreatePlaylist {
                    name: self.new_playlist_name.trim().to_string(),
                    track_ids: vec![],
                });
                self.new_playlist_name.clear();
            }
        });
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.import_path)
                    .hint_text("Import: path to an .m3u/.m3u8, .csv or Roon .xlsx export")
                    .desired_width(420.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Import").clicked() || enter) && !self.import_path.trim().is_empty() {
                let path = PathBuf::from(shellexpand_home(self.import_path.trim()));
                self.backend.send(Cmd::ImportPlaylistFile(path));
                self.import_path.clear();
            }
        });
        ui.add_space(6.0);
        if self.playlists.is_empty() {
            ui.weak("No playlists yet. Create one above, import a file, or use ⋯ on any track to add it.");
            return;
        }
        let lists = self.playlists.clone();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("playlists")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    for p in &lists {
                        if ui
                            .add(egui::Button::new(egui::RichText::new(&p.name).strong()).frame(false))
                            .clicked()
                        {
                            self.backend.send(Cmd::LoadPlaylist(p.id.clone()));
                            self.view = View::Playlist(p.id.clone());
                        }
                        ui.weak(p.description.clone().unwrap_or_default());
                        ui.weak(format!("{} items", p.items));
                        ui.horizontal(|ui| {
                            ui.weak(if p.synced { "synced" } else { "local" });
                            if ui.button("▶").on_hover_text("Play").clicked() {
                                self.play_playlist(&p.id);
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    }

    fn play_playlist(&mut self, id: &str) {
        if self.is_local() {
            // Local zone: resolve on the client from the detail's tracks.
            if let Some(d) = self.playlist_details.get(id) {
                let tracks: Vec<Track> = d.tracks.iter().flatten().cloned().collect();
                self.play_tracks(&tracks, true, QueueAt::End);
            } else {
                self.backend.send(Cmd::LoadPlaylist(id.to_string()));
                self.toast("loading playlist, press play again");
            }
        } else {
            self.backend.send(Cmd::PlayPlaylist {
                id: id.to_string(),
                zone: self.settings.zone.clone(),
            });
        }
    }

    fn playlist_view(&mut self, ui: &mut egui::Ui, id: &str) {
        ui.horizontal(|ui| {
            if ui.button("‹ Playlists").clicked() {
                self.view = View::Playlists;
            }
        });
        let Some(d) = self.playlist_details.get(id).cloned() else {
            ui.spinner();
            return;
        };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            art_square(ui, &d.summary.name, 120.0);
            ui.vertical(|ui| {
                ui.heading(egui::RichText::new(&d.summary.name).size(26.0).strong());
                if let Some(desc) = &d.summary.description {
                    ui.label(desc);
                }
                let in_library = d.tracks.iter().filter(|t| t.is_some()).count();
                ui.weak(format!(
                    "{} items · {} in this library · {} · {}",
                    d.playlist.items.len(),
                    in_library,
                    d.summary.visibility,
                    if d.summary.synced {
                        "synced to homeserver"
                    } else {
                        "local only"
                    }
                ));
                ui.horizontal(|ui| {
                    if ui.button("▶  Play").clicked() {
                        self.play_playlist(id);
                    }
                    if ui.button("Delete").clicked() {
                        self.backend.send(Cmd::DeletePlaylist(id.to_string()));
                    }
                });
            });
        });
        ui.add_space(10.0);
        let mut remove = None;
        let mut moved: Option<(usize, usize)> = None;
        let n_items = d.playlist.items.len();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("playlist-items")
                .num_columns(5)
                .striped(true)
                .show(ui, |ui| {
                    for (i, item) in d.playlist.items.iter().enumerate() {
                        ui.weak(format!("{}", i + 1));
                        match &d.tracks[i] {
                            Some(t) => {
                                ui.vertical(|ui| {
                                    ui.label(&t.title);
                                    let by = t
                                        .artists
                                        .iter()
                                        .map(|a| a.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    ui.small(egui::RichText::new(by).weak());
                                });
                                ui.weak(t.duration_ms.map(fmt_duration).unwrap_or_default());
                            }
                            None => {
                                ui.vertical(|ui| {
                                    ui.label(egui::RichText::new(short_uri(&item.uri)).weak());
                                    ui.small(egui::RichText::new("not in this library").weak());
                                });
                                ui.weak("");
                            }
                        }
                        ui.horizontal(|ui| {
                            if ui.add_enabled(i > 0, egui::Button::new("↑").small()).clicked() {
                                moved = Some((i, i - 1));
                            }
                            if ui
                                .add_enabled(i + 1 < n_items, egui::Button::new("↓").small())
                                .clicked()
                            {
                                moved = Some((i, i + 1));
                            }
                        });
                        if ui.small_button("✕").clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some((from, to)) = moved {
            let mut uris: Vec<String> = d.playlist.items.iter().map(|it| it.uri.clone()).collect();
            let u = uris.remove(from);
            uris.insert(to, u);
            self.backend.send(Cmd::MovePlaylistItem {
                id: id.to_string(),
                uris,
            });
        }
        if let Some(i) = remove {
            self.backend.send(Cmd::RemovePlaylistItem {
                id: id.to_string(),
                index: i,
            });
        }
    }

    /// Sleep timer: the node keeps it for its zones; the local player is timed here.
    fn set_sleep(&mut self, seconds: Option<u64>) {
        if self.is_local() {
            self.local_sleep_at = seconds.map(|s| Instant::now() + Duration::from_secs(s));
        } else {
            self.backend.send(Cmd::SetSleep {
                zone: self.settings.zone.clone(),
                seconds,
            });
        }
    }

    fn queue_view(&mut self, ui: &mut egui::Ui) {
        let zone = self.current_zone();
        let items = self.queue_items();
        ui.horizontal(|ui| {
            ui.heading(format!("Queue · {}", self.zone_name(&self.settings.zone)));
            if ui.button("Clear").clicked() {
                if self.is_local() {
                    self.local.zone.stop();
                    self.local.zone.clear();
                } else {
                    self.backend.send(Cmd::ClearQueue(self.settings.zone.clone()));
                }
            }
            if ui
                .add_enabled(items.len() > 1, egui::Button::new("Shuffle"))
                .on_hover_text("Shuffle what comes next; the playing track stays")
                .clicked()
            {
                if self.is_local() {
                    self.local.zone.shuffle_upcoming();
                } else {
                    self.backend.send(Cmd::ShuffleQueue(self.settings.zone.clone()));
                }
            }
            if !self.is_local() && !items.is_empty() && self.zones.len() > 1 {
                let here = self.settings.zone.clone();
                let others: Vec<(String, String)> = self
                    .zones
                    .iter()
                    .filter(|(id, _)| **id != here)
                    .map(|(id, z)| (id.clone(), z.name.clone()))
                    .collect();
                ui.menu_button("Transfer to…", |ui| {
                    for (id, name) in others {
                        if ui.button(name).clicked() {
                            self.backend.send(Cmd::Transfer {
                                zone: here.clone(),
                                to: id.clone(),
                            });
                            self.settings.zone = id;
                            self.refresh_queue();
                            ui.close();
                        }
                    }
                });
            }
            if ui
                .add_enabled(!items.is_empty(), egui::Button::new("Save as playlist"))
                .on_hover_text("A new private playlist from this queue")
                .clicked()
            {
                let cmd = if self.is_local() {
                    Cmd::SaveQueueAsPlaylist {
                        zone: None,
                        uris: items.iter().map(|i| i.uri.clone()).collect(),
                    }
                } else {
                    Cmd::SaveQueueAsPlaylist {
                        zone: Some(self.settings.zone.clone()),
                        uris: Vec::new(),
                    }
                };
                self.backend.send(cmd);
            }
            let mut repeat = zone.repeat;
            egui::ComboBox::from_id_salt("repeat")
                .selected_text(match repeat {
                    pm_zones::Repeat::Off => "Repeat off",
                    pm_zones::Repeat::All => "Repeat all",
                    pm_zones::Repeat::One => "Repeat one",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut repeat, pm_zones::Repeat::Off, "Repeat off");
                    ui.selectable_value(&mut repeat, pm_zones::Repeat::All, "Repeat all");
                    ui.selectable_value(&mut repeat, pm_zones::Repeat::One, "Repeat one");
                });
            if repeat != zone.repeat {
                if self.is_local() {
                    self.local.zone.set_repeat(repeat);
                } else {
                    self.backend.send(Cmd::Repeat {
                        zone: self.settings.zone.clone(),
                        repeat,
                    });
                }
            }
            if !self.is_local() {
                let mut radio = zone.radio;
                if ui
                    .toggle_value(&mut radio, "Radio")
                    .on_hover_text("When the queue ends, keep playing similar music from the library")
                    .changed()
                {
                    self.backend.send(Cmd::SetRadio {
                        zone: self.settings.zone.clone(),
                        on: radio,
                    });
                }
            }
        });
        if items.is_empty() {
            ui.weak("Nothing queued.");
            return;
        }
        let mut play_index = None;
        let mut remove = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("queue")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    for (i, it) in items.iter().enumerate() {
                        let current = zone.queue_index == Some(i);
                        if ui
                            .add(egui::Button::new(if current { "▶" } else { " " }).frame(false))
                            .clicked()
                        {
                            play_index = Some(i);
                        }
                        ui.label(if current {
                            egui::RichText::new(it.title.clone().unwrap_or_else(|| it.uri.clone())).strong()
                        } else {
                            egui::RichText::new(it.title.clone().unwrap_or_else(|| it.uri.clone()))
                        });
                        ui.weak(it.artist.clone().unwrap_or_default());
                        ui.horizontal(|ui| {
                            ui.weak(it.duration_ms.map(fmt_duration).unwrap_or_default());
                            if ui.small_button("✕").clicked() {
                                remove = Some(it.id);
                            }
                        });
                        ui.end_row();
                    }
                });
        });
        if let Some(i) = play_index {
            self.transport(TransportAction::Play { index: Some(i) });
        }
        if let Some(id) = remove {
            if self.is_local() {
                self.local.zone.remove(id);
            } else {
                self.backend.send(Cmd::RemoveItem {
                    zone: self.settings.zone.clone(),
                    item: id,
                });
            }
        }
    }

    /// Compact queue for the selected zone: current item highlighted, click to
    /// jump, ✕ to remove.
    fn queue_panel(&mut self, ui: &mut egui::Ui) {
        let zone = self.current_zone();
        let items = self.queue_items();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Queue").strong());
            ui.weak(self.zone_name(&self.settings.zone));
        });
        ui.separator();
        if items.is_empty() {
            ui.weak("Empty. Play an album or add tracks with ⋯.");
            return;
        }
        let mut play_index = None;
        let mut remove = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (i, it) in items.iter().enumerate() {
                let current = zone.queue_index == Some(i);
                let title = it.title.clone().unwrap_or_else(|| it.uri.clone());
                ui.horizontal(|ui| {
                    let label = if current {
                        egui::RichText::new(format!("▶ {title}")).strong()
                    } else {
                        egui::RichText::new(format!("{}. {title}", i + 1))
                    };
                    if ui
                        .add(egui::Button::new(label).frame(false).truncate())
                        .on_hover_text(it.artist.clone().unwrap_or_default())
                        .clicked()
                    {
                        play_index = Some(i);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").clicked() {
                            remove = Some(it.id);
                        }
                        ui.weak(it.duration_ms.map(fmt_duration).unwrap_or_default());
                    });
                });
            }
        });
        if let Some(i) = play_index {
            self.transport(TransportAction::Play { index: Some(i) });
        }
        if let Some(id) = remove {
            if self.is_local() {
                self.local.zone.remove(id);
            } else {
                self.backend.send(Cmd::RemoveItem {
                    zone: self.settings.zone.clone(),
                    item: id,
                });
            }
        }
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("Node");
        egui::Grid::new("settings")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("URL");
                ui.add(egui::TextEdit::singleline(&mut self.settings.node_url).desired_width(360.0));
                ui.end_row();
                ui.label("Token");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.token)
                        .password(true)
                        .desired_width(360.0),
                );
                ui.end_row();
                ui.label("Pubky session");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.session_secret)
                        .password(true)
                        .hint_text("exported session secret; finds your node through your homeserver")
                        .desired_width(360.0),
                );
                ui.end_row();
            });
        ui.horizontal(|ui| {
            if ui.button("Connect").clicked() {
                self.connect();
            }
            let has_secret = !self.settings.session_secret.trim().is_empty();
            if ui
                .add_enabled(has_secret, egui::Button::new("Find my node via Pubky"))
                .on_hover_text("Reads /priv/pubky.music/node.json from your homeserver")
                .clicked()
            {
                self.backend.send(Cmd::ConnectViaPubky {
                    secret: self.settings.session_secret.trim().to_string(),
                });
            }
            if ui.button("Scan library").clicked() {
                self.backend.send(Cmd::Scan);
            }
            if ui.button("Refresh").clicked() {
                self.backend.send(Cmd::Refresh);
            }
            if ui
                .button("Sync with Pubky")
                .on_hover_text("Playlists, favourites, tags and reviews; needs a session on the node")
                .clicked()
            {
                self.backend.send(Cmd::PubkySync);
            }
            if ui
                .button("Sign in node with Pubky Ring")
                .on_hover_text("Starts a sign-in on the node and shows the QR to scan")
                .clicked()
            {
                self.backend.send(Cmd::PubkyLoginStart);
            }
        });
        if let Conn::Failed(e) = &self.conn {
            ui.colored_label(theme::BAD, e);
        }
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Homeserver files").strong().size(16.0));
        ui.small(
            egui::RichText::new(
                "Audio you uploaded to your Pubky homeserver's private storage (never under /pub/). Upload from a track's ⋯ menu.",
            )
            .weak(),
        );
        ui.horizontal(|ui| {
            if ui.button("List files").clicked() {
                self.backend.send(Cmd::PubkyFiles);
            }
            if ui
                .button("Mirror to this node")
                .on_hover_text("Download what this node does not have yet, then scan it")
                .clicked()
            {
                self.backend.send(Cmd::PubkyMirror);
            }
        });
        match &self.pubky_files {
            None => {}
            Some(Err(e)) => {
                ui.weak(format!("Not available: {e}"));
            }
            Some(Ok(files)) if files.is_empty() => {
                ui.weak("No private files on the homeserver yet.");
            }
            Some(Ok(files)) => {
                let total: u64 = files.iter().filter_map(|f| f.size).sum();
                ui.weak(format!("{} files · {:.1} MB", files.len(), total as f64 / 1e6));
                egui::ScrollArea::vertical()
                    .id_salt("pubky-files")
                    .max_height(200.0)
                    .show(ui, |ui| {
                        for f in files {
                            ui.horizontal(|ui| {
                                ui.label(f.name());
                                if let Some(sz) = f.size {
                                    ui.weak(format!("{:.1} MB", sz as f64 / 1e6));
                                }
                            });
                        }
                    });
            }
        }
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Upstream contact").strong().size(16.0));
        ui.small(
            egui::RichText::new(
                "An email or URL sent in the node's User-Agent; Pinakes (Discover, On Pubky), MusicBrainz and Wikipedia ask for one.",
            )
            .weak(),
        );
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.contact_input)
                    .hint_text("you@example.org or https://your.site")
                    .desired_width(300.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add(theme::primary("Save")).clicked() || enter) && !self.contact_input.trim().is_empty() {
                self.backend
                    .send(Cmd::SetContact(self.contact_input.trim().to_string()));
            }
            if let Some(v) = &self.node_settings {
                theme::pill(
                    ui,
                    Some(if v.discovery { theme::OK } else { theme::WARN }),
                    if v.discovery {
                        "Discover on"
                    } else {
                        "Discover off"
                    },
                );
            }
        });
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Library folders").strong().size(16.0));
        ui.small(
            egui::RichText::new(
                "Paths on the node. New folders are watched and scanned; removing one forgets its tracks.",
            )
            .weak(),
        );
        if self.folders.is_empty() {
            ui.weak("No folders yet.");
        }
        let folders = self.folders.clone();
        for (path, present) in &folders {
            ui.horizontal(|ui| {
                ui.label(path);
                if !present {
                    theme::pill(ui, Some(theme::WARN), "missing");
                }
                if ui.add(theme::secondary("Remove")).clicked() {
                    self.backend.send(Cmd::RemoveFolder(path.clone()));
                }
            });
        }
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.folder_input)
                    .hint_text("/path/on/the/node/Music")
                    .desired_width(360.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add(theme::primary("Add folder")).clicked() || enter)
                && !self.folder_input.trim().is_empty()
            {
                self.backend
                    .send(Cmd::AddFolder(self.folder_input.trim().to_string()));
                self.folder_input.clear();
            }
        });
        ui.add_space(12.0);
        ui.heading("Zones");
        ui.label(format!("This computer plays through: {}", self.local.output_name));
        let mut delete = None;
        egui::Grid::new("zones-table")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for z in self.zones.values() {
                    ui.label(&z.name);
                    ui.weak(z.output.to_string());
                    if ui.small_button("remove").clicked() {
                        delete = Some(z.id.0.clone());
                    }
                    ui.end_row();
                }
            });
        if let Some(id) = delete {
            self.backend.send(Cmd::DeleteZone(id));
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new("New zone or group").strong());
        ui.weak("Pick one output for a zone, several for a group (the first picked leads).");
        let outputs = self.outputs.clone();
        for o in &outputs {
            let id = o.id.0.clone();
            let mut on = self.new_zone_outputs.contains(&id);
            if ui.checkbox(&mut on, format!("{}  ({})", o.name, id)).changed() {
                if on {
                    self.new_zone_outputs.push(id);
                } else {
                    self.new_zone_outputs.retain(|x| *x != id);
                }
            }
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_zone_name)
                    .hint_text("Zone name")
                    .desired_width(220.0),
            );
            let ready = !self.new_zone_name.trim().is_empty() && !self.new_zone_outputs.is_empty();
            if ui.add_enabled(ready, egui::Button::new("Create")).clicked() {
                self.backend.send(Cmd::CreateZone {
                    name: self.new_zone_name.trim().to_string(),
                    outputs: self.new_zone_outputs.clone(),
                });
                self.new_zone_name.clear();
                self.new_zone_outputs.clear();
            }
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.heading("Node outputs");
            if ui
                .button("Rescan")
                .on_hover_text("Find endpoints started after the node")
                .clicked()
            {
                self.backend.send(Cmd::RescanOutputs);
            }
        });
        for o in &self.outputs {
            let caps = &o.caps;
            let rates = caps
                .pcm_rates
                .iter()
                .map(|r| format!("{:.1}k", *r as f64 / 1000.0))
                .collect::<Vec<_>>()
                .join(" ");
            let dsd = match caps.dsd {
                Some(pm_model::DsdSupport::Native { max_rate }) => {
                    format!("DSD native to {}", dsd_name(max_rate))
                }
                Some(pm_model::DsdSupport::DoP { max_rate }) => format!("DoP to {}", dsd_name(max_rate)),
                None => "no DSD".into(),
            };
            ui.label(format!(
                "{}  ·  {} bit  ·  {rates}  ·  {dsd}",
                o.name, caps.max_pcm_bits
            ));
        }
        ui.add_space(12.0);
        ui.heading("Cache");
        ui.weak(format!(
            "Tracks played on this computer are cached in {}",
            cache_dir().display()
        ));
    }

    fn transport_bar(&mut self, ui: &mut egui::Ui) {
        let zone = self.current_zone();
        let np = zone.now_playing.clone();
        let width = ui.available_width();
        ui.horizontal(|ui| {
            // Now playing (left third).
            ui.allocate_ui_with_layout(
                egui::vec2(width * 0.32, 72.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    if let Some(np) = &np {
                        let album = self.album_of_item(&np.item);
                        let tex = album.as_ref().and_then(|a| self.cover(a.id));
                        let station = np.item.title.clone().unwrap_or_else(|| np.item.uri.clone());
                        let title = np.stream_title.clone().unwrap_or_else(|| station.clone());
                        let sub = if np.stream_title.is_some() {
                            station.clone()
                        } else {
                            np.item.artist.clone().unwrap_or_default()
                        };
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::click());
                        theme::cover(ui, rect, tex.as_ref(), &title, resp.hovered());
                        let open = resp
                            .on_hover_text("Now playing")
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked();
                        let open = ui
                            .vertical(|ui| {
                                let r = ui.add(
                                    egui::Label::new(egui::RichText::new(&title).strong())
                                        .sense(egui::Sense::click()),
                                );
                                ui.weak(sub);
                                ui.small(
                                    egui::RichText::new(np.item.album.clone().unwrap_or_default()).weak(),
                                );
                                r.clicked()
                            })
                            .inner
                            || open;
                        if open {
                            if self.view == View::NowPlaying {
                                self.close_now_playing();
                            } else {
                                self.open_now_playing();
                            }
                        }
                    } else {
                        ui.weak("Nothing playing");
                    }
                },
            );
            // Transport (middle).
            ui.allocate_ui_with_layout(
                egui::vec2(width * 0.36, 72.0),
                egui::Layout::top_down(egui::Align::Center),
                |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 136.0).max(0.0) / 2.0);
                        if theme::icon_button(ui, 36.0, "⏮", "Previous").clicked() {
                            self.transport(TransportAction::Previous);
                        }
                        let playing = zone.transport == Transport::Playing;
                        if theme::play_button(ui, 44.0, playing).clicked() {
                            self.transport(TransportAction::Toggle);
                        }
                        if theme::icon_button(ui, 36.0, "⏭", "Next").clicked() {
                            self.transport(TransportAction::Next);
                        }
                    });
                    let (pos, dur) = np
                        .as_ref()
                        .map(|n| (n.position_ms, n.duration_ms.unwrap_or(0)))
                        .unwrap_or((0, 0));
                    ui.horizontal(|ui| {
                        ui.weak(fmt_duration(pos));
                        let mut p = pos as f64;
                        let slider = egui::Slider::new(&mut p, 0.0..=dur.max(1) as f64)
                            .show_value(false)
                            .trailing_fill(true);
                        let r = ui.add_sized([ui.available_width() - 48.0, 18.0], slider);
                        if r.drag_stopped() || (r.changed() && !r.dragged()) {
                            self.transport(TransportAction::Seek {
                                position_ms: p as u64,
                            });
                        }
                        ui.weak(fmt_duration(dur));
                    });
                },
            );
            // Zone, volume, signal path (right).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let headline = np.as_ref().map(|n| n.signal_path.headline());
                let (label, color) = match headline {
                    Some(q) => (theme::quality_label(q), theme::quality_color(q)),
                    None => ("—", theme::MUTED),
                };
                if theme::pill(ui, Some(color), label)
                    .on_hover_text("Signal path")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.show_path = !self.show_path;
                }
                if ui
                    .add(egui::Button::new("DSP").frame(false))
                    .on_hover_text("EQ, headroom, volume for this zone")
                    .clicked()
                {
                    self.open_dsp();
                }
                if let Some(n) = np.as_ref().map(|n| &n.members).filter(|m| m.len() > 1) {
                    let worst = n.iter().map(|m| m.drift_ms.abs()).max().unwrap_or(0);
                    let slips: i64 = n.iter().map(|m| m.slipped_frames.abs()).sum();
                    ui.weak(format!(
                        "group · {} outputs · drift ≤ {worst} ms · {slips} frames slipped",
                        n.len()
                    ));
                }
                ui.add_space(8.0);
                let mut db = self.volume_db;
                if ui
                    .add(
                        egui::Slider::new(&mut db, -60.0..=0.0)
                            .show_value(false)
                            .suffix(" dB"),
                    )
                    .drag_stopped()
                {
                    self.set_volume(db);
                }
                ui.weak(format!("{db:+.0} dB"));
                ui.add_space(8.0);
                let mut ids: Vec<String> = vec![LOCAL_ZONE_ID.to_string()];
                ids.extend(self.zones.keys().cloned());
                let mut chosen = self.settings.zone.clone();
                egui::ComboBox::from_id_salt("zone")
                    .selected_text(self.zone_name(&chosen))
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for id in &ids {
                            ui.selectable_value(&mut chosen, id.clone(), self.zone_name(id));
                        }
                    });
                if chosen != self.settings.zone {
                    self.settings.zone = chosen;
                    self.refresh_queue();
                }
            });
        });
    }

    fn open_dsp(&mut self) {
        self.show_dsp = true;
        let zone = self.settings.zone.clone();
        if self.is_local() {
            self.dsp_edit = Some((zone, self.local.dsp()));
        } else {
            self.dsp_edit = None;
            self.backend.send(Cmd::LoadDsp(zone));
        }
    }

    fn dsp_window(&mut self, ctx: &egui::Context) {
        if !self.show_dsp {
            return;
        }
        let mut open = self.show_dsp;
        let zone_name = self.zone_name(&self.settings.zone);
        let mut apply = false;
        let mut reset = false;
        egui::Window::new(format!("DSP · {zone_name}"))
            .open(&mut open)
            .resizable(true)
            .default_width(520.0)
            .show(ctx, |ui| {
                let Some((_, cfg)) = self.dsp_edit.as_mut() else {
                    ui.spinner();
                    return;
                };
                ui.weak("Changes apply from the next track. Every stage shows in the signal path.");
                ui.add_space(6.0);
                egui::Grid::new("dsp-gains")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Headroom");
                        let mut h = cfg.headroom_db.unwrap_or(0.0);
                        if ui
                            .add(egui::Slider::new(&mut h, -12.0..=0.0).suffix(" dB"))
                            .changed()
                        {
                            cfg.headroom_db = (h != 0.0).then_some(h);
                        }
                        ui.end_row();
                        ui.label("Software volume");
                        let mut v = cfg.volume_db.unwrap_or(0.0);
                        if ui
                            .add(egui::Slider::new(&mut v, -60.0..=0.0).suffix(" dB"))
                            .changed()
                        {
                            cfg.volume_db = (v != 0.0).then_some(v);
                        }
                        ui.end_row();
                        ui.label("Volume limit")
                            .on_hover_text("Every volume request for this zone is capped here");
                        let mut m = cfg.max_volume_db.unwrap_or(0.0);
                        if ui
                            .add(egui::Slider::new(&mut m, -60.0..=0.0).suffix(" dB"))
                            .changed()
                        {
                            cfg.max_volume_db = (m != 0.0).then_some(m);
                        }
                        ui.end_row();
                        ui.label("Loudness (ReplayGain)");
                        egui::ComboBox::from_id_salt("loudness")
                            .selected_text(format!("{:?}", cfg.loudness))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut cfg.loudness, pm_audio::Loudness::Off, "Off");
                                ui.selectable_value(&mut cfg.loudness, pm_audio::Loudness::Track, "Track");
                                ui.selectable_value(&mut cfg.loudness, pm_audio::Loudness::Album, "Album");
                            });
                        ui.end_row();
                        ui.label("Room correction IR");
                        ui.horizontal(|ui| {
                            let mut conv = cfg.convolution.clone().unwrap_or(pm_audio::ConvolutionConfig {
                                path: String::new(),
                                gain_db: 0.0,
                                enabled: true,
                            });
                            let mut changed = false;
                            changed |= ui.checkbox(&mut conv.enabled, "").changed();
                            changed |= ui
                                .add(
                                    egui::TextEdit::singleline(&mut conv.path)
                                        .hint_text("path to a WAV impulse response on the node")
                                        .desired_width(260.0),
                                )
                                .changed();
                            changed |= ui
                                .add(
                                    egui::DragValue::new(&mut conv.gain_db)
                                        .range(-24.0..=12.0)
                                        .speed(0.1)
                                        .suffix(" dB"),
                                )
                                .changed();
                            if changed {
                                cfg.convolution = (!conv.path.trim().is_empty()).then_some(conv);
                            }
                        });
                        ui.end_row();
                        ui.label("Headphone crossfeed");
                        ui.horizontal(|ui| {
                            let mut on = cfg.crossfeed.as_ref().is_some_and(|c| c.enabled);
                            if ui.checkbox(&mut on, "").changed() {
                                cfg.crossfeed = on.then(pm_audio::CrossfeedConfig::default);
                            }
                            if let Some(cf) = cfg.crossfeed.as_mut() {
                                ui.add(
                                    egui::Slider::new(&mut cf.level_db, -15.0..=-2.0)
                                        .suffix(" dB")
                                        .text("level"),
                                );
                                ui.add(
                                    egui::DragValue::new(&mut cf.cutoff_hz)
                                        .range(200.0..=2000.0)
                                        .speed(5.0)
                                        .suffix(" Hz"),
                                );
                            }
                        });
                        ui.end_row();
                        ui.label("Resample when needed");
                        ui.checkbox(&mut cfg.resample, "");
                        ui.end_row();
                        ui.label("Dither on bit reduction");
                        ui.checkbox(&mut cfg.dither, "");
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Parametric EQ").strong());
                    if ui.small_button("+ band").clicked() {
                        cfg.eq.push(pm_audio::EqBand::peaking(1000.0, 0.0, 1.0));
                    }
                });
                let mut remove = None;
                egui::Grid::new("dsp-eq")
                    .num_columns(6)
                    .striped(true)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        ui.weak("on");
                        ui.weak("type");
                        ui.weak("Hz");
                        ui.weak("dB");
                        ui.weak("Q");
                        ui.weak("");
                        ui.end_row();
                        for (i, b) in cfg.eq.iter_mut().enumerate() {
                            ui.checkbox(&mut b.enabled, "");
                            egui::ComboBox::from_id_salt(("kind", i))
                                .selected_text(format!("{:?}", b.kind))
                                .width(110.0)
                                .show_ui(ui, |ui| {
                                    for k in [
                                        pm_audio::BandKind::Peaking,
                                        pm_audio::BandKind::LowShelf,
                                        pm_audio::BandKind::HighShelf,
                                        pm_audio::BandKind::LowPass,
                                        pm_audio::BandKind::HighPass,
                                    ] {
                                        ui.selectable_value(&mut b.kind, k, format!("{k:?}"));
                                    }
                                });
                            ui.add(
                                egui::DragValue::new(&mut b.freq_hz)
                                    .range(10.0..=20_000.0)
                                    .speed(5.0)
                                    .max_decimals(0),
                            );
                            ui.add(
                                egui::DragValue::new(&mut b.gain_db)
                                    .range(-24.0..=24.0)
                                    .speed(0.1)
                                    .max_decimals(1),
                            );
                            ui.add(
                                egui::DragValue::new(&mut b.q)
                                    .range(0.1..=20.0)
                                    .speed(0.02)
                                    .max_decimals(2),
                            );
                            if ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                            ui.end_row();
                        }
                    });
                if let Some(i) = remove {
                    cfg.eq.remove(i);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Reset to default").clicked() {
                        reset = true;
                    }
                });
            });
        self.show_dsp = open;
        if apply {
            if let Some((zone, cfg)) = self.dsp_edit.clone() {
                if self.is_local() {
                    self.local.set_dsp(cfg);
                    self.toast("DSP saved; applies from the next track");
                } else {
                    self.backend.send(Cmd::SetDsp(zone, Box::new(cfg)));
                }
            }
        }
        if reset {
            if self.is_local() {
                self.local.set_dsp(pm_audio::PipelineConfig::default());
                self.dsp_edit = Some((self.settings.zone.clone(), pm_audio::PipelineConfig::default()));
            } else {
                self.backend.send(Cmd::ResetDsp(self.settings.zone.clone()));
            }
        }
    }

    fn login_window(&mut self, ctx: &egui::Context) {
        let Some((url, tex, last_poll)) = self.login.as_mut() else {
            return;
        };
        if last_poll.elapsed() >= Duration::from_secs(2) {
            *last_poll = std::time::Instant::now();
            self.backend.send(Cmd::PubkyLoginPoll);
        }
        if self.local_sleep_at.is_some_and(|at| Instant::now() >= at) {
            self.local_sleep_at = None;
            self.local.zone.pause();
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        let mut open = true;
        let url_text = url.clone();
        let tex = tex.clone();
        egui::Window::new("Sign in with Pubky Ring")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label("Scan with Pubky Ring, or paste the URL there. Waiting for approval…");
                if let Some(t) = &tex {
                    ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(260.0, 260.0)));
                }
                ui.add(egui::Label::new(egui::RichText::new(&url_text).small().weak()).wrap());
                if ui.button("Copy URL").clicked() {
                    ui.ctx().copy_text(url_text.clone());
                }
            });
        if !open {
            self.login = None;
        }
    }

    fn signal_path_window(&mut self, ctx: &egui::Context) {
        if !self.show_path {
            return;
        }
        let zone = self.current_zone();
        let mut open = self.show_path;
        egui::Window::new("Signal path")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                match zone.now_playing.as_ref().map(|n| &n.signal_path) {
                    Some(path) => signal_path_ui(ui, path, &zone),
                    None => {
                        ui.weak("Nothing playing in this zone.");
                    }
                }
            });
        self.show_path = open;
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "settings", &self.settings);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_backend();
        self.report_local_play();
        let zone = self.current_zone();
        if zone.transport == Transport::Playing {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        // Keyboard: space play/pause, ← → previous/next, Q toggles the queue panel.
        if !ui.ctx().egui_wants_keyboard_input() {
            let (space, right, left, q) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::Space),
                    i.key_pressed(egui::Key::ArrowRight),
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::Q),
                )
            });
            if space {
                self.transport(TransportAction::Toggle);
            }
            if right {
                self.transport(TransportAction::Next);
            }
            if left {
                self.transport(TransportAction::Previous);
            }
            if q {
                self.show_queue = !self.show_queue;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) && self.view == View::NowPlaying {
                self.close_now_playing();
            }
        }
        egui::Panel::left("nav")
            .default_size(200.0)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(theme::SURFACE).inner_margin(12))
            .show(ui, |ui| self.nav(ui));
        egui::Panel::bottom("transport")
            .exact_size(84.0)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(theme::SURFACE).inner_margin(10))
            .show(ui, |ui| self.transport_bar(ui));
        egui::Panel::top("search")
            .exact_size(44.0)
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(8))
            .show(ui, |ui| self.top_bar(ui));
        if self.show_queue {
            egui::Panel::right("queue")
                .default_size(300.0)
                .resizable(true)
                .frame(egui::Frame::NONE.fill(theme::SURFACE).inner_margin(10))
                .show(ui, |ui| self.queue_panel(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(16))
            .show(ui, |ui| self.content(ui));
        let ctx = ui.ctx().clone();
        self.signal_path_window(&ctx);
        self.dsp_window(&ctx);
        self.login_window(&ctx);
    }
}

// ---- widgets ----

fn signal_path_ui(ui: &mut egui::Ui, path: &SignalPath, zone: &ZoneState) {
    ui.label(egui::RichText::new(format!("{:?}", path.headline())).strong());
    ui.weak(format!("Zone: {} · Output: {}", zone.name, zone.output));
    ui.add_space(6.0);
    egui::Grid::new("path")
        .num_columns(3)
        .spacing([14.0, 4.0])
        .show(ui, |ui| {
            for s in &path.stages {
                let color = match s.quality {
                    Quality::BitPerfect => theme::Q_BIT_PERFECT,
                    Quality::Enhanced => theme::Q_ENHANCED,
                    Quality::Lossless => theme::Q_LOSSLESS,
                    Quality::Lossy => theme::Q_LOSSY,
                };
                ui.colored_label(color, "●");
                ui.label(&s.name);
                ui.vertical(|ui| {
                    ui.weak(format!("{} → {}", s.in_format, s.out_format));
                    if let Some(n) = &s.note {
                        ui.small(egui::RichText::new(n).weak());
                    }
                });
                ui.end_row();
            }
        });
}

/// An album tile: `(response, play_clicked)`. The play circle appears on
/// hover in the art's corner and plays the album without opening it.
fn album_tile(
    ui: &mut egui::Ui,
    a: &Album,
    size: f32,
    tex: Option<&egui::TextureHandle>,
) -> (egui::Response, bool) {
    album_tile_badged(ui, a, size, tex, None)
}

/// An album tile with an optional format badge (`24/96 FLAC`) on the art.
fn album_tile_badged(
    ui: &mut egui::Ui,
    a: &Album,
    size: f32,
    tex: Option<&egui::TextureHandle>,
    badge: Option<&str>,
) -> (egui::Response, bool) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size + 44.0), egui::Sense::click());
    let art = egui::Rect::from_min_size(rect.min, egui::vec2(size, size));
    let play_rect = egui::Rect::from_min_size(
        egui::pos2(art.max.x - 46.0, art.max.y - 46.0),
        egui::vec2(38.0, 38.0),
    );
    let play_resp = ui.interact(play_rect, resp.id.with("play"), egui::Sense::click());
    let hovered = resp.hovered() || play_resp.hovered();
    if hovered {
        // A soft lift: shadow under the art.
        ui.painter().rect_filled(
            art.translate(egui::vec2(0.0, 4.0)).expand(2.0),
            egui::CornerRadius::same(theme::RADIUS + 2),
            egui::Color32::from_black_alpha(90),
        );
    }
    theme::cover(ui, art, tex, &a.title, hovered);
    if let Some(b) = badge {
        let font = egui::FontId::proportional(10.5);
        let galley = ui.painter().layout_no_wrap(b.to_string(), font, theme::FG);
        let pad = egui::vec2(5.0, 2.0);
        let size_b = galley.size() + pad * 2.0;
        let r = egui::Rect::from_min_size(egui::pos2(art.min.x + 6.0, art.max.y - 6.0 - size_b.y), size_b);
        ui.painter()
            .rect_filled(r, 4.0, egui::Color32::from_black_alpha(170));
        ui.painter().galley(r.min + pad, galley, theme::FG);
    }
    if hovered {
        ui.painter().rect_stroke(
            art,
            egui::CornerRadius::same(theme::RADIUS),
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(40)),
            egui::StrokeKind::Inside,
        );
    }
    let p = ui.painter();
    let title = truncate(&a.title, (size / 7.0) as usize);
    p.text(
        egui::pos2(rect.min.x + 1.0, art.max.y + 9.0),
        egui::Align2::LEFT_TOP,
        title,
        egui::FontId::proportional(13.0),
        theme::FG,
    );
    let mut sub = a.album_artist.clone().unwrap_or_default();
    if let Some(y) = a.year {
        if !sub.is_empty() {
            sub.push_str(" · ");
        }
        sub.push_str(&y.to_string());
    }
    p.text(
        egui::pos2(rect.min.x + 1.0, art.max.y + 26.0),
        egui::Align2::LEFT_TOP,
        truncate(&sub, (size / 6.5) as usize),
        egui::FontId::proportional(12.0),
        theme::MUTED,
    );
    if hovered {
        let c = play_rect.center();
        p.circle_filled(
            c,
            19.0,
            if play_resp.hovered() {
                theme::ACCENT_STRONG
            } else {
                theme::ACCENT
            },
        );
        let s = 7.0;
        p.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(c.x - s * 0.8 + 1.0, c.y - s),
                egui::pos2(c.x + s * 1.1 + 1.0, c.y),
                egui::pos2(c.x - s * 0.8 + 1.0, c.y + s),
            ],
            theme::ON_ACCENT,
            egui::Stroke::NONE,
        ));
    }
    let play = play_resp
        .on_hover_text("Play album")
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked();
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), play)
}

fn album_art(ui: &mut egui::Ui, a: &Album, size: f32, tex: Option<&egui::TextureHandle>) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter().rect_filled(
        rect.translate(egui::vec2(0.0, 6.0)).expand(2.0),
        egui::CornerRadius::same(theme::RADIUS + 2),
        egui::Color32::from_black_alpha(110),
    );
    theme::cover(ui, rect, tex, &a.title, false);
}

fn art_square(ui: &mut egui::Ui, title: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    theme::placeholder(ui.painter(), rect, title, false);
}

fn short_uri(uri: &str) -> String {
    let s = uri.trim_start_matches("https://").trim_start_matches("http://");
    truncate(s, 48)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

/// "3 min ago", "2 h ago", "4 d ago" from a unix timestamp.
fn fmt_ago(unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix);
    let s = (now - unix).max(0);
    if s < 60 {
        "just now".into()
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 86_400 {
        format!("{} h ago", s / 3600)
    } else {
        format!("{} d ago", s / 86_400)
    }
}

/// `~/x` → the home directory.
fn shellexpand_home(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|_| p.to_string()),
        None => p.to_string(),
    }
}

fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Render a QR code of `text` as a black-on-white texture.
fn qr_texture(ctx: &egui::Context, text: &str) -> Option<egui::TextureHandle> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let w = code.width();
    let quiet = 4;
    let size = w + 2 * quiet;
    let mut rgba = vec![255u8; size * size * 4];
    for y in 0..w {
        for x in 0..w {
            if code[(x, y)] == qrcode::Color::Dark {
                let i = ((y + quiet) * size + (x + quiet)) * 4;
                rgba[i] = 0;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
            }
        }
    }
    let img = egui::ColorImage::from_rgba_unmultiplied([size, size], &rgba);
    Some(ctx.load_texture("pubky-login-qr", img, egui::TextureOptions::NEAREST))
}

fn dsd_name(rate: u32) -> String {
    format!("DSD{}", rate / 44_100)
}

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("pubky-music")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_truncation() {
        assert_eq!(fmt_duration(0), "0:00");
        assert_eq!(fmt_duration(61_000), "1:01");
        assert_eq!(fmt_duration(3_600_000 + 5_000), "1:00:05");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a rather long title", 8), "a rathe…");
    }

    #[test]
    fn placeholder_colours_are_stable() {
        assert_eq!(theme::hsl(0.0, 0.35, 0.34), theme::hsl(0.0, 0.35, 0.34));
        let (r, g, b) = theme::hsl(0.5, 0.35, 0.34);
        assert!(r < g && r < b);
    }
}
