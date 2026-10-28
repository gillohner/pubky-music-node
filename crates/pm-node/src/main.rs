//! `pm-node`: the Pubky Music Node binary.
//!
//! For now a CLI over the core services; the long-running daemon (API,
//! zones, homeserver embedding) arrives with M3.

mod api_token;
mod config;
mod library;
mod outputs;
mod play;
mod social;

use clap::{Parser, Subcommand};
use config::NodeConfig;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pm-node", version, about = "Pubky Music Node")]
struct Cli {
    /// Path to the node's config.toml.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the effective configuration as TOML.
    Config,
    /// Run the node: API on `[api] listen`, one zone per output.
    Serve,
    /// List the outputs this node can play to.
    Outputs,
    /// Inspect an audio file: container, format, duration.
    Info { file: PathBuf },
    /// Play a file to an output and print the signal path.
    /// Play files in order through a zone (gapless when formats match).
    Play {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Output id from `outputs`, or `wav:<path>` to write a WAV file.
        #[arg(long, default_value = "null")]
        to: String,
        /// Software volume in dB (negative). Leaves the bit-perfect path.
        #[arg(long)]
        volume_db: Option<f32>,
        /// Pre-DSP headroom in dB (negative).
        #[arg(long)]
        headroom_db: Option<f32>,
    },
    /// Set an output's hardware volume in dB (0 = full scale). Leaves the
    /// signal path bit-perfect; outputs without a mixer control refuse.
    Volume {
        /// Output id from `outputs`.
        to: String,
        #[arg(allow_hyphen_values = true)]
        db: f32,
    },
    /// Scan folders into the library (configured folders when none given).
    Scan { folders: Vec<PathBuf> },
    /// Sign in with Pubky Ring: prints a URL and QR code, waits for approval,
    /// stores the session secret for `serve`, `upload` and sync.
    Login,
    /// Upload audio files to the user's private homeserver storage
    /// (`pubky.session_secret_file` must be set).
    Upload {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Import a playlist file (M3U/M3U8, CSV, Roon's Excel export) into the
    /// library as a private playlist; entries match by path, file name, then
    /// title and artist. Unmatched entries are listed.
    ImportPlaylist {
        file: PathBuf,
        /// Playlist name (default: from the file).
        #[arg(long)]
        name: Option<String>,
    },
    /// Library counts.
    Stats,
    /// Write a snapshot of the library database (playlists, favourites,
    /// tags, history, settings, lyrics, art index) to a file; safe while
    /// the node runs.
    Backup {
        file: PathBuf,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Replace the library database with a backup; stop the node first.
    Restore { file: PathBuf },
    /// List tracks.
    Tracks {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Full-text search over title, artist and album.
    Search {
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Resolve a service URL or ISRC URI to its MusicBrainz canonical form.
    Resolve { input: String },
    /// Parse and normalize a music URL into its canonical form (ADR 0001).
    Uri { input: String },
    /// Read-only calls against the Pinakes index.
    Pinakes {
        #[command(subcommand)]
        cmd: PinakesCmd,
    },
    /// Social discovery over Pinakes and Nexus (handoff §8.4).
    Discover {
        #[command(subcommand)]
        cmd: DiscoverCmd,
    },
}

#[derive(Subcommand)]
enum PinakesCmd {
    /// Instance info and ingestion lag.
    Info,
    /// Cards for one or more URLs.
    Lookup { uris: Vec<String> },
    /// Search by tags (comma list, max 5).
    Search {
        #[arg(long, default_value = "music")]
        tags: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum DiscoverCmd {
    /// Trending music: tags made inside a window (needs Pinakes's music
    /// step), falling back to all-time activity and tag facets.
    Trends {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value = "7d")]
        window: String,
    },
    /// Every tagged link carrying an exact id: `mbid:<uuid>`, `isrc:<ISRC>`.
    Key { key: String },
    /// Music cards tagged with a label.
    ByTag {
        label: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Music referenced by the people a viewer follows (raw z32 key).
    Friends {
        viewer: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

async fn serve(cfg: &NodeConfig, config_file: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let db_path = cfg.db_path();
    let lib = pm_library::Library::open(&db_path)?;
    let registry = outputs::discover_all(&cfg.outputs);
    pm_api::tidal_unofficial::set_config_client(
        &cfg.tidal.unofficial_client_id,
        &cfg.tidal.unofficial_client_secret,
    );
    pm_api::ytdlp::set_data_dir(&cfg.data_dir);
    pm_api::ytdlp::set_config_path(&cfg.youtube.ytdlp);
    pm_api::soloist::set_data_dir(&cfg.data_dir);
    pm_api::soloist::set_config_path(&cfg.spotify.soloist);
    pm_api::librespot::set_data_dir(&cfg.data_dir);
    pm_api::librespot::set_config_path(&cfg.spotify.librespot);
    pm_api::qobuz::set_config_app(&cfg.qobuz.app_id, &cfg.qobuz.app_secret);
    pm_api::soundcloud::set_config_client(&cfg.soundcloud.client_id, &cfg.soundcloud.token);
    pm_api::lastfm::set_config_app(
        &env_or("PM_LASTFM_API_KEY", &cfg.lastfm.api_key),
        &env_or("PM_LASTFM_API_SECRET", &cfg.lastfm.api_secret),
    );
    let (token, source) = api_token::resolve(&cfg.api.token, &cfg.api.listen, &cfg.data_dir)
        .map_err(|e| anyhow::anyhow!("API token: {e}"))?;
    match &source {
        api_token::Source::File(path) => tracing::info!(
            listen = %cfg.api.listen,
            "no API token configured; clients sign in with the one in {}",
            path.display()
        ),
        api_token::Source::ReplacedPlaceholder(path) => tracing::warn!(
            "[api] token is a placeholder from the docs that anyone can know; clients sign in with the one in {} \
             instead (or set a long random token of your own)",
            path.display()
        ),
        api_token::Source::Configured | api_token::Source::None => {}
    }
    let (public_url, public_url_guessed) = public_url(cfg);
    let state = pm_api::NodeState::new(
        pm_api::NodeSettings {
            name: cfg.api.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            db_path,
            scan_folders: cfg.library.folders.clone(),
            art_dir: cfg.data_dir.join("art"),
            mirror_dir: cfg.data_dir.join("homeserver"),
            pipeline: pm_audio::PipelineConfig::default(),
            token,
            subsonic_user: cfg.api.subsonic_user.clone(),
            pubky: cfg.pubky.clone(),
            public_url,
            upnp: cfg.outputs.upnp,
            cast: cfg.outputs.cast,
            slimproto_port: cfg.outputs.slimproto_port,
            session_secret_path: Some(session_secret_path(cfg)),
            // An empty config falls back to the project's built-in apps (ids only, no secrets)
            tidal: if cfg.tidal.client_id.trim().is_empty() {
                pm_api::tidal::TidalCredentials {
                    client_id: pm_api::project_apps::tidal_client_id()
                        .unwrap_or_default()
                        .to_string(),
                    client_secret: String::new(),
                    scopes: cfg.tidal.scopes.clone(),
                }
            } else {
                pm_api::tidal::TidalCredentials {
                    client_id: cfg.tidal.client_id.clone(),
                    client_secret: cfg.tidal.client_secret.clone(),
                    scopes: cfg.tidal.scopes.clone(),
                }
            },
            spotify: pm_api::spotify::SpotifyCredentials {
                client_id: cfg.spotify.client_id.clone(),
                client_secret: cfg.spotify.client_secret.clone(),
            },
            acoustid_key: {
                let own = std::env::var("PM_ACOUSTID_KEY")
                    .ok()
                    .filter(|k| !k.trim().is_empty())
                    .unwrap_or_else(|| cfg.acoustid.api_key.clone());
                if own.trim().is_empty() {
                    pm_api::project_apps::acoustid_key()
                        .unwrap_or_default()
                        .to_string()
                } else {
                    own
                }
            },
            jamendo: {
                let id = env_or("PM_JAMENDO_CLIENT_ID", &cfg.jamendo.client_id);
                if id.trim().is_empty() {
                    pm_api::jamendo::JamendoCredentials {
                        client_id: pm_api::project_apps::jamendo_client_id()
                            .unwrap_or_default()
                            .to_string(),
                        client_secret: String::new(),
                    }
                } else {
                    pm_api::jamendo::JamendoCredentials {
                        client_id: id,
                        client_secret: env_or("PM_JAMENDO_CLIENT_SECRET", &cfg.jamendo.client_secret),
                    }
                }
            },
        },
        lib,
        registry,
    );
    state.set_public_url_guessed(public_url_guessed);
    // A token the node made itself may be replaced from Settings; one from the config may not
    if let api_token::Source::File(path) | api_token::Source::ReplacedPlaceholder(path) = &source {
        state.set_token_file(path.clone());
    }
    state.zones_for_all_outputs();
    {
        let st = state.clone();
        tokio::task::spawn_blocking(move || pm_api::soloist::autostart(&st));
        let st = state.clone();
        tokio::task::spawn_blocking(move || pm_api::librespot::autostart(&st));
    }
    if let Err(e) = state.start_slimproto() {
        tracing::warn!("slimproto: {e}");
    }
    if !cfg.outputs.snapcast.trim().is_empty() {
        state.set_snapcast(Some(cfg.outputs.snapcast.trim().to_string()));
    }
    state.set_airplay(cfg.outputs.airplay);
    state.set_bluos(cfg.outputs.bluos);
    state.set_mpd(&cfg.outputs.mpd);
    {
        let st = state.clone();
        let found = tokio::task::spawn_blocking(move || {
            let mut found = st.discover_upnp(std::time::Duration::from_secs(3));
            found.extend(st.discover_snapcast());
            found.extend(st.discover_airplay(std::time::Duration::from_millis(1500)));
            found.extend(st.discover_bluos(std::time::Duration::from_millis(1500)));
            found.extend(st.discover_mpd());
            found.extend(st.restore_upnp_addresses());
            found.extend(st.restore_endpoint_addresses());
            found
        })
        .await?;
        if !found.is_empty() {
            tracing::info!(count = found.len(), "UPnP renderers found");
        }
    }
    for z in state.zones.list() {
        tracing::info!(zone = %z.id, output = %z.output, "zone ready");
    }
    // A "Bring files here" job the node was running carries on
    pm_api::files::resume_jobs(&state);
    {
        let ocfg = cfg.outputs.clone();
        state.set_discover(std::sync::Arc::new(move || {
            let reg = outputs::discover_all(&ocfg);
            reg.list()
                .into_iter()
                .filter_map(|info| reg.get(&info.id))
                .collect()
        }));
    }
    if cfg.library.watch {
        state.watch_folders(std::time::Duration::from_secs(
            cfg.library.watch_quiet_secs.max(1),
        ));
    }
    // The config's folders, in the background: unchanged files are skipped, so a restart stays cheap
    state.scan_config_folders();
    if cfg.remote.enabled && state.token().is_none() {
        // Whoever reached a tokenless node through the tunnel would be its owner
        tracing::info!("remote access off: the node has no API token");
    } else if cfg.remote.enabled {
        // The tunnel gets its own listener, so the node knows those requests came through it
        match pm_api::peer::serve_remote(state.clone()).await {
            Ok(target) => {
                if let Some(remote) = remote_config(cfg, target) {
                    state.configure_remote(remote).await;
                }
            }
            Err(e) => tracing::warn!("remote access off: no listener for it: {e}"),
        }
    }
    let secret_file = session_secret_path(cfg);
    if secret_file.exists() {
        let file = &secret_file;
        match std::fs::read_to_string(file) {
            Ok(secret) => match pm_api::sync::PubkySync::connect_with(state.homeserver()?, &secret).await {
                Ok(sync) => {
                    tracing::info!(user = %sync.z32(), "Pubky session restored");
                    let sync = std::sync::Arc::new(sync);
                    state.set_sync(Some(sync.clone()));
                    // Republish the node record: the address, token or version may
                    // have changed since the sign-in that first wrote it (ADR 0008).
                    if let Err(e) = sync.publish_node_for(&state).await {
                        tracing::warn!("publishing the node record failed: {e}");
                    }
                    match sync.restore_settings(&state, true).await {
                        Ok(Some(r)) => tracing::info!(?r, "settings restored from the homeserver"),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("settings restore failed: {e}"),
                    }
                    // The sign-ins in the person's vault, then every 15 minutes
                    pm_api::vault_cache::start(&state, sync.clone());
                    match sync.sync(&state.library).await {
                        Ok(r) => {
                            tracing::info!(
                                pushed = r.pushed,
                                pulled = r.pulled,
                                deleted = r.deleted,
                                pending = r.pending,
                                errors = r.errors.len(),
                                "initial sync"
                            );
                            sync.carry_on(&state, &r);
                        }
                        Err(e) => tracing::warn!("initial sync failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!(file = %file.display(), "cannot restore Pubky session: {e}"),
            },
            Err(e) => tracing::warn!(file = %file.display(), "cannot read session secret: {e}"),
        }
    }
    if let Some(ep) = pm_api::bundled::AdminEndpoint::new(
        &env_or("PM_HOMESERVER_ADMIN_URL", &cfg.homeserver.admin_url),
        &env_or("PM_HOMESERVER_ADMIN_PASSWORD", &cfg.homeserver.admin_password),
    ) {
        tracing::info!(admin = %ep.url, "bundled homeserver");
        state.set_bundled_homeserver(ep);
    }
    let tunnel_dir = env_or("PM_TUNNEL_DIR", &cfg.homeserver.tunnel_dir);
    if !tunnel_dir.trim().is_empty() {
        state.set_tunnel_dir(PathBuf::from(tunnel_dir.trim()));
    }
    let files_dir = env_or("PM_HOMESERVER_FILES_DIR", &cfg.homeserver.files_dir);
    if !files_dir.trim().is_empty() {
        state.set_homeserver_files_dir(PathBuf::from(files_dir.trim()));
    }
    pm_api::storage::spawn_autosave(&state);
    pm_api::loudness::spawn(&state);
    pm_api::webdav::spawn_rescans(&state);
    state.set_drive_apps(pm_api::drives::apps_from(
        pm_api::drives::App {
            client_id: env_or("PM_GOOGLE_DRIVE_CLIENT_ID", &cfg.drives.google_client_id),
            client_secret: env_or("PM_GOOGLE_DRIVE_CLIENT_SECRET", &cfg.drives.google_client_secret),
        },
        pm_api::drives::App {
            client_id: env_or("PM_ONEDRIVE_CLIENT_ID", &cfg.drives.onedrive_client_id),
            client_secret: String::new(),
        },
    ));
    pm_api::drives::spawn_rescans(&state);
    state.set_config_file(config_file.and_then(|p| p.canonicalize().ok()));
    state.set_return_page(&cfg.auth.return_page);
    // Tests point TIDAL at a mock (tools/ui-tidal-login.js); never needed in use
    {
        let var = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let mut t = state.tidal.write().expect("tidal endpoints");
        if let Some(v) = var("PM_TIDAL_AUTH_BASE") {
            t.auth_base = v;
        }
        if let Some(v) = var("PM_TIDAL_LOGIN_BASE") {
            t.login_base = v;
        }
        if let Some(v) = var("PM_TIDAL_API_BASE") {
            t.api_base = v;
        }
    }
    pm_api::backup::start_schedule(&state);
    pm_api::podcasts::spawn_background(&state);
    pm_api::gpodder::spawn_background(&state);
    pm_api::audiobookshelf::spawn_background(&state);
    let _announced = announce(cfg, &state);
    let _renderers = renderers(cfg, &state);
    pm_api::serve(state, &cfg.api.listen).await
}

/// Remote access settings from `[remote]` for the tunnel's listener at `target`, or `None` when
/// it is off or misconfigured.
fn remote_config(cfg: &NodeConfig, target: std::net::SocketAddr) -> Option<pm_api::remote::RemoteConfig> {
    if !cfg.remote.enabled {
        return None;
    }
    let relays = match cfg.remote.relay.trim() {
        "" => pm_remote::Relays::Public,
        "none" => pm_remote::Relays::Off,
        url => match url.parse() {
            Ok(u) => pm_remote::Relays::Custom(u),
            Err(e) => {
                tracing::warn!(relay = url, "remote access off: bad [remote] relay: {e}");
                return None;
            }
        },
    };
    Some(pm_api::remote::RemoteConfig {
        key_path: cfg.data_dir.join("remote.key"),
        relays,
        target,
    })
}

/// Announce the node on the LAN unless it only listens on loopback (or `[api] announce = false`).
fn announce(cfg: &NodeConfig, state: &pm_api::NodeState) -> Option<pm_discovery::Advertiser> {
    if !cfg.api.announce {
        return None;
    }
    let addr: std::net::SocketAddr = match cfg.api.listen.parse() {
        Ok(a) => a,
        Err(_) => {
            tracing::info!(listen = %cfg.api.listen, "not announcing on the LAN: listen is not an IP:port");
            return None;
        }
    };
    if addr.ip().is_loopback() {
        tracing::info!("not announcing on the LAN: the API listens on loopback only");
        return None;
    }
    Some(pm_discovery::Advertiser::start(state.announcement(addr.port())))
}

/// The zones as UPnP/DLNA renderers (`[renderer] enabled`); announced over SSDP unless the API
/// listens on loopback only.
fn renderers(
    cfg: &NodeConfig,
    state: &std::sync::Arc<pm_api::NodeState>,
) -> Option<pm_renderer::ssdp::Advertiser> {
    if !cfg.renderer.enabled {
        return None;
    }
    let addr: std::net::SocketAddr = cfg.api.listen.parse().ok()?;
    state.start_renderer(addr.port(), !addr.ip().is_loopback())
}

/// `[api] public_url`, or one devices can reach when it is empty (the log says which), and
/// whether it was guessed.
fn public_url(cfg: &NodeConfig) -> (String, bool) {
    if !cfg.api.public_url.is_empty() {
        return (cfg.api.public_url.clone(), false);
    }
    let url = config::default_public_url(&cfg.api.listen, config::route_address());
    if url.contains("0.0.0.0") || url.contains("[::]") {
        tracing::warn!(%url, "public_url is not set and this machine has no network route: speakers cannot fetch from the node; set [api] public_url (PM_PUBLIC_URL)");
    } else {
        tracing::info!(%url, "public_url is not set: speakers fetch music from this address (set [api] public_url to change it)");
    }
    (url, true)
}

fn session_secret_path(cfg: &NodeConfig) -> PathBuf {
    cfg.pubky
        .session_secret_file
        .clone()
        .unwrap_or_else(|| cfg.data_dir.join("pubky-session.secret"))
}

async fn login(cfg: &NodeConfig) -> anyhow::Result<()> {
    let hs = pm_pubky::homeserver::NodeHomeserver::new(cfg.pubky.network)?;
    let flow = hs.start_login("pubky-music-node")?;
    println!("Open Pubky Ring and scan this code, or paste the URL:\n");
    if let Ok(code) = qrcode::QrCode::new(flow.url().as_bytes()) {
        let text = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Light)
            .light_color(qrcode::render::unicode::Dense1x2::Dark)
            .build();
        println!("{text}\n");
    }
    println!("{}\n", flow.url());
    println!("waiting for approval…");
    let session = flow.await_approval().await?;
    let z32 = session.public_key().to_string();
    let secret = pm_pubky::homeserver::export_session_secret(&session)
        .await
        .ok_or_else(|| anyhow::anyhow!("the session cannot be exported; sign in again"))?;
    let path = session_secret_path(cfg);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Created 0600, never written first and made private after
    pm_api::imports::write_private(&path, secret.as_bytes())?;
    println!("signed in as {z32}");
    println!("session secret stored in {}", path.display());
    if cfg.pubky.session_secret_file.is_none() {
        println!(
            "add to the config:\n  [pubky]\n  session_secret_file = \"{}\"",
            path.display()
        );
    }
    Ok(())
}

async fn upload(cfg: &NodeConfig, files: &[PathBuf]) -> anyhow::Result<()> {
    let file = session_secret_path(cfg);
    let secret = std::fs::read_to_string(&file).map_err(|e| {
        anyhow::anyhow!(
            "no session secret at {} ({e}); run `pm-node login`",
            file.display()
        )
    })?;
    let sync = pm_api::sync::PubkySync::connect(cfg.pubky.network, &secret).await?;
    println!("signed in as {}", sync.z32());
    for f in files {
        anyhow::ensure!(
            pm_library::tags::is_supported(f),
            "{} is not a supported audio file",
            f.display()
        );
        let bytes = std::fs::read(f)?;
        let remote = format!(
            "{}{}",
            pm_pubky::homeserver::PRIVATE_FILES_DIR,
            pm_api::files::remote_name(&bytes, f)
        );
        let size = bytes.len();
        sync.upload_private(&remote, bytes).await?;
        println!("{} -> {remote} ({size} bytes)", f.display());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pm_out_local::quiet_alsa_lib();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let mut cfg = NodeConfig::load_or_default(cli.config.as_deref())?;
    let took = cfg.fill_from_restored();
    if !took.is_empty() {
        tracing::info!(?took, "service keys from the restored config");
    }
    let cfg = cfg;
    let config_file = cli.config.clone();
    match cli.cmd {
        Cmd::Config => print!("{}", toml::to_string_pretty(&cfg)?),
        Cmd::Serve => serve(&cfg, config_file).await?,
        Cmd::Outputs => {
            let registry = outputs::discover_all(&cfg.outputs);
            for o in registry.list() {
                println!("{}\t{}\t{}", o.id, o.name, serde_json::to_string(&o.caps)?);
            }
        }
        Cmd::Info { file } => play::info(&file)?,
        Cmd::Play {
            files,
            to,
            volume_db,
            headroom_db,
        } => {
            let registry = outputs::discover();
            let output = play::resolve_output(&registry, &to)?;
            let pcfg = pm_audio::PipelineConfig {
                volume_db,
                headroom_db,
                ..Default::default()
            };
            play::play(&files, output, pcfg).await?;
        }
        Cmd::Volume { to, db } => {
            let registry = outputs::discover();
            let output = play::resolve_output(&registry, &to)?;
            output.set_volume(pm_model::Volume::Db(db)).await?;
            println!("{to}: volume {db:+.1} dB");
        }
        Cmd::Scan { folders } => library::scan(&cfg, &folders)?,
        Cmd::Upload { files } => upload(&cfg, &files).await?,
        Cmd::Login => login(&cfg).await?,
        Cmd::Stats => library::stats(&cfg)?,
        Cmd::Backup { file, force } => library::backup(&cfg, &file, force)?,
        Cmd::Restore { file } => library::restore(&cfg, &file)?,
        Cmd::ImportPlaylist { file, name } => library::import_playlist(&cfg, &file, name.as_deref())?,
        Cmd::Tracks { limit } => library::tracks(&cfg, limit)?,
        Cmd::Search { query, limit } => library::search(&cfg, &query, limit)?,
        Cmd::Resolve { input } => library::resolve(&cfg, &input).await?,
        Cmd::Uri { input } => social::uri(&input)?,
        Cmd::Pinakes { cmd } => {
            let pc = cfg.pubky_config()?;
            match cmd {
                PinakesCmd::Info => social::pinakes_info(&pc).await?,
                PinakesCmd::Lookup { uris } => social::pinakes_lookup(&pc, &uris).await?,
                PinakesCmd::Search { tags, limit } => social::pinakes_search(&pc, &tags, limit).await?,
            }
        }
        Cmd::Discover { cmd } => {
            let pc = cfg.pubky_config()?;
            match cmd {
                DiscoverCmd::Trends { limit, window } => social::discover_trends(&pc, limit, &window).await?,
                DiscoverCmd::Key { key } => social::discover_key(&pc, &key).await?,
                DiscoverCmd::ByTag { label, limit } => social::discover_by_tag(&pc, &label, limit).await?,
                DiscoverCmd::Friends { viewer, limit } => {
                    social::discover_friends(&pc, &viewer, limit).await?
                }
            }
        }
    }
    Ok(())
}

/// An environment variable when set and not blank, else the config value.
fn env_or(var: &str, config: &str) -> String {
    std::env::var(var)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| config.to_string())
}
