//! `pm-node`: the Pubky Music Node binary.
//!
//! For now a CLI over the core services; the long-running daemon (API,
//! zones, homeserver embedding) arrives with M3.

mod config;
mod outputs;
mod play;

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
        Cmd::Play { file, to, volume_db, headroom_db } => {
            let registry = outputs::discover();
            let output = play::resolve_output(&registry, &to)?;
            let pcfg = pm_audio::PipelineConfig { volume_db, headroom_db, ..Default::default() };
            play::play(&file, output, pcfg).await?;
        }
    }
    Ok(())
}
