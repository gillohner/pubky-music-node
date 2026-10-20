//! Automatic library pickup: watch the configured folders and rescan after
//! a quiet period, so anything another app drops into a shared folder (a
//! download client on Umbrel, a NAS sync) shows up without a manual scan.
//!
//! The debounce matters: a file being written changes size repeatedly; a
//! scan in the middle would index half a file or fail to probe it. Waiting
//! until no event arrived for `quiet` avoids most of that, and the scanner's
//! size+mtime check re-indexes anything that changed afterwards anyway.

use crate::state::NodeState;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Start watching `folders`; the returned watcher must be kept alive.
pub fn spawn(
    state: Arc<NodeState>,
    folders: Vec<PathBuf>,
    quiet: Duration,
) -> notify::Result<RecommendedWatcher> {
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
        Ok(ev) if ev.kind.is_create() || ev.kind.is_modify() || ev.kind.is_remove() => {
            let _ = tx.send(());
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("folder watch: {e}"),
    })?;
    for f in &folders {
        match watcher.watch(f, RecursiveMode::Recursive) {
            Ok(()) => tracing::info!(folder = %f.display(), "watching for changes"),
            Err(e) => tracing::warn!(folder = %f.display(), "cannot watch: {e}"),
        }
    }
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            // Drain until quiet.
            loop {
                match tokio::time::timeout(quiet, rx.recv()).await {
                    Ok(Some(())) => continue,
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            match state.start_scan(folders.clone()) {
                Ok(true) => tracing::info!("folder change: rescanning"),
                Ok(false) => tracing::debug!("folder change: scan already running"),
                Err(e) => tracing::warn!("folder change: {e}"),
            }
        }
    });
    Ok(watcher)
}
