//! `pm-endpoint`: serve this machine's outputs to a Pubky Music Node.
//!
//! ```sh
//! pm-endpoint --name "Kitchen Pi"              # listens on 0.0.0.0:7790, advertises over mDNS
//! pm-endpoint --listen 192.168.1.20:7790 --no-mdns
//! ```

use clap::Parser;
use std::net::TcpListener;

#[derive(Parser)]
#[command(name = "pm-endpoint", version, about = "Pubky Music endpoint daemon")]
struct Cli {
    /// Name shown in the node's output list.
    #[arg(long, default_value_t = default_name())]
    name: String,
    /// Address to listen on.
    #[arg(long, default_value_t = format!("0.0.0.0:{}", pm_stream::DEFAULT_PORT))]
    listen: String,
    /// Do not advertise over mDNS (configure the node with the address instead).
    #[arg(long)]
    no_mdns: bool,
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
    let listener = TcpListener::bind(&cli.listen)?;
    let port = listener.local_addr()?.port();
    tracing::info!(listen = %cli.listen, name = %cli.name, outputs = outputs.len(), "endpoint ready");

    let _mdns = if cli.no_mdns {
        None
    } else {
        match advertise(&cli.name, port) {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!("mDNS advertising failed ({e}); the node needs the address configured");
                None
            }
        }
    };
    let endpoint = pm_endpoint::Endpoint::new(&cli.name, outputs);
    endpoint.serve(listener)?;
    Ok(())
}

/// Register `_pubky-music._tcp` with the name and protocol version as TXT records.
fn advertise(name: &str, port: u16) -> anyhow::Result<mdns_sd::ServiceDaemon> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let host = format!("{}.local.", hostname_label());
    let instance = name.replace(['.', '\n'], " ");
    let props = [
        ("name", name.to_string()),
        ("version", pm_stream::PROTOCOL_VERSION.to_string()),
    ];
    let info = mdns_sd::ServiceInfo::new(pm_stream::SERVICE_TYPE, &instance, &host, "", port, &props[..])?
        .enable_addr_auto();
    daemon.register(info)?;
    tracing::info!(service = pm_stream::SERVICE_TYPE, %instance, port, "advertising over mDNS");
    Ok(daemon)
}

fn hostname_label() -> String {
    default_name()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
        .to_lowercase()
}
