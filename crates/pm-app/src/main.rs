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

use backend::{Backend, Cmd, DiscoverKind, Msg};
use local::{LocalPlayer, LOCAL_ZONE_ID};
use pm_api::sources::NodeSources;
use pm_api::types::{
    ArtistDetail, DiscoverPage, Info, PlaylistDetail, PlaylistSummary, QueueAt, TransportAction,
};
use pm_model::{Album, Quality, SignalPath, Track, Volume};
use pm_zones::{Insert, QueueItem, Transport, ZoneState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

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
    Albums,
    Album(i64),
    Artists,
    Artist(i64),
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
    album_tracks: HashMap<i64, Vec<Track>>,
    tracks: Vec<Track>,
    search: String,
    results: Option<(String, Vec<Track>)>,
    queues: HashMap<String, Vec<QueueItem>>,
    playlists: Vec<PlaylistSummary>,
    playlist_details: HashMap<String, PlaylistDetail>,
    artists: Vec<pm_library::ArtistSummary>,
    artist_details: HashMap<i64, ArtistDetail>,
    discover_kind: DiscoverKind,
    discover: Option<(DiscoverKind, DiscoverPage)>,
    discover_loading: bool,
    friends_viewer: String,
    new_zone_name: String,
    new_zone_outputs: Vec<String>,
    show_queue: bool,
    show_dsp: bool,
    /// DSP editor state: zone id it belongs to and the config being edited.
    dsp_edit: Option<(String, pm_audio::PipelineConfig)>,
    /// Last queue version seen per remote zone, to refetch the queue on change.
    queue_versions: HashMap<String, u64>,
    new_playlist_name: String,
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
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let mut style = (*cc.egui_ctx.style_of(egui::Theme::Dark)).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        cc.egui_ctx.set_style_of(egui::Theme::Dark, style);
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
            album_tracks: HashMap::new(),
            tracks: Vec::new(),
            search: String::new(),
            results: None,
            queues: HashMap::new(),
            playlists: Vec::new(),
            playlist_details: HashMap::new(),
            artists: Vec::new(),
            artist_details: HashMap::new(),
            discover_kind: DiscoverKind::Trends,
            discover: None,
            discover_loading: false,
            friends_viewer: String::new(),
            new_zone_name: String::new(),
            new_zone_outputs: Vec::new(),
            show_queue: true,
            show_dsp: false,
            dsp_edit: None,
            queue_versions: HashMap::new(),
            new_playlist_name: String::new(),
            covers: HashMap::new(),
            cover_requests: std::collections::HashSet::new(),
            view: View::Albums,
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
                Msg::AlbumTracks(id, t) => {
                    self.album_tracks.insert(id, t);
                }
                Msg::Tracks(t) => self.tracks = t,
                Msg::SearchResults(q, t) => self.results = Some((q, t)),
                Msg::Queue(z, q) => {
                    self.queues.insert(z, q);
                }
                Msg::LibraryChanged => {
                    self.scanning = false;
                    self.backend.send(Cmd::LoadAlbums);
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

    fn play_tracks(&mut self, tracks: &[Track], replace: bool, at: QueueAt) {
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
        ui.add_space(6.0);
        ui.heading(egui::RichText::new("Pubky Music").strong());
        ui.add_space(10.0);
        ui.label(egui::RichText::new("LIBRARY").small().weak());
        for (label, view) in [
            ("Albums", View::Albums),
            ("Artists", View::Artists),
            ("Tracks", View::Tracks),
            ("Playlists", View::Playlists),
        ] {
            let selected = self.view == view
                || matches!((&self.view, &view), (View::Album(_), View::Albums))
                || matches!((&self.view, &view), (View::Artist(_), View::Artists))
                || matches!((&self.view, &view), (View::Playlist(_), View::Playlists));
            if ui.selectable_label(selected, label).clicked() {
                if view == View::Tracks && self.tracks.is_empty() {
                    self.backend.send(Cmd::LoadTracks);
                }
                if view == View::Playlists {
                    self.backend.send(Cmd::LoadPlaylists);
                }
                if view == View::Artists {
                    self.backend.send(Cmd::LoadArtists);
                }
                self.view = view;
            }
        }
        ui.add_space(10.0);
        ui.label(egui::RichText::new("PUBKY").small().weak());
        if ui
            .selectable_label(self.view == View::Discover, "Discover")
            .clicked()
        {
            self.view = View::Discover;
            if self.discover.is_none() && !self.discover_loading {
                self.load_discover(DiscoverKind::Trends);
            }
        }
        ui.add_space(10.0);
        ui.label(egui::RichText::new("PLAYING").small().weak());
        if ui.selectable_label(self.view == View::Queue, "Queue").clicked() {
            self.view = View::Queue;
            self.refresh_queue();
        }
        ui.add_space(10.0);
        ui.label(egui::RichText::new("NODE").small().weak());
        if ui
            .selectable_label(self.view == View::Settings, "Settings")
            .clicked()
        {
            self.view = View::Settings;
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            match &self.conn {
                Conn::Connected(info) => {
                    ui.small(format!("{} tracks", info.library.tracks));
                    ui.small(
                        egui::RichText::new(format!("● {}", info.name))
                            .color(egui::Color32::from_rgb(90, 200, 120)),
                    );
                }
                Conn::Connecting => {
                    ui.small(egui::RichText::new("○ connecting…").weak());
                }
                Conn::Failed(e) => {
                    ui.small(
                        egui::RichText::new("● not connected").color(egui::Color32::from_rgb(220, 90, 90)),
                    )
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
            View::Albums => self.albums_view(ui),
            View::Album(id) => self.album_view(ui, id),
            View::Artists => self.artists_view(ui),
            View::Artist(id) => self.artist_view(ui, id),
            View::Discover => self.discover_view(ui),
            View::Playlists => self.playlists_view(ui),
            View::Playlist(id) => self.playlist_view(ui, &id),
            View::Tracks => {
                let tracks = self.tracks.clone();
                ui.heading("Tracks");
                self.track_table(ui, &tracks, true);
            }
            View::Search => {
                let Some((q, tracks)) = self.results.clone() else {
                    ui.weak("Type a query and press Enter.");
                    return;
                };
                ui.heading(format!("Results for “{q}”"));
                if tracks.is_empty() {
                    ui.weak("Nothing matched.");
                } else {
                    self.track_table(ui, &tracks, true);
                }
            }
            View::Queue => self.queue_view(ui),
            View::Settings => self.settings_view(ui),
        }
    }

    fn albums_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Albums");
            ui.weak(format!("{}", self.albums.len()));
        });
        if self.albums.is_empty() {
            ui.weak("No albums yet. Add folders to the node config and run a scan from Settings.");
            return;
        }
        let albums = self.albums.clone();
        let tile = 168.0;
        let gap = 14.0;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let cols = ((ui.available_width() + gap) / (tile + gap)).floor().max(1.0) as usize;
            let mut clicked = None;
            for row in albums.chunks(cols) {
                ui.horizontal(|ui| {
                    for a in row {
                        let tex = self.cover(a.id);
                        if album_tile(ui, a, tile, tex.as_ref()).clicked() {
                            clicked = a.id;
                        }
                    }
                });
                ui.add_space(gap - 6.0);
            }
            if let Some(id) = clicked {
                if !self.album_tracks.contains_key(&id) {
                    self.backend.send(Cmd::LoadAlbumTracks(id));
                }
                self.view = View::Album(id);
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
            album_art(ui, &album, 180.0, tex.as_ref());
            ui.vertical(|ui| {
                ui.heading(egui::RichText::new(&album.title).size(26.0).strong());
                ui.label(
                    album
                        .album_artist
                        .clone()
                        .unwrap_or_else(|| "Unknown artist".into()),
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
                    if ui.button("▶  Play album").clicked() {
                        self.play_tracks(&tracks, true, QueueAt::End);
                    }
                    if ui.button("Play next").clicked() {
                        self.play_tracks(&tracks, false, QueueAt::Next);
                    }
                    if ui.button("Add to queue").clicked() {
                        self.play_tracks(&tracks, false, QueueAt::End);
                    }
                });
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
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let mut action: Option<(usize, &str)> = None;
            let mut playlist_add: Option<(usize, usize)> = None;
            egui::Grid::new(("tracks", show_album))
                .num_columns(if show_album { 5 } else { 4 })
                .striped(true)
                .min_col_width(40.0)
                .show(ui, |ui| {
                    for (i, t) in tracks.iter().enumerate() {
                        let no = t.track_no.map(|n| n.to_string()).unwrap_or_default();
                        if ui
                            .add(egui::Button::new("▶").frame(false))
                            .on_hover_text("Play from here")
                            .clicked()
                        {
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
                            ui.menu_button("⋯", |ui| {
                                if ui.button("Play next").clicked() {
                                    action = Some((i, "next"));
                                    ui.close();
                                }
                                if ui.button("Add to queue").clicked() {
                                    action = Some((i, "add"));
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
                    _ => self.play_tracks(&tracks[i..=i], false, QueueAt::End),
                }
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
        ui.heading(egui::RichText::new(&d.artist.name).size(26.0).strong());
        ui.weak(format!("{} albums · {} tracks", d.artist.albums, d.artist.tracks));
        ui.add_space(8.0);
        let tile = 168.0;
        let gap = 14.0;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let cols = ((ui.available_width() + gap) / (tile + gap)).floor().max(1.0) as usize;
            let mut clicked = None;
            for row in d.albums.chunks(cols) {
                ui.horizontal(|ui| {
                    for a in row {
                        let tex = self.cover(a.id);
                        if album_tile(ui, a, tile, tex.as_ref()).clicked() {
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
                    .hint_text("your pubky (z32) for friends' activity")
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
        ui.add_space(6.0);
        if self.playlists.is_empty() {
            ui.weak("No playlists yet. Create one above, then use ⋯ on any track to add it.");
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
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("playlist-items")
                .num_columns(4)
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
                        if ui.small_button("✕").clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some(i) = remove {
            self.backend.send(Cmd::RemovePlaylistItem {
                id: id.to_string(),
                index: i,
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
                .button("Sync playlists to Pubky")
                .on_hover_text("Needs a session on the node")
                .clicked()
            {
                self.backend.send(Cmd::PubkySync);
            }
        });
        if let Conn::Failed(e) = &self.conn {
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), e);
        }
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
                        art_square(
                            ui,
                            &np.item.title.clone().unwrap_or_else(|| np.item.uri.clone()),
                            56.0,
                        );
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    np.item.title.clone().unwrap_or_else(|| np.item.uri.clone()),
                                )
                                .strong(),
                            );
                            ui.weak(np.item.artist.clone().unwrap_or_default());
                            ui.small(egui::RichText::new(np.item.album.clone().unwrap_or_default()).weak());
                        });
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
                        ui.add_space((ui.available_width() - 150.0).max(0.0) / 2.0);
                        if ui
                            .add(egui::Button::new("⏮").min_size(egui::vec2(36.0, 30.0)))
                            .clicked()
                        {
                            self.transport(TransportAction::Previous);
                        }
                        let playing = zone.transport == Transport::Playing;
                        if ui
                            .add(
                                egui::Button::new(if playing { "⏸" } else { "▶" })
                                    .min_size(egui::vec2(44.0, 30.0)),
                            )
                            .clicked()
                        {
                            self.transport(TransportAction::Toggle);
                        }
                        if ui
                            .add(egui::Button::new("⏭").min_size(egui::vec2(36.0, 30.0)))
                            .clicked()
                        {
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
                    Some(Quality::BitPerfect) => ("Bit-perfect", egui::Color32::from_rgb(120, 90, 220)),
                    Some(Quality::Enhanced) => ("Enhanced", egui::Color32::from_rgb(90, 160, 220)),
                    Some(Quality::Lossless) => ("Lossless", egui::Color32::from_rgb(90, 200, 120)),
                    Some(Quality::Lossy) => ("Lossy", egui::Color32::from_rgb(230, 170, 60)),
                    None => ("—", egui::Color32::GRAY),
                };
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new(format!("● {label}")).color(color))
                            .frame(false),
                    )
                    .on_hover_text("Signal path")
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
                    ui.weak(format!("group · {} outputs · drift ≤ {worst} ms", n.len()));
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
                        ui.label("Loudness (ReplayGain)");
                        egui::ComboBox::from_id_salt("loudness")
                            .selected_text(format!("{:?}", cfg.loudness))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut cfg.loudness, pm_audio::Loudness::Off, "Off");
                                ui.selectable_value(&mut cfg.loudness, pm_audio::Loudness::Track, "Track");
                                ui.selectable_value(&mut cfg.loudness, pm_audio::Loudness::Album, "Album");
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
        }
        egui::Panel::left("nav")
            .default_size(200.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(24, 24, 28))
                    .inner_margin(12),
            )
            .show(ui, |ui| self.nav(ui));
        egui::Panel::bottom("transport")
            .exact_size(84.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(30, 30, 36))
                    .inner_margin(10),
            )
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
                .frame(
                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgb(26, 26, 31))
                        .inner_margin(10),
                )
                .show(ui, |ui| self.queue_panel(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(16))
            .show(ui, |ui| self.content(ui));
        let ctx = ui.ctx().clone();
        self.signal_path_window(&ctx);
        self.dsp_window(&ctx);
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
                    Quality::BitPerfect => egui::Color32::from_rgb(120, 90, 220),
                    Quality::Enhanced => egui::Color32::from_rgb(90, 160, 220),
                    Quality::Lossless => egui::Color32::from_rgb(90, 200, 120),
                    Quality::Lossy => egui::Color32::from_rgb(230, 170, 60),
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

fn album_tile(ui: &mut egui::Ui, a: &Album, size: f32, tex: Option<&egui::TextureHandle>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size + 40.0), egui::Sense::click());
    let art = egui::Rect::from_min_size(rect.min, egui::vec2(size, size));
    match tex {
        Some(t) => paint_texture(ui.painter(), art, t, resp.hovered()),
        None => paint_art(ui.painter(), art, &a.title, resp.hovered()),
    }
    let p = ui.painter();
    let font = egui::FontId::proportional(13.0);
    let title = truncate(&a.title, 24);
    p.text(
        egui::pos2(rect.min.x, art.max.y + 6.0),
        egui::Align2::LEFT_TOP,
        title,
        font.clone(),
        ui.visuals().text_color(),
    );
    p.text(
        egui::pos2(rect.min.x, art.max.y + 22.0),
        egui::Align2::LEFT_TOP,
        truncate(a.album_artist.as_deref().unwrap_or(""), 26),
        egui::FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn album_art(ui: &mut egui::Ui, a: &Album, size: f32, tex: Option<&egui::TextureHandle>) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    match tex {
        Some(t) => paint_texture(ui.painter(), rect, t, false),
        None => paint_art(ui.painter(), rect, &a.title, false),
    }
}

fn paint_texture(p: &egui::Painter, rect: egui::Rect, tex: &egui::TextureHandle, hovered: bool) {
    let tint = if hovered {
        egui::Color32::from_white_alpha(230)
    } else {
        egui::Color32::WHITE
    };
    p.image(
        tex.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        tint,
    );
}

fn art_square(ui: &mut egui::Ui, title: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    paint_art(ui.painter(), rect, title, false);
}

/// Placeholder cover: a colour from the title and its initials. Real
/// artwork arrives with the library's art extraction.
fn paint_art(p: &egui::Painter, rect: egui::Rect, title: &str, hovered: bool) {
    let h = title
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    let hue = (h % 360) as f32 / 360.0;
    let (r, g, b) = hsl(hue, 0.35, if hovered { 0.42 } else { 0.34 });
    p.rect_filled(
        rect,
        egui::CornerRadius::same(6),
        egui::Color32::from_rgb(r, g, b),
    );
    let initials: String = title
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initials,
        egui::FontId::proportional(rect.height() * 0.3),
        egui::Color32::from_white_alpha(200),
    );
}

fn hsl(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h * 6.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// `https://musicbrainz.org/recording/<mbid>` → `musicbrainz.org/recording/…`
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

fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
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
        assert_eq!(hsl(0.0, 0.35, 0.34), hsl(0.0, 0.35, 0.34));
        let (r, g, b) = hsl(0.5, 0.35, 0.34);
        assert!(r < g && r < b);
    }
}
