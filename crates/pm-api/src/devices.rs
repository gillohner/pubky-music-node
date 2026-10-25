//! A person's phones and laptops, and what each keeps offline (STEWARD-DOWNLOADS.md).
//!
//! An app install registers itself once under an id it picks (a UUID, or its vault key) and tells
//! the node what it keeps: songs, albums, playlists and the favourites marked "keep offline", and
//! the files it has. The node answers with what it still `wants` (a kept playlist's new songs, an
//! album added to the favourites) and shows each device's storage. The files come from
//! `GET /v1/library/tracks/{id}/download`.
//!
//! Devices belong to whoever registered them: the owner sees and manages theirs, a listener only
//! their own. Playlists and favourites are the owner's, so only the owner's devices keep them.

use crate::accounts::{Caller, Role};
use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use pm_model::TrackId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;

const PREFIX: &str = "device.";
/// Most devices one person registers.
const MAX_DEVICES: usize = 32;
/// Most songs one manifest lists.
const MAX_TRACKS: usize = 100_000;

/// An id an app picks once: 8–64 of `A-Z a-z 0-9 - _`.
fn valid_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What a device keeps offline.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Keep {
    #[serde(default)]
    pub tracks: Vec<i64>,
    #[serde(default)]
    pub albums: Vec<i64>,
    /// Playlist ids: their new songs are wanted too (owner's devices).
    #[serde(default)]
    pub playlists: Vec<String>,
    /// The favourite songs and albums (owner's devices).
    #[serde(default)]
    pub favourites: bool,
}

/// A file a device has.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Have {
    pub track_id: i64,
    pub bytes: u64,
    /// `original`, `aac`, `mp3` or `opus`.
    #[serde(default = "original")]
    pub format: String,
}

fn original() -> String {
    "original".into()
}

/// A device as the node keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// `android`, `ios`, `desktop`, `browser`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `owner`, or the listener's key.
    owner: String,
    pub created_at: i64,
    pub last_seen: i64,
    #[serde(default)]
    pub keep: Keep,
    #[serde(default)]
    pub have: Vec<Have>,
}

/// A device in a list: no manifest, its storage.
#[derive(Debug, Serialize)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub last_seen: i64,
    /// Files it has, and their bytes.
    pub files: usize,
    pub bytes: u64,
}

fn who(c: &Caller) -> Result<String, ApiError> {
    match c.role {
        Role::Owner => Ok("owner".into()),
        Role::Listener => c
            .key
            .clone()
            .ok_or_else(|| ApiError::new(StatusCode::FORBIDDEN, "a listener without a key")),
        _ => Err(ApiError::new(StatusCode::FORBIDDEN, "guests have no devices")),
    }
}

fn load(s: &NodeState, id: &str) -> Result<Option<Device>, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    Ok(lib
        .get_setting(&format!("{PREFIX}{id}"))?
        .and_then(|v| serde_json::from_str(&v).ok()))
}

fn store(s: &NodeState, d: &Device) -> Result<(), ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    lib.set_setting(
        &format!("{PREFIX}{}", d.id),
        &serde_json::to_string(d).expect("device serializes"),
    )?;
    Ok(())
}

fn all(s: &NodeState) -> Result<Vec<Device>, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    Ok(lib
        .settings_with_prefix(PREFIX)?
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_str::<Device>(&v).ok())
        .collect())
}

/// The caller's device, or 404 (someone else's looks the same as none).
fn mine(s: &NodeState, c: &Caller, id: &str) -> Result<Device, ApiError> {
    let me = who(c)?;
    load(s, id)?
        .filter(|d| d.owner == me)
        .ok_or_else(|| ApiError::not_found(format!("no device {id}")))
}

fn row(d: &Device) -> DeviceRow {
    DeviceRow {
        id: d.id.clone(),
        name: d.name.clone(),
        kind: d.kind.clone(),
        last_seen: d.last_seen,
        files: d.have.len(),
        bytes: d.have.iter().map(|h| h.bytes).sum(),
    }
}

/// `GET /v1/devices`: the caller's devices and their storage.
pub(crate) async fn list(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
) -> ApiResult<Vec<DeviceRow>> {
    let me = who(&c)?;
    let mut out: Vec<DeviceRow> = all(&s)?.iter().filter(|d| d.owner == me).map(row).collect();
    out.sort_by_key(|d| std::cmp::Reverse(d.last_seen));
    Ok(Json(out))
}

/// `PUT /v1/devices/{id}`
#[derive(Debug, Deserialize)]
pub struct Register {
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
}

/// `PUT /v1/devices/{id} {name, kind?}`: register an app install, or rename it.
pub(crate) async fn register(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    Json(b): Json<Register>,
) -> ApiResult<DeviceRow> {
    if !valid_id(&id) {
        return Err(ApiError::bad_request(
            "a device id is 8–64 of A-Z, a-z, 0-9, - and _",
        ));
    }
    let name = b.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(ApiError::bad_request("a device name is 1–80 characters"));
    }
    if let Some(k) = &b.kind {
        if !["android", "ios", "desktop", "browser"].contains(&k.as_str()) {
            return Err(ApiError::bad_request("kind is android, ios, desktop or browser"));
        }
    }
    let me = who(&c)?;
    let now = pm_library::db::now_unix();
    let d = match load(&s, &id)? {
        Some(d) if d.owner != me => {
            // Someone else's id: the same answer as a clash, without saying whose
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "that device id is taken; pick another",
            ));
        }
        Some(mut d) => {
            d.name = name.to_string();
            d.kind = b.kind.clone();
            d.last_seen = now;
            d
        }
        None => {
            if all(&s)?.iter().filter(|d| d.owner == me).count() >= MAX_DEVICES {
                return Err(ApiError::bad_request(format!(
                    "at most {MAX_DEVICES} devices; forget one first"
                )));
            }
            Device {
                id: id.clone(),
                name: name.to_string(),
                kind: b.kind.clone(),
                owner: me,
                created_at: now,
                last_seen: now,
                keep: Keep::default(),
                have: Vec::new(),
            }
        }
    };
    store(&s, &d)?;
    Ok(Json(row(&d)))
}

/// `DELETE /v1/devices/{id}`: forget a device and its manifest (its files stay on it).
pub(crate) async fn forget(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let d = mine(&s, &c, &id)?;
    s.library
        .lock()
        .expect("library poisoned")
        .delete_setting(&format!("{PREFIX}{}", d.id))?;
    Ok(StatusCode::NO_CONTENT)
}

/// A song the device should download.
#[derive(Debug, Serialize)]
pub struct Wanted {
    pub track_id: i64,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// The original's size.
    pub bytes: u64,
}

/// `GET /v1/devices/{id}/downloads`.
#[derive(Debug, Serialize)]
pub struct Manifest {
    pub device: DeviceRow,
    pub keep: Keep,
    pub have: Vec<Have>,
    /// Kept songs it doesn't have yet.
    pub wanted: Vec<Wanted>,
    /// Files it has that nothing keeps any more (it may delete them).
    pub unkept: Vec<i64>,
}

/// Every song `keep` covers, as library ids (missing ones dropped), in order.
fn kept_tracks(s: &NodeState, keep: &Keep) -> Result<Vec<i64>, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let mut ids: Vec<i64> = Vec::new();
    ids.extend(keep.tracks.iter().copied());
    for a in &keep.albums {
        ids.extend(lib.album_tracks(*a)?.iter().filter_map(|t| t.id.map(|i| i.0)));
    }
    for p in &keep.playlists {
        // A playlist that was deleted keeps nothing
        if let Ok(t) = crate::playlists::library_track_ids(&lib, p) {
            ids.extend(t);
        }
    }
    if keep.favourites {
        let f = crate::favourites::collect(&lib)?;
        ids.extend(f.tracks.iter().filter_map(|t| t.id.map(|i| i.0)));
        for a in f.albums.iter().filter_map(|a| a.id) {
            ids.extend(lib.album_tracks(a)?.iter().filter_map(|t| t.id.map(|i| i.0)));
        }
    }
    let mut seen = BTreeSet::new();
    ids.retain(|i| seen.insert(*i));
    // Only songs the library still has
    ids.retain(|i| lib.get_track(TrackId(*i)).ok().flatten().is_some());
    Ok(ids)
}

fn manifest(s: &NodeState, d: &Device) -> Result<Manifest, ApiError> {
    let kept = kept_tracks(s, &d.keep)?;
    let have: BTreeSet<i64> = d.have.iter().map(|h| h.track_id).collect();
    let kept_set: BTreeSet<i64> = kept.iter().copied().collect();
    let lib = s.library.lock().expect("library poisoned");
    let wanted = kept
        .iter()
        .filter(|i| !have.contains(i))
        .filter_map(|i| lib.get_track(TrackId(*i)).ok().flatten())
        .map(|t| Wanted {
            track_id: t.id.map_or(0, |i| i.0),
            title: t.title.clone(),
            artist: t.artists.first().map(|a| a.name.clone()),
            album: t.album.as_ref().map(|a| a.title.clone()),
            bytes: t.file.size_bytes,
        })
        .collect();
    Ok(Manifest {
        device: row(d),
        keep: d.keep.clone(),
        have: d.have.clone(),
        wanted,
        unkept: have.difference(&kept_set).copied().collect(),
    })
}

/// `GET /v1/devices/{id}/downloads`: what the device keeps and has, and what it still wants.
pub(crate) async fn get_downloads(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Manifest> {
    let mut d = mine(&s, &c, &id)?;
    d.last_seen = pm_library::db::now_unix();
    store(&s, &d)?;
    Ok(Json(manifest(&s, &d)?))
}

/// `PUT /v1/devices/{id}/downloads {keep?, have?}`
#[derive(Debug, Deserialize)]
pub struct Update {
    #[serde(default)]
    pub keep: Option<Keep>,
    #[serde(default)]
    pub have: Option<Vec<Have>>,
}

/// `PUT /v1/devices/{id}/downloads {keep?, have?}`: the device says what it keeps and what it has
/// (each replaces the stored one when given). Answers like the GET.
pub(crate) async fn put_downloads(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    Json(b): Json<Update>,
) -> ApiResult<Manifest> {
    let mut d = mine(&s, &c, &id)?;
    if let Some(k) = b.keep {
        if c.role != Role::Owner && (k.favourites || !k.playlists.is_empty()) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Not for listeners: playlists and favourites are the owner's; keep songs and albums",
            ));
        }
        if k.tracks.len() + k.albums.len() > MAX_TRACKS {
            return Err(ApiError::bad_request(format!(
                "at most {MAX_TRACKS} songs and albums"
            )));
        }
        d.keep = k;
    }
    if let Some(mut h) = b.have {
        if h.len() > MAX_TRACKS {
            return Err(ApiError::bad_request(format!("at most {MAX_TRACKS} files")));
        }
        let mut seen = BTreeSet::new();
        h.retain(|x| seen.insert(x.track_id));
        d.have = h;
    }
    d.last_seen = pm_library::db::now_unix();
    store(&s, &d)?;
    Ok(Json(manifest(&s, &d)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids() {
        assert!(valid_id("2f6c1a9e-7d4b-4c55-9a1e-8b2c3d4e5f60"));
        assert!(valid_id("pixel8_install"));
        for bad in [
            "short",
            "has space here",
            "../../etc/passwd",
            &"a".repeat(65),
            "dot.ted.name",
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
    }
}
