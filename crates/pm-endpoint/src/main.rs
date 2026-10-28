//! `pm-endpoint`: the desktop app's player without the app, for tests and development. It answers
//! only a node that shows its key, so register it with the node, key and all
//! (`POST /v1/outputs/endpoints {"address": "<ip>:7790", "key": "<the key file's contents>"}`).
//! Speakers on a Raspberry Pi or another computer with a DAC run squeezelite instead.
//!
//! ```sh
//! pm-endpoint --key-file ~/.config/pubky-music/endpoint.key     # made (0600) on first start
//! pm-endpoint --listen 127.0.0.1:7790 --key "$(cat endpoint.key)"
//! ```

use clap::Parser;
use std::io::{ErrorKind, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "pm-endpoint",
    version,
    about = "Pubky Music endpoint (the app's player, headless)"
)]
struct Cli {
    /// Name shown in the node's output list.
    #[arg(long, default_value_t = default_name())]
    name: String,
    /// Address to listen on.
    #[arg(long, default_value_t = format!("0.0.0.0:{}", pm_stream::DEFAULT_PORT))]
    listen: String,
    /// File with the key the node must show; a new key is made there (0600) when it is missing.
    #[arg(long, conflicts_with = "key")]
    key_file: Option<PathBuf>,
    /// The key itself (other users of this machine can see a command line; prefer --key-file).
    #[arg(long)]
    key: Option<String>,
    /// List the outputs this endpoint would offer and exit.
    #[arg(long)]
    outputs: bool,
}

fn default_name() -> String {
    pm_endpoint::machine_name()
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let outputs = pm_endpoint::local_outputs();
    if cli.outputs {
        for o in outputs.list() {
            println!("{}\t{}\t{}", o.id, o.name, serde_json::to_string(&o.caps)?);
        }
        return Ok(());
    }
    let key = match (&cli.key, &cli.key_file) {
        (Some(k), _) => pm_stream::Key::parse(k).map_err(anyhow::Error::msg)?,
        (None, Some(path)) => load_or_make(path)?,
        (None, None) => {
            anyhow::bail!("an endpoint needs a key: --key-file PATH (made on first start) or --key")
        }
    };
    let listener = TcpListener::bind(&cli.listen)?;
    tracing::info!(listen = %cli.listen, name = %cli.name, outputs = outputs.len(), "endpoint ready");
    let endpoint = pm_endpoint::Endpoint::new(&cli.name, outputs, key);
    endpoint.serve(listener)?;
    Ok(())
}

/// The key in `path`, or a new one written there readable by this user only.
fn load_or_make(path: &Path) -> anyhow::Result<pm_stream::Key> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
                    tracing::warn!(path = %path.display(), "the key file was readable by others; now 0600");
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                }
            }
            pm_stream::Key::parse(&s)
                .map_err(|e| anyhow::anyhow!("{}: {e} (delete it to make a new key)", path.display()))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let key = pm_stream::Key::random();
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            let mut f = o.open(path)?;
            f.write_all(key.expose().as_bytes())?;
            f.sync_all()?;
            tracing::info!(path = %path.display(), "made a new endpoint key; register it with the node together with the address");
            Ok(key)
        }
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}
