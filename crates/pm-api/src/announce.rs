//! Announcements on a zone (docs/compare-music-assistant.md, "port these" #5): a clip (any URL,
//! Home Assistant's TTS included) plays *instead of* the music for its length, with an optional
//! chime first and its own volume where the output has hardware volume; then the zone goes back to
//! where it was: the same track at the same position, playing or paused as before, the volume
//! restored. No mixing and no software gain: the zones stay bit-perfect, so the music pauses under
//! the announcement rather than ducking.
//!
//! The clip goes into the queue right after the current item and is removed again afterwards, so
//! the queue looks as it did. Announcement items carry no artist, so nothing scrobbles them.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use pm_model::Volume;
use pm_zones::{Insert, QueueItem, Transport, ZoneId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The chime: two short tones, written once into `<data_dir>/announce/chime.wav`.
const CHIME_RATE: u32 = 44_100;
/// How long a clip may run before the zone is given back anyway.
const MAX_CLIP: std::time::Duration = std::time::Duration::from_secs(120);

/// The last volume set through the API per zone (percent), so an announcement can put it back.
fn volumes() -> &'static Mutex<HashMap<String, u8>> {
    static V: std::sync::OnceLock<Mutex<HashMap<String, u8>>> = std::sync::OnceLock::new();
    V.get_or_init(Default::default)
}

/// Called by the volume route: remember what the zone was set to.
pub fn note_volume(zone: &ZoneId, v: &Volume) {
    if let Volume::Percent(p) = v {
        volumes()
            .lock()
            .expect("announce volumes")
            .insert(zone.0.clone(), *p);
    }
}

/// Zones with an announcement under way.
fn busy() -> &'static Mutex<std::collections::HashSet<String>> {
    static B: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    B.get_or_init(Default::default)
}

/// A 16-bit stereo WAV of two tones (G5 then C6, 0.25 s each), written by hand.
pub fn chime_wav() -> Vec<u8> {
    let frames = (CHIME_RATE / 2) as usize;
    let mut pcm: Vec<u8> = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let t = i as f64 / CHIME_RATE as f64;
        let f = if t < 0.25 { 783.99 } else { 1046.5 };
        // A quick fade at both ends of each tone, so nothing clicks
        let local = if t < 0.25 { t } else { t - 0.25 };
        let env = (local / 0.02)
            .min(1.0)
            .min(((0.25 - local) / 0.05).clamp(0.0, 1.0));
        let v = ((t * f * std::f64::consts::TAU).sin() * 9000.0 * env) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // channels
    out.extend_from_slice(&CHIME_RATE.to_le_bytes());
    out.extend_from_slice(&(CHIME_RATE * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(&pcm);
    out
}

fn chime_path(s: &NodeState) -> Result<std::path::PathBuf, ApiError> {
    let dir = s.data_dir().join("announce");
    let p = dir.join("chime.wav");
    if !p.is_file() {
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&p, chime_wav()))
            .map_err(|e| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("writing the chime: {e}"),
                )
            })?;
    }
    Ok(p)
}

#[derive(Debug, Deserialize, Default)]
pub struct AnnounceRequest {
    /// What to play: an `http(s)` URL (a TTS clip, a sound file) or a local path.
    #[serde(default)]
    pub uri: Option<String>,
    /// A short chime before it.
    #[serde(default)]
    pub chime: bool,
    /// The clip's volume, 0 to 100, on outputs with hardware volume; the zone's volume comes back
    /// afterwards.
    #[serde(default)]
    pub volume_percent: Option<u8>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Announced {
    /// The zone was playing or paused on a track; it goes back to it afterwards.
    pub resumes: bool,
    /// The clip's volume was set (the output has hardware volume and a volume was asked for).
    pub volume_applied: bool,
    /// The zone's volume to come back to, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restores_volume: Option<u8>,
    pub items: usize,
}

/// `POST /v1/zones/{id}/announce {uri?, chime?, volume_percent?, title?}`: play the clip now,
/// then go back to what was playing.
pub(crate) async fn announce(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    Json(req): Json<AnnounceRequest>,
) -> ApiResult<Announced> {
    let zone = s
        .zones
        .get(&ZoneId::new(&id))
        .ok_or_else(|| ApiError::not_found(format!("no zone {id}")))?;
    let uri = req.uri.as_deref().map(str::trim).filter(|u| !u.is_empty());
    if uri.is_none() && !req.chime {
        return Err(ApiError::bad_request("give a uri to play, or chime: true"));
    }
    if let Some(u) = uri {
        if !(u.starts_with("http://") || u.starts_with("https://") || u.starts_with('/')) {
            return Err(ApiError::bad_request("uri is an http(s) URL or a file path"));
        }
        // A listener's clip is a public web link, fetched only there (no files, no LAN)
        if caller.role != crate::accounts::Role::Owner {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                return Err(crate::accounts::refused(
                    "a listener announces a web link, not a file",
                ));
            }
            crate::accounts::check_listener_item(&mut QueueItem::new(u))?;
        }
    }
    if let Some(v) = req.volume_percent {
        if v > 100 {
            return Err(ApiError::bad_request("volume_percent is 0 to 100"));
        }
    }
    if !busy().lock().expect("announce busy").insert(id.clone()) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "an announcement is already playing on this zone",
        ));
    }
    let release = || {
        busy().lock().expect("announce busy").remove(&id);
    };
    // What to come back to
    let before = zone.state();
    let saved = before
        .now_playing
        .as_ref()
        .map(|np| (np.index, np.item.id, np.position_ms, before.transport));
    if before.transport == Transport::Playing {
        zone.pause();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), zone.wait_acked()).await;
    }
    // The volume for the clip
    let has_volume = zone.output().caps().hw_volume.is_some();
    let previous = volumes().lock().expect("announce volumes").get(&id).copied();
    let mut volume_applied = false;
    if let (Some(v), true) = (req.volume_percent, has_volume) {
        if zone.set_volume(Volume::Percent(v)).await.is_ok() {
            volume_applied = true;
        }
    }
    // The items, right after the current one
    let mut items = Vec::new();
    if req.chime {
        let p = match chime_path(&s) {
            Ok(p) => p,
            Err(e) => {
                release();
                return Err(e);
            }
        };
        let mut it = QueueItem::new(p.to_string_lossy().into_owned());
        it.title = Some("Chime".into());
        items.push(it);
    }
    if let Some(u) = uri {
        let mut it = QueueItem::new(u);
        it.title = Some(req.title.clone().unwrap_or_else(|| "Announcement".into()));
        if caller.role != crate::accounts::Role::Owner {
            it.limit = Some(pm_zones::FetchLimit::PUBLIC);
        }
        it.by = crate::accounts::Who::of(&caller).by();
        items.push(it);
    }
    let count = items.len();
    let ids = zone.enqueue(items, Insert::Next);
    let first = saved.map(|(i, _, _, _)| i + 1).unwrap_or(0);
    zone.play_index(first);
    let restore_to = volume_applied.then_some(previous).flatten();
    let out = Announced {
        resumes: saved.is_some(),
        volume_applied,
        restores_volume: restore_to,
        items: count,
    };
    let zone2 = zone.clone();
    let id2 = id.clone();
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let last = *ids.last().expect("at least one item");
        // Give the engine a moment to pick the first item up, then wait until the last clip is
        // no longer what plays (it ended, was skipped, or the zone stopped)
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        loop {
            let st = zone2.state();
            let on_clip = st
                .now_playing
                .as_ref()
                .is_some_and(|np| ids.contains(&np.item.id));
            if !on_clip
                && (st.transport != Transport::Playing
                    || st.now_playing.as_ref().is_some_and(|np| np.item.id != last))
            {
                break;
            }
            if st.transport == Transport::Stopped && st.now_playing.is_none() {
                break;
            }
            if start.elapsed() > MAX_CLIP {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // Back to where it was
        match saved {
            Some((index, item_id, position_ms, transport)) => {
                // The saved item sits at its old index again once the clips are gone; find it by id
                // first, in case the queue changed meanwhile
                for cid in &ids {
                    zone2.remove(*cid);
                }
                let q = zone2.queue();
                let idx = q
                    .iter()
                    .position(|it| it.id == item_id)
                    .unwrap_or(index.min(q.len().saturating_sub(1)));
                if q.is_empty() {
                    zone2.stop();
                } else {
                    zone2.play_index(idx);
                    if position_ms > 0 {
                        zone2.seek_ms(position_ms);
                    }
                    if transport != Transport::Playing {
                        zone2.pause();
                    }
                }
            }
            None => {
                zone2.stop();
                for cid in &ids {
                    zone2.remove(*cid);
                }
            }
        }
        if let Some(v) = restore_to {
            let _ = zone2.set_volume(Volume::Percent(v)).await;
        }
        busy().lock().expect("announce busy").remove(&id2);
    });
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chime_is_a_well_formed_wav() {
        let w = chime_wav();
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(w.len(), 44 + data_len);
        assert_eq!(data_len, (CHIME_RATE / 2) as usize * 4);
        // Starts and ends quiet (no click)
        assert_eq!(i16::from_le_bytes([w[44], w[45]]), 0);
        let last = i16::from_le_bytes([w[w.len() - 2], w[w.len() - 1]]);
        assert!(last.abs() < 200, "{last}");
    }
}
