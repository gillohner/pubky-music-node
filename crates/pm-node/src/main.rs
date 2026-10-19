//! `pm-node`: the Pubky Music Node binary.
//!
//! For now a CLI over the core services; the long-running daemon (API,
//! zones, homeserver embedding) arrives with M3.

mod config;
mod outputs;

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
    }
    Ok(())
}
