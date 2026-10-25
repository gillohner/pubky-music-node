//! Try the offline store by hand:
//!
//! ```sh
//! PM_TOKEN=… cargo run -p pm-offline --example offline -- <dir> sync http://node:7788 [album ids…]
//! cargo run -p pm-offline --example offline -- <dir> serve
//! ```
//! `sync` keeps the given albums (if any) and syncs; `serve` runs the offline web UI and prints
//! its address.

use pm_offline::{Keep, Node, Store, SyncOptions};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(dir), Some(cmd)) = (args.first(), args.get(1)) else {
        eprintln!("usage: offline <dir> sync <node url> [album ids…] | offline <dir> serve");
        std::process::exit(2);
    };
    let store = Arc::new(Store::open(dir)?);
    match cmd.as_str() {
        "sync" => {
            let url = args.get(2).ok_or("the node's address")?;
            let albums: Vec<i64> = args[3..].iter().filter_map(|a| a.parse().ok()).collect();
            if !albums.is_empty() {
                let keep = store.index().keep;
                store.set_keep(Keep { albums, ..keep })?;
            }
            let token = std::env::var("PM_TOKEN").unwrap_or_default();
            let node = Node::new(url, &token)?;
            let opts = SyncOptions {
                name: "Example device".into(),
                kind: "desktop".into(),
            };
            let r = pm_offline::sync(&store, &node, &opts).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        "serve" => {
            let (addr, task) = pm_offline::server::serve(store).await?;
            println!("http://{addr}/");
            task.await?;
        }
        other => {
            eprintln!("unknown command {other}");
            std::process::exit(2);
        }
    }
    Ok(())
}
