//! Try the tunnel by hand (ADR 0013).
//!
//! ```sh
//! cargo run -p pm-remote --example tunnel -- serve 127.0.0.1:7788   # prints the node key
//! cargo run -p pm-remote --example tunnel -- dial <key>             # prints http://127.0.0.1:<port>
//! ```
//! Both use n0's public relays and address lookup, so they find each other by key alone.

use pm_remote::{ClientTunnel, NodeTunnel, Relays, SecretKey};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => {
            let target = args
                .get(1)
                .map(String::as_str)
                .unwrap_or("127.0.0.1:7788")
                .parse()?;
            let t = NodeTunnel::start(SecretKey::generate(), &Relays::Public, target).await?;
            let online = t.online(Duration::from_secs(10)).await;
            println!("node key: {}  (relay connected: {online})", t.id());
            tokio::signal::ctrl_c().await?;
            t.close().await;
        }
        Some("dial") => {
            let id: pm_remote::EndpointId = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("dial <key>"))?
                .parse()?;
            let t = ClientTunnel::start(id, &Relays::Public).await?;
            let started = std::time::Instant::now();
            let route = t.connect().await?;
            println!(
                "http://{}  ({route:?}, connected in {:?})",
                t.local_addr(),
                started.elapsed()
            );
            tokio::signal::ctrl_c().await?;
            t.close().await;
        }
        _ => eprintln!("usage: tunnel serve <host:port> | tunnel dial <key>"),
    }
    Ok(())
}
