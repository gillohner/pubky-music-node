//! The app's connection to a node: one tokio runtime on its own thread,
//! commands in, messages out, a reconnecting `/v1/events` stream.

use pm_api::types::{
    AddPlaylistItems, ApiEvent, ArtistDetail, CreatePlaylist, DiscoverPage, Enqueue, Info, PlaylistDetail,
    PlaylistSummary, QueueAt, TransportAction,
};
use pm_client::NodeClient;
use pm_model::{Album, Track, Volume};
use pm_zones::{OutputInfo, QueueItem, Repeat, ZoneId, ZoneState};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
pub enum Cmd {
    Connect {
        base: String,
        token: String,
    },
    /// Find the node through the user's homeserver (ADR 0008).
    ConnectViaPubky {
        secret: String,
    },
    Refresh,
    LoadAlbums,
    LoadAlbumTracks(i64),
    LoadTracks,
    Search(String),
    Queue(String),
    Enqueue {
        zone: String,
        track_ids: Vec<i64>,
        at: QueueAt,
        replace: bool,
        play: bool,
    },
    ClearQueue(String),
    RemoveItem {
        zone: String,
        item: u64,
    },
    Transport {
        zone: String,
        action: TransportAction,
    },
    Volume {
        zone: String,
        volume: Volume,
    },
    Repeat {
        zone: String,
        repeat: Repeat,
    },
    Scan,
    LoadPlaylists,
    LoadPlaylist(String),
    CreatePlaylist {
        name: String,
        track_ids: Vec<i64>,
    },
    AddToPlaylist {
        id: String,
        track_ids: Vec<i64>,
    },
    RemovePlaylistItem {
        id: String,
        index: usize,
    },
    DeletePlaylist(String),
    PlayPlaylist {
        id: String,
        zone: String,
    },
    PubkySync,
    LoadCover(i64),
    LoadArtists,
    LoadArtist(i64),
    Discover(DiscoverKind),
    CreateZone {
        name: String,
        outputs: Vec<String>,
    },
    DeleteZone(String),
    RescanOutputs,
    LoadDsp(String),
    SetDsp(String, Box<pm_audio::PipelineConfig>),
    ResetDsp(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverKind {
    Trends,
    Tag(String),
    Friends(String),
}

#[derive(Debug)]
pub enum Msg {
    Connecting,
    Connected(Info),
    /// The node's address and token as resolved through the homeserver.
    ResolvedNode(String, String),
    ConnectFailed(String),
    Disconnected(String),
    Zones(Vec<ZoneState>),
    Zone(Box<ZoneState>),
    ZoneRemoved(ZoneId),
    Outputs(Vec<OutputInfo>),
    Albums(Vec<Album>),
    AlbumTracks(i64, Vec<Track>),
    Tracks(Vec<Track>),
    SearchResults(String, Vec<Track>),
    Queue(String, Vec<QueueItem>),
    LibraryChanged,
    ScanStarted,
    Playlists(Vec<PlaylistSummary>),
    Playlist(Box<PlaylistDetail>),
    PlaylistDeleted(String),
    Notice(String),
    Artists(Vec<pm_library::ArtistSummary>),
    Dsp(String, Box<pm_audio::PipelineConfig>),
    Discovered(DiscoverKind, Box<DiscoverPage>),
    Artist(Box<ArtistDetail>),
    /// Decoded RGBA cover, or `None` when the album has none.
    Cover(i64, Option<(usize, usize, Vec<u8>)>),
    Error(String),
}

pub struct Backend {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    msg_rx: std::sync::mpsc::Receiver<Msg>,
    /// The current client, shared with the local player's source factory.
    pub client: Arc<RwLock<Option<NodeClient>>>,
    pub handle: tokio::runtime::Handle,
}

impl Backend {
    pub fn start(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        let client = Arc::new(RwLock::new(None));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let handle = rt.handle().clone();
        let shared = client.clone();
        std::thread::Builder::new()
            .name("pm-app-backend".into())
            .spawn(move || {
                rt.block_on(run(
                    cmd_rx,
                    Emitter {
                        tx: msg_tx,
                        repaint: Arc::new(repaint),
                    },
                    shared,
                ));
            })
            .expect("backend thread");
        Self {
            cmd_tx,
            msg_rx,
            client,
            handle,
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn poll(&self) -> Vec<Msg> {
        let mut v = Vec::new();
        while let Ok(m) = self.msg_rx.try_recv() {
            v.push(m);
        }
        v
    }
}

#[derive(Clone)]
struct Emitter {
    tx: std::sync::mpsc::Sender<Msg>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Emitter {
    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        (self.repaint)();
    }
}

async fn run(mut cmds: mpsc::UnboundedReceiver<Cmd>, out: Emitter, shared: Arc<RwLock<Option<NodeClient>>>) {
    let mut client: Option<NodeClient> = None;
    // Bumped on every (re)connect so the previous event task stops.
    let (gen_tx, _) = watch::channel(0u64);
    let mut generation = 0u64;
    while let Some(cmd) = cmds.recv().await {
        let resolved = match &cmd {
            Cmd::ConnectViaPubky { secret } => {
                out.send(Msg::Connecting);
                match pm_client::connect_via_pubky(pm_pubky::Network::Mainnet, secret).await {
                    Ok((c, record)) => {
                        out.send(Msg::Notice(format!(
                            "found node “{}” at {}",
                            record.name, record.api_url
                        )));
                        out.send(Msg::ResolvedNode(record.api_url.clone(), record.token.clone()));
                        Some(Ok(c))
                    }
                    Err(e) => Some(Err(e)),
                }
            }
            Cmd::Connect { base, token } => {
                out.send(Msg::Connecting);
                Some(NodeClient::new(base, Some(token.clone())))
            }
            _ => None,
        };
        if let Some(resolved) = resolved {
            match resolved {
                Ok(c) => {
                    client = Some(c.clone());
                    *shared.write().expect("client lock") = Some(c.clone());
                    generation += 1;
                    let _ = gen_tx.send(generation);
                    let c2 = c.clone();
                    let out2 = out.clone();
                    match c.info().await {
                        Ok(info) => {
                            out.send(Msg::Connected(info));
                            let mut gen_rx = gen_tx.subscribe();
                            let my_gen = generation;
                            tokio::spawn(async move {
                                event_loop(c2, out2, &mut gen_rx, my_gen).await;
                            });
                            initial_load(&c, &out).await;
                        }
                        Err(e) => out.send(Msg::ConnectFailed(e.to_string())),
                    }
                }
                Err(e) => out.send(Msg::ConnectFailed(e.to_string())),
            }
            continue;
        }
        let Some(c) = client.clone() else {
            out.send(Msg::Error("not connected".into()));
            continue;
        };
        let out = out.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(c, cmd, &out).await {
                out.send(Msg::Error(e.to_string()));
            }
        });
    }
}

async fn initial_load(c: &NodeClient, out: &Emitter) {
    for r in [
        c.zones().await.map(Msg::Zones),
        c.outputs().await.map(Msg::Outputs),
        c.albums(None, 500).await.map(Msg::Albums),
        c.playlists().await.map(Msg::Playlists),
    ] {
        match r {
            Ok(m) => out.send(m),
            Err(e) => out.send(Msg::Error(e.to_string())),
        }
    }
}

async fn handle(c: NodeClient, cmd: Cmd, out: &Emitter) -> pm_client::Result<()> {
    match cmd {
        Cmd::Connect { .. } | Cmd::ConnectViaPubky { .. } => {}
        Cmd::Refresh => {
            out.send(Msg::Connected(c.info().await?));
            initial_load(&c, out).await;
        }
        Cmd::LoadAlbums => out.send(Msg::Albums(c.albums(None, 500).await?)),
        Cmd::LoadAlbumTracks(id) => out.send(Msg::AlbumTracks(id, c.album_tracks(id).await?)),
        Cmd::LoadTracks => out.send(Msg::Tracks(c.tracks(None, 1000).await?)),
        Cmd::Search(q) => {
            let r = c.search(&q, 200).await?;
            out.send(Msg::SearchResults(q, r));
        }
        Cmd::Queue(z) => {
            let q = c.queue(&z).await?;
            out.send(Msg::Queue(z, q));
        }
        Cmd::Enqueue {
            zone,
            track_ids,
            at,
            replace,
            play,
        } => {
            c.enqueue(
                &zone,
                &Enqueue {
                    track_ids,
                    items: vec![],
                    at,
                    replace,
                    play,
                },
            )
            .await?;
            let q = c.queue(&zone).await?;
            out.send(Msg::Queue(zone, q));
        }
        Cmd::ClearQueue(z) => {
            c.clear_queue(&z).await?;
            out.send(Msg::Queue(z, vec![]));
        }
        Cmd::RemoveItem { zone, item } => {
            c.remove_item(&zone, item).await?;
            let q = c.queue(&zone).await?;
            out.send(Msg::Queue(zone, q));
        }
        Cmd::Transport { zone, action } => {
            let st = c.transport(&zone, action).await?;
            out.send(Msg::Zone(Box::new(st)));
        }
        Cmd::Volume { zone, volume } => c.set_volume(&zone, volume).await?,
        Cmd::Repeat { zone, repeat } => c.set_repeat(&zone, repeat).await?,
        Cmd::Scan => {
            c.scan(vec![]).await?;
            out.send(Msg::ScanStarted);
        }
        Cmd::LoadPlaylists => out.send(Msg::Playlists(c.playlists().await?)),
        Cmd::LoadPlaylist(id) => out.send(Msg::Playlist(Box::new(c.playlist(&id).await?))),
        Cmd::CreatePlaylist { name, track_ids } => {
            let d = c
                .create_playlist(&CreatePlaylist {
                    name,
                    track_ids,
                    ..Default::default()
                })
                .await?;
            out.send(Msg::Notice(format!("created playlist “{}”", d.summary.name)));
            out.send(Msg::Playlist(Box::new(d)));
            out.send(Msg::Playlists(c.playlists().await?));
        }
        Cmd::AddToPlaylist { id, track_ids } => {
            let d = c
                .add_playlist_items(
                    &id,
                    &AddPlaylistItems {
                        track_ids,
                        ..Default::default()
                    },
                )
                .await?;
            out.send(Msg::Notice(format!("added to “{}”", d.summary.name)));
            out.send(Msg::Playlist(Box::new(d)));
            out.send(Msg::Playlists(c.playlists().await?));
        }
        Cmd::RemovePlaylistItem { id, index } => {
            let d = c.remove_playlist_item(&id, index).await?;
            out.send(Msg::Playlist(Box::new(d)));
            out.send(Msg::Playlists(c.playlists().await?));
        }
        Cmd::DeletePlaylist(id) => {
            c.delete_playlist(&id).await?;
            out.send(Msg::PlaylistDeleted(id));
            out.send(Msg::Playlists(c.playlists().await?));
        }
        Cmd::PlayPlaylist { id, zone } => {
            let r = c.play_playlist(&id, &zone, true).await?;
            if r.missing.is_empty() {
                out.send(Msg::Notice(format!("queued {} tracks", r.queued)));
            } else {
                out.send(Msg::Notice(format!(
                    "queued {} tracks; {} not in the library",
                    r.queued,
                    r.missing.len()
                )));
            }
        }
        Cmd::CreateZone { name, outputs } => {
            let z = if outputs.len() == 1 {
                c.create_zone(&name, &outputs[0]).await?
            } else {
                c.create_group_zone(&name, outputs).await?
            };
            out.send(Msg::Notice(format!("zone “{}” created", z.name)));
            out.send(Msg::Zones(c.zones().await?));
        }
        Cmd::LoadDsp(z) => {
            let cfg = c.dsp(&z).await?;
            out.send(Msg::Dsp(z, Box::new(cfg)));
        }
        Cmd::SetDsp(z, cfg) => {
            let cfg = c.set_dsp(&z, &cfg).await?;
            out.send(Msg::Notice("DSP saved; applies from the next track".into()));
            out.send(Msg::Dsp(z, Box::new(cfg)));
        }
        Cmd::ResetDsp(z) => {
            c.reset_dsp(&z).await?;
            let cfg = c.dsp(&z).await?;
            out.send(Msg::Dsp(z, Box::new(cfg)));
        }
        Cmd::RescanOutputs => {
            let outs = c.rescan_outputs().await?;
            out.send(Msg::Notice(format!("{} outputs", outs.len())));
            out.send(Msg::Outputs(outs));
            out.send(Msg::Zones(c.zones().await?));
        }
        Cmd::DeleteZone(id) => {
            c.delete_zone(&id).await?;
            out.send(Msg::Zones(c.zones().await?));
        }
        Cmd::Discover(kind) => {
            let page = match &kind {
                DiscoverKind::Trends => c.discover_trends("7d", 40).await?,
                DiscoverKind::Tag(l) => c.discover_tag(l, 40).await?,
                DiscoverKind::Friends(v) => c.discover_friends(v, 40).await?,
            };
            out.send(Msg::Discovered(kind, Box::new(page)));
        }
        Cmd::LoadArtists => out.send(Msg::Artists(c.artists().await?)),
        Cmd::LoadArtist(id) => out.send(Msg::Artist(Box::new(c.artist(id).await?))),
        Cmd::LoadCover(id) => {
            let decoded = match c.album_cover(id).await? {
                Some((_, bytes)) => tokio::task::spawn_blocking(move || {
                    image::load_from_memory(&bytes).ok().map(|img| {
                        let img = img.thumbnail(512, 512).to_rgba8();
                        (img.width() as usize, img.height() as usize, img.into_raw())
                    })
                })
                .await
                .ok()
                .flatten(),
                None => None,
            };
            out.send(Msg::Cover(id, decoded));
        }
        Cmd::PubkySync => {
            let r = c.pubky_sync().await?;
            out.send(Msg::Notice(format!(
                "sync: {} pushed, {} pulled, {} deleted{}",
                r.pushed,
                r.pulled,
                r.deleted,
                if r.errors.is_empty() {
                    String::new()
                } else {
                    format!(", {} errors", r.errors.len())
                }
            )));
            out.send(Msg::Playlists(c.playlists().await?));
        }
    }
    Ok(())
}

/// Keep `/v1/events` open; reconnect with backoff until a newer generation
/// (a new Connect) supersedes this task.
async fn event_loop(c: NodeClient, out: Emitter, gen_rx: &mut watch::Receiver<u64>, my_gen: u64) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *gen_rx.borrow() != my_gen {
            return;
        }
        match c.events().await {
            Ok(mut stream) => {
                backoff = Duration::from_secs(1);
                loop {
                    tokio::select! {
                        ev = stream.next() => match ev {
                            Some(Ok(ev)) => forward(ev, &out),
                            Some(Err(e)) => {
                                out.send(Msg::Disconnected(e.to_string()));
                                break;
                            }
                            None => {
                                out.send(Msg::Disconnected("node closed the event stream".into()));
                                break;
                            }
                        },
                        _ = gen_rx.changed() => return,
                    }
                }
            }
            Err(e) => out.send(Msg::Disconnected(e.to_string())),
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = gen_rx.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(15));
    }
}

fn forward(ev: ApiEvent, out: &Emitter) {
    match ev {
        ApiEvent::Snapshot { zones } => out.send(Msg::Zones(zones)),
        ApiEvent::Zone { state } => out.send(Msg::Zone(Box::new(state))),
        ApiEvent::ZoneEvent { .. } => {}
        ApiEvent::ZoneRemoved { zone } => out.send(Msg::ZoneRemoved(zone)),
        ApiEvent::LibraryChanged => out.send(Msg::LibraryChanged),
    }
}
