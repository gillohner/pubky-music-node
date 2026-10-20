//! `pm-node`: the Pubky Music Node binary.
//!
//! For now a CLI over the core services; the long-running daemon (API,
//! zones, homeserver embedding) arrives with M3.

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
    /// Upload audio files to the user's private homeserver storage
    /// (`pubky.session_secret_file` must be set).
    Upload {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Library counts.
    Stats,
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

async fn serve(cfg: &NodeConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let db_path = cfg.db_path();
    let lib = pm_library::Library::open(&db_path)?;
    let registry = outputs::discover_all(&cfg.outputs);
    let token = (!cfg.api.token.is_empty()).then(|| cfg.api.token.clone());
    if token.is_none() && !cfg.api.listen.starts_with("127.") && !cfg.api.listen.starts_with("localhost") {
        tracing::warn!(listen = %cfg.api.listen, "API has no token and is not bound to localhost");
    }
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
            public_url: if cfg.api.public_url.is_empty() {
                format!("http://{}", cfg.api.listen)
            } else {
                cfg.api.public_url.clone()
            },
            upnp: cfg.outputs.upnp,
        },
        lib,
        registry,
    );
    state.zones_for_all_outputs();
    {
        let st = state.clone();
        let found =
            tokio::task::spawn_blocking(move || st.discover_upnp(std::time::Duration::from_secs(3))).await?;
        if !found.is_empty() {
            tracing::info!(count = found.len(), "UPnP renderers found");
        }
    }
    for z in state.zones.list() {
        tracing::info!(zone = %z.id, output = %z.output, "zone ready");
    }
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
    if let Some(file) = &cfg.pubky.session_secret_file {
        match std::fs::read_to_string(file) {
            Ok(secret) => match pm_api::sync::PubkySync::connect(cfg.pubky.network, &secret).await {
                Ok(sync) => {
                    tracing::info!(user = %sync.z32(), "Pubky session restored");
                    let sync = std::sync::Arc::new(sync);
                    state.set_sync(Some(sync.clone()));
                    match sync.sync(&state.library).await {
                        Ok(r) => tracing::info!(
                            pushed = r.pushed,
                            pulled = r.pulled,
                            deleted = r.deleted,
                            errors = r.errors.len(),
                            "initial sync"
                        ),
                        Err(e) => tracing::warn!("initial sync failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!(file = %file.display(), "cannot restore Pubky session: {e}"),
            },
            Err(e) => tracing::warn!(file = %file.display(), "cannot read session secret: {e}"),
        }
    }
    pm_api::serve(state, &cfg.api.listen).await
}

async fn upload(cfg: &NodeConfig, files: &[PathBuf]) -> anyhow::Result<()> {
    let file = cfg
        .pubky
        .session_secret_file
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("set `pubky.session_secret_file` in the node config"))?;
    let secret = std::fs::read_to_string(file)?;
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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let cfg = NodeConfig::load_or_default(cli.config.as_deref())?;
    match cli.cmd {
        Cmd::Config => print!("{}", toml::to_string_pretty(&cfg)?),
        Cmd::Serve => serve(&cfg).await?,
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
        Cmd::Stats => library::stats(&cfg)?,
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
