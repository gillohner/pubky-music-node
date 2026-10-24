//! Measured loudness for library files without ReplayGain tags (compare-music-assistant #7).
//!
//! While a zone has loudness on (Settings › Playback › DSP, or the node's default), the node
//! measures those files in the background, one at a time (EBU R128, `pm_audio::loudness`), and
//! the ReplayGain stage uses the result where a tag is missing (`NodeSources::open`). With
//! loudness off on every zone, nothing is measured and nothing changes.
//!
//! - `GET /v1/library/loudness`: `{on, measured, failed, to_measure, running}`.
//! - `POST /v1/library/loudness/measure`: start a pass now, even with loudness off.

use crate::state::NodeState;
use crate::ApiResult;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(false);
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();
/// How often the background pass looks for new files while loudness is on.
const EVERY: Duration = Duration::from_secs(10 * 60);
const BATCH: u32 = 50;

/// Measure up to `limit` files that need it; returns how many were measured (or marked as not
/// measurable). Blocking work runs off the async runtime.
pub async fn measure_pending(s: &Arc<NodeState>, limit: u32) -> usize {
    let todo = s
        .library
        .lock()
        .expect("library poisoned")
        .tracks_needing_loudness(limit)
        .unwrap_or_default();
    let mut done = 0;
    for (track, path, size, mtime) in todo {
        let p = path.clone();
        let r = tokio::task::spawn_blocking(move || pm_audio::loudness::measure(&p)).await;
        let (lufs, peak) = match r {
            Ok(Ok(m)) => (m.lufs, m.peak),
            Ok(Err(e)) => {
                tracing::debug!(file = %path.display(), "loudness not measured: {e}");
                (0.0, -1.0)
            }
            Err(_) => continue,
        };
        if s.library
            .lock()
            .expect("library poisoned")
            .set_loudness(track, lufs, peak, size, mtime)
            .is_ok()
        {
            done += 1;
        }
        // Leave the CPU to playback and scans between files
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done
}

/// One pass over everything still to measure.
async fn pass(s: &Arc<NodeState>) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut total = 0;
    loop {
        let n = measure_pending(s, BATCH).await;
        total += n;
        if n == 0 {
            break;
        }
    }
    RUNNING.store(false, Ordering::SeqCst);
    if total > 0 {
        tracing::info!(files = total, "loudness measured");
    }
}

/// The background measurer: a pass every few minutes while a zone has loudness on, and at once
/// when asked (`POST /v1/library/loudness/measure`).
pub fn spawn(s: &Arc<NodeState>) {
    let s = s.clone();
    tokio::spawn(async move {
        loop {
            let forced = tokio::time::timeout(EVERY, WAKE.notified()).await.is_ok();
            if forced || s.sources.loudness_on() {
                pass(&s).await;
            }
        }
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoudnessStatus {
    /// A zone has loudness on, so new files are measured by themselves.
    pub on: bool,
    pub measured: u64,
    /// Files that couldn't be decoded or are silent; not tried again until they change.
    pub failed: u64,
    pub to_measure: u64,
    pub running: bool,
}

fn status(s: &NodeState) -> LoudnessStatus {
    let (measured, failed, to_measure) = s
        .library
        .lock()
        .expect("library poisoned")
        .loudness_counts()
        .unwrap_or_default();
    LoudnessStatus {
        on: s.sources.loudness_on(),
        measured,
        failed,
        to_measure,
        running: RUNNING.load(Ordering::SeqCst),
    }
}

/// `GET /v1/library/loudness`.
pub(crate) async fn get(State(s): State<Arc<NodeState>>) -> ApiResult<LoudnessStatus> {
    Ok(Json(status(&s)))
}

/// `POST /v1/library/loudness/measure`: start a pass now.
pub(crate) async fn measure(State(s): State<Arc<NodeState>>) -> ApiResult<LoudnessStatus> {
    WAKE.notify_one();
    Ok(Json(status(&s)))
}
