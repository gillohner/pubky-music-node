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
    /// List the outputs this node can play to.
    Outputs,
    /// Inspect an audio file: container, format, duration.
    Info { file: PathBuf },
    /// Play a file to an output and print the signal path.
    Play {
        file: PathBuf,
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
    /// Scan folders into the library (configured folders when none given).
    Scan { folders: Vec<PathBuf> },
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
        Cmd::Outputs => {
            let registry = outputs::discover();
            for o in registry.list() {
                println!("{}\t{}\t{}", o.id, o.name, serde_json::to_string(&o.caps)?);
            }
        }
        Cmd::Info { file } => play::info(&file)?,
        Cmd::Play {
            file,
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
            play::play(&file, output, pcfg).await?;
        }
        Cmd::Scan { folders } => library::scan(&cfg, &folders)?,
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
