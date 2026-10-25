//! What a device keeps, on the device: `index.json` (the node's own answers for the kept songs,
//! playlists and favourites), the song files, the album covers, the web UI, and the queue of
//! plays and favourites made offline (`queue.jsonl`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// What a device keeps offline, as `PUT /v1/devices/{id}/downloads {keep}` takes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Keep {
    #[serde(default)]
    pub tracks: Vec<i64>,
    #[serde(default)]
    pub albums: Vec<i64>,
    #[serde(default)]
    pub playlists: Vec<String>,
    /// The favourite songs and albums (the owner's devices).
    #[serde(default)]
    pub favourites: bool,
    /// The copy to keep: the original when absent, else a smaller one the node makes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
}

/// A smaller copy: `aac`, `mp3` or `opus`, at `bitrate` kbit/s (the node's default when absent).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Format {
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
}

impl Format {
    /// The file extension a copy in this format gets.
    pub fn ext(&self) -> &'static str {
        match self.format.as_str() {
            "aac" => "m4a",
            "opus" => "ogg",
            _ => "mp3",
        }
    }
}

/// A song the device has.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeptTrack {
    /// The node's answer for the song (`GET /v1/library/tracks/{id}`), its file path shortened to
    /// the name.
    pub track: Value,
    /// The file's name under `files/`.
    pub file: String,
    pub bytes: u64,
    /// `original`, `aac`, `mp3` or `opus`.
    pub format: String,
}

/// `index.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default = "version")]
    pub version: u32,
    /// The id this install registered under (`PUT /v1/devices/{id}`), made once.
    #[serde(default)]
    pub device_id: String,
    /// The node's `/v1/info` at the last sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Value>,
    /// The node's `/v1/me` at the last sync (owner or listener).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub me: Option<Value>,
    #[serde(default)]
    pub keep: Keep,
    #[serde(default)]
    pub tracks: BTreeMap<i64, KeptTrack>,
    /// Album id → the cover's file name under `art/`.
    #[serde(default)]
    pub covers: BTreeMap<i64, String>,
    /// Kept playlists: the node's answer for each (`GET /v1/playlists/{id}`).
    #[serde(default)]
    pub playlists: BTreeMap<String, Value>,
    /// The node's favourites (`GET /v1/favourites`), when they are kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favourites: Option<Value>,
    /// Hearts set or cleared offline and not yet sent: song id → favourite.
    #[serde(default)]
    pub favourite_changes: BTreeMap<i64, bool>,
    /// The last sync that finished (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<i64>,
}

fn version() -> u32 {
    1
}

impl Index {
    /// Bytes of the songs the device has.
    pub fn bytes(&self) -> u64 {
        self.tracks.values().map(|t| t.bytes).sum()
    }

    /// Whether a song is a favourite here: an offline change first, then the node's list.
    pub fn is_favourite(&self, track_id: i64) -> bool {
        if let Some(on) = self.favourite_changes.get(&track_id) {
            return *on;
        }
        self.favourites
            .as_ref()
            .and_then(|f| f.get("tracks"))
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter()
                    .any(|t| t.get("id").and_then(Value::as_i64) == Some(track_id))
            })
    }
}

/// One thing done offline, sent to the node when it is back (`queue.jsonl`, one per line).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// A song played to the end or most of the way (as the web UI reports plays).
    Play { track_id: i64, at: i64 },
    /// A heart set (`on`) or cleared.
    Favourite { track_id: i64, on: bool, at: i64 },
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A device's offline store, in one folder.
pub struct Store {
    dir: PathBuf,
    index: Mutex<Index>,
    /// Held while the queue file is read and rewritten.
    queue: Mutex<()>,
}

impl Store {
    /// Open (or make) the store in `dir`. A new store gets a device id.
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        for sub in ["files", "art", "ui"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        let mut index = match std::fs::read(dir.join("index.json")) {
            Ok(b) => serde_json::from_slice::<Index>(&b).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("index.json: {e}"))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Index::default(),
            Err(e) => return Err(e),
        };
        index.version = version();
        if index.device_id.is_empty() {
            let mut b = [0u8; 16];
            getrandom::fill(&mut b).map_err(|e| std::io::Error::other(e.to_string()))?;
            index.device_id = b.iter().map(|x| format!("{x:02x}")).collect();
        }
        let store = Self {
            dir,
            index: Mutex::new(index),
            queue: Mutex::new(()),
        };
        store.save(&store.lock())?;
        Ok(store)
    }

    fn lock(&self) -> MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, ix: &Index) -> std::io::Result<()> {
        let tmp = self.dir.join("index.json.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(ix).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        std::fs::rename(tmp, self.dir.join("index.json"))
    }

    /// The index as it is now.
    pub fn index(&self) -> Index {
        self.lock().clone()
    }

    /// Change the index and save it.
    pub fn update<T>(&self, f: impl FnOnce(&mut Index) -> T) -> std::io::Result<T> {
        let mut ix = self.lock();
        let out = f(&mut ix);
        self.save(&ix)?;
        Ok(out)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    pub(crate) fn art_dir(&self) -> PathBuf {
        self.dir.join("art")
    }

    /// The cached web UI (index.html, app.js, …), refreshed at every sync.
    pub fn ui_dir(&self) -> PathBuf {
        self.dir.join("ui")
    }

    /// A kept song's file.
    pub fn track_file(&self, track_id: i64) -> Option<PathBuf> {
        let name = self.lock().tracks.get(&track_id)?.file.clone();
        Some(self.files_dir().join(name))
    }

    /// An album's cover file.
    pub fn cover_file(&self, album_id: i64) -> Option<PathBuf> {
        let name = self.lock().covers.get(&album_id)?.clone();
        Some(self.art_dir().join(name))
    }

    /// Choose what to keep (sent to the node at the next sync).
    pub fn set_keep(&self, keep: Keep) -> std::io::Result<()> {
        self.update(|ix| ix.keep = keep)
    }

    fn queue_path(&self) -> PathBuf {
        self.dir.join("queue.jsonl")
    }

    /// Note something done offline. A heart also shows at once in the offline index.
    pub fn push(&self, e: Event) -> std::io::Result<()> {
        if let Event::Favourite { track_id, on, .. } = e {
            self.update(|ix| {
                ix.favourite_changes.insert(track_id, on);
            })?;
        }
        let _q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.queue_path())?;
        let mut line = serde_json::to_vec(&e).map_err(std::io::Error::other)?;
        line.push(b'\n');
        f.write_all(&line)?;
        f.sync_all()
    }

    /// What waits to be sent, oldest first. Lines that can't be read are skipped.
    pub fn queued(&self) -> Vec<Event> {
        let _q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        self.read_queue()
    }

    fn read_queue(&self) -> Vec<Event> {
        std::fs::read_to_string(self.queue_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Drop the first `n` events (sent, or refused for good); events queued meanwhile stay.
    pub(crate) fn drop_sent(&self, n: usize) -> std::io::Result<()> {
        if n == 0 {
            return Ok(());
        }
        let _q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let rest: Vec<Event> = self.read_queue().into_iter().skip(n).collect();
        let tmp = self.dir.join("queue.jsonl.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        for e in &rest {
            let mut line = serde_json::to_vec(e).map_err(std::io::Error::other)?;
            line.push(b'\n');
            f.write_all(&line)?;
        }
        f.sync_all()?;
        std::fs::rename(tmp, self.queue_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_keeps_its_device_id_and_index() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let id = s.index().device_id;
        assert_eq!(id.len(), 32);
        s.set_keep(Keep {
            albums: vec![3],
            favourites: true,
            ..Default::default()
        })
        .unwrap();
        drop(s);
        let s = Store::open(d.path()).unwrap();
        assert_eq!(s.index().device_id, id, "the same id on every start");
        assert_eq!(s.index().keep.albums, [3]);
    }

    #[test]
    fn the_queue_keeps_what_was_queued_while_sending() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        s.push(Event::Play { track_id: 1, at: 10 }).unwrap();
        s.push(Event::Favourite {
            track_id: 2,
            on: true,
            at: 11,
        })
        .unwrap();
        assert!(s.index().is_favourite(2), "a heart shows at once");
        let sending = s.queued();
        assert_eq!(sending.len(), 2);
        // Played while the first two were on their way
        s.push(Event::Play { track_id: 3, at: 12 }).unwrap();
        s.drop_sent(sending.len()).unwrap();
        assert_eq!(s.queued(), [Event::Play { track_id: 3, at: 12 }]);
    }
}
