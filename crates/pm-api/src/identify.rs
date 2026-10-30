//! The "Identify library" job (steward identify brief), a ladder per song, cheapest first: the
//! IDs in the tags (at scan time), the folder as an album (one MusicBrainz release for the whole
//! folder, see [`pm_library::album_fit`]), a Chromaprint fingerprint (cached by file key, decoded
//! on every core) looked up on AcoustID (3 requests a second) and decided folder by folder with
//! the folder as a release, and last a MusicBrainz search by the tags. What stays ambiguous, or
//! where two methods disagree, becomes suggestions for the owner; suggestions never become
//! identity on their own. The job resumes: tracks already looked at are skipped unless `retry`
//! is set.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use futures_util::StreamExt;
use pm_library::album_fit::{
    fit_release, folder_album, release_candidates, release_query, titles_agree, AlbumFit, FolderAlbum,
    FolderFile, Matched,
};
use pm_library::identify::{best_folder_release, decide, AcoustIdResult, Candidate, Decision, FileFacts};
use pm_library::{IdentityCounts, Library, SuggestionRow};
use pm_model::{Confidence, Mbid, MusicUri, Track};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub struct AcoustIdSettings {
    pub api_key: String,
    pub base_url: String,
    /// The key from the config, env or build; a key entered in Settings (`acoustid.api_key`)
    /// is used instead, and clearing it goes back to this one.
    pub configured: String,
}

/// A key entered in Settings › Library (second review: it needed a config-file edit).
pub const KEY_SETTING: &str = "acoustid.api_key";

/// Where the current (or last) run stands.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IdentifyProgress {
    pub running: bool,
    pub total: usize,
    pub done: usize,
    pub identified: usize,
    pub suggested: usize,
    pub unknown: usize,
    pub errors: usize,
    /// This run's identified songs by how (`album_fit`, `fingerprint`, `fingerprint+release`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub identified_by: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Why the run ended before its last song (the next run carries on from there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

/// `GET /v1/library/identify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifyStatus {
    pub progress: IdentifyProgress,
    pub counts: IdentityCounts,
    /// Whether an AcoustID key is configured (fingerprint lookups).
    pub acoustid: bool,
    /// Whether that key was entered in Settings (it can be changed or removed there).
    pub acoustid_own: bool,
    /// "Write IDs into my files" (off by default).
    pub write_ids: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IdentifyStart {
    /// Look again at tracks a previous run found nothing for.
    #[serde(default)]
    pub retry: bool,
}

/// `PUT /v1/library/identify/settings`.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentifySettings {
    #[serde(default)]
    pub write_ids: Option<bool>,
    /// Your own AcoustID key; empty removes it.
    #[serde(default)]
    pub acoustid_key: Option<String>,
}

/// `POST /v1/library/identify/write`: what was written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WriteReport {
    pub written: usize,
    /// Files left as they were, with the reason (format, read-only share, …).
    pub skipped: Vec<WriteSkip>,
    /// Where the tag backups are.
    pub backup_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteSkip {
    pub track_id: i64,
    pub path: String,
    pub reason: String,
}

const WRITE_IDS_KEY: &str = "library.write_ids";

fn write_ids_on(s: &NodeState) -> bool {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(WRITE_IDS_KEY)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Tag backups live next to the node's other data.
pub(crate) fn backup_dir(s: &NodeState) -> PathBuf {
    s.art_dir
        .parent()
        .map(|p| p.join("tag-backups"))
        .unwrap_or_else(|| PathBuf::from("tag-backups"))
}

/// Write the IDs identification found into the files (all such tracks, or
/// `only`); each file's ID tags are backed up first. Blocking.
pub fn write_found_ids(s: &NodeState, only: Option<&[i64]>) -> Result<WriteReport, ApiError> {
    let found = s
        .library
        .lock()
        .expect("library poisoned")
        .identified_not_tagged()?;
    let dir = backup_dir(s);
    let mut report = WriteReport {
        backup_dir: dir.display().to_string(),
        ..Default::default()
    };
    for (t, release) in found {
        let id = t.id.map(|i| i.0).unwrap_or_default();
        if only.is_some_and(|o| !o.contains(&id)) {
            continue;
        }
        let Some((MusicUri::Recording { mbid }, _)) = t.canonical.clone() else {
            continue;
        };
        let ids = pm_library::tagwrite::Ids {
            recording: mbid,
            release,
            acoustid: t.acoustid.clone(),
        };
        match pm_library::tagwrite::write_ids(&t.file.path, &ids, &dir) {
            Ok(_) => {
                report.written += 1;
                let _ = s.library.lock().expect("library poisoned").set_identify_state(
                    id,
                    "identified",
                    Some("written into the file"),
                );
            }
            Err(e) => report.skipped.push(WriteSkip {
                track_id: id,
                path: t.file.path.display().to_string(),
                reason: e.to_string(),
            }),
        }
    }
    Ok(report)
}

/// `PUT /v1/library/identify/settings {write_ids}`.
pub(crate) async fn put_settings(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<IdentifySettings>,
) -> ApiResult<IdentifyStatus> {
    if let Some(w) = body.write_ids {
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(WRITE_IDS_KEY, if w { "true" } else { "false" })?;
    }
    if let Some(k) = body.acoustid_key {
        let k = k.trim().to_string();
        if k.len() > 64 || k.chars().any(|c| !c.is_ascii_alphanumeric()) {
            return Err(ApiError::bad_request(
                "That doesn't look like an AcoustID key (letters and digits, from acoustid.org/new-application)",
            ));
        }
        {
            let lib = s.library.lock().expect("library poisoned");
            if k.is_empty() {
                lib.delete_setting(KEY_SETTING)?;
            } else {
                lib.set_setting(KEY_SETTING, &k)?;
            }
        }
        let mut a = s.acoustid.write().expect("acoustid lock");
        a.api_key = if k.is_empty() { a.configured.clone() } else { k };
    }
    Ok(Json(status(&s)?))
}

/// `POST /v1/library/identify/write`: write the IDs found so far into the files now.
pub(crate) async fn write_now(State(s): State<Arc<NodeState>>) -> ApiResult<WriteReport> {
    let st = s.clone();
    let r = tokio::task::spawn_blocking(move || write_found_ids(&st, None))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(r))
}

/// One suggestion with the track it is for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionView {
    #[serde(flatten)]
    pub row: SuggestionRow,
    pub track: Track,
}

fn status(s: &NodeState) -> Result<IdentifyStatus, ApiError> {
    let (counts, acoustid_own) = {
        let lib = s.library.lock().expect("library poisoned");
        let own = lib.get_setting(KEY_SETTING)?.is_some_and(|k| !k.is_empty());
        (lib.identity_counts()?, own)
    };
    // Its own statement: the run counts while it holds the library, so never hold both
    let progress = s.identify.lock().expect("identify lock").clone();
    Ok(IdentifyStatus {
        progress,
        counts,
        acoustid: !s.acoustid.read().expect("acoustid lock").api_key.is_empty(),
        acoustid_own,
        write_ids: write_ids_on(s),
    })
}

pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<IdentifyStatus> {
    Ok(Json(status(&s)?))
}

/// `POST /v1/library/identify {retry?}`: start the job (no-op while running).
pub(crate) async fn start(
    State(s): State<Arc<NodeState>>,
    body: Option<Json<IdentifyStart>>,
) -> ApiResult<IdentifyStatus> {
    let retry = body.map(|b| b.retry).unwrap_or(false);
    {
        let mut p = s.identify.lock().expect("identify lock");
        if p.running {
            drop(p);
            return Ok(Json(status(&s)?));
        }
        *p = IdentifyProgress {
            running: true,
            started_at: Some(pm_library::db::now_unix()),
            ..Default::default()
        };
    }
    let st = s.clone();
    tokio::spawn(async move {
        if let Err(e) = run(&st, retry).await {
            tracing::warn!("identify: {}", e.message);
            st.identify.lock().expect("identify lock").stopped = Some(e.message);
        }
        let mut p = st.identify.lock().expect("identify lock");
        p.running = false;
        p.finished_at = Some(pm_library::db::now_unix());
        let _ = st.events.send(crate::types::ApiEvent::LibraryChanged);
    });
    Ok(Json(status(&s)?))
}

/// Fingerprints decoded at once: the machine's cores, at most 8 (each decodes up to two minutes
/// of audio, from a share over the network too).
fn fingerprint_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 8)
}

/// Releases a folder's search may look up before the folder goes to the fingerprint.
const RELEASES_PER_FOLDER: usize = 2;

/// A folder that says it is an album, for the album lane.
struct AlbumWork {
    album: FolderAlbum,
    /// Every file of the folder (identified or not): the folder as a release.
    files: Vec<FolderFile>,
    /// Its songs still to identify.
    tracks: Vec<Track>,
}

/// A folder's songs for the fingerprint.
struct Folder {
    tracks: Vec<Track>,
    /// Audio files in the folder.
    len: usize,
    /// The release the rest of the folder fits, when it fits one.
    release: Option<String>,
}

/// A song the fingerprint left open, for the search by tags.
struct Miss {
    track: Track,
    outcome: Outcome,
}

/// Run identification to the end. Public for tests and the CLI.
///
/// Each song goes down a ladder, cheapest first, and stops at the first sure answer: the IDs in
/// its tags (settled at scan time, never here), its folder as an album (one MusicBrainz search
/// for the whole folder, every file matched by position, length and title), its fingerprint
/// (decoded on every core, looked up on AcoustID three a second), and last a MusicBrainz search
/// by its tags (suggestions only). The three lanes run at once: the fingerprint starts on the
/// folders that are no album while the album lane asks MusicBrainz, and takes what the album
/// lane leaves; the search waits for the album lane, which shares MusicBrainz's one request a
/// second and settles a dozen songs a request.
pub async fn run(s: &Arc<NodeState>, retry: bool) -> Result<(), ApiError> {
    let (todo, all) = {
        let lib = s.library.lock().expect("library poisoned");
        (lib.tracks_to_identify(retry)?, lib.all_tracks()?)
    };
    s.identify.lock().expect("identify lock").total = todo.len();
    let folder_of = |t: &Track| t.file.path.parent().map(PathBuf::from).unwrap_or_default();
    let mut by_folder: BTreeMap<PathBuf, Vec<Track>> = BTreeMap::new();
    for t in todo {
        by_folder.entry(folder_of(&t)).or_default().push(t);
    }
    let mut in_folder: HashMap<PathBuf, Vec<FolderFile>> = HashMap::new();
    for t in &all {
        let dir = folder_of(t);
        if by_folder.contains_key(&dir) {
            in_folder.entry(dir).or_default().push(folder_file(t));
        }
    }
    drop(all);
    let settings = s.acoustid.read().expect("acoustid lock").clone();
    let client = if settings.api_key.is_empty() {
        None
    } else {
        let mut cfg = pm_library::identify::AcoustIdConfig::new(settings.api_key.clone(), s.user_agent());
        cfg.base_url = settings.base_url.clone();
        Some(pm_library::identify::AcoustIdClient::new(cfg).map_err(ApiError::from)?)
    };
    let mb = mb_client(s);
    let found: Mutex<Vec<i64>> = Mutex::new(Vec::new());
    // Folders that are no album go to the fingerprint at once; albums to the album lane
    let (to_fp, from_albums) = mpsc::unbounded_channel::<Folder>();
    let mut albums = Vec::new();
    for (dir, tracks) in by_folder {
        let files = in_folder
            .remove(&dir)
            .unwrap_or_else(|| tracks.iter().map(folder_file).collect());
        match folder_album(&files).filter(|_| mb.is_some()) {
            Some(album) => albums.push(AlbumWork { album, files, tracks }),
            None => {
                let _ = to_fp.send(Folder {
                    len: files.len().max(tracks.len()),
                    tracks,
                    release: None,
                });
            }
        }
    }
    let (to_search, misses) = mpsc::unbounded_channel::<Miss>();
    let (albums_done, after_albums) = tokio::sync::oneshot::channel::<()>();
    let album_lane = async {
        let r = album_lane(s, mb.as_ref(), albums, &to_fp, &found).await;
        drop(to_fp);
        let _ = albums_done.send(());
        r
    };
    tokio::try_join!(
        album_lane,
        fingerprint_lane(s, client.as_ref(), from_albums, to_search, &found),
        search_lane(s, misses, after_albums, &found),
    )?;
    let identified_now = found.into_inner().expect("found lock");
    // "Write IDs into my files", when the owner turned it on.
    if !identified_now.is_empty() && write_ids_on(s) {
        let st = s.clone();
        let only = identified_now.clone();
        let r = tokio::task::spawn_blocking(move || write_found_ids(&st, Some(&only)))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
        tracing::info!(
            written = r.written,
            skipped = r.skipped.len(),
            "identify: IDs written into files"
        );
    }
    Ok(())
}

fn folder_file(t: &Track) -> FolderFile {
    FolderFile {
        track: t.id.map(|i| i.0).unwrap_or_default(),
        title: t.title.clone(),
        artist: t.artists.first().map(|a| a.name.clone()).unwrap_or_default(),
        album: t.album.as_ref().map(|a| a.title.clone()),
        album_artist: t.album.as_ref().and_then(|a| a.album_artist.clone()),
        disc_no: t.disc_no,
        track_no: t.track_no,
        duration_ms: t.duration_ms.or(t.file.duration_ms),
        release: t.album.as_ref().and_then(|a| a.release_mbid),
    }
}

/// MusicBrainz, when the owner gave a contact (it requires one).
fn mb_client(s: &NodeState) -> Option<pm_library::musicbrainz::MusicBrainzClient> {
    let contact = s.contact();
    if contact.trim().is_empty() {
        return None;
    }
    let mut cfg = pm_library::musicbrainz::MusicBrainzConfig::new(contact);
    cfg.base_url = s.mb_base.read().expect("mb lock").clone();
    pm_library::musicbrainz::MusicBrainzClient::new(cfg).ok()
}

/// Each album folder against MusicBrainz: what fits is identified, the rest goes on to the
/// fingerprint (with the release the folder fits, when it fits one).
async fn album_lane(
    s: &NodeState,
    mb: Option<&pm_library::musicbrainz::MusicBrainzClient>,
    albums: Vec<AlbumWork>,
    next: &mpsc::UnboundedSender<Folder>,
    found: &Mutex<Vec<i64>>,
) -> Result<(), ApiError> {
    let Some(mb) = mb else {
        return Ok(());
    };
    // A disc per folder asks for its album once
    let mut searches: HashMap<String, Option<serde_json::Value>> = HashMap::new();
    let mut releases: HashMap<Mbid, Option<serde_json::Value>> = HashMap::new();
    // An earlier run's fingerprints are a second opinion, read by file content; with none
    // cached, no file is read for one
    let second_opinion = s
        .library
        .lock()
        .expect("library poisoned")
        .has_cached_lookups()
        .unwrap_or(true);
    let workers = fingerprint_workers();
    for w in albums {
        let fit = match fit_folder(mb, &w, &mut searches, &mut releases).await {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(album = %w.album.title, "identify: the album lookup failed ({e}); the fingerprint decides");
                None
            }
        };
        let mut left = Vec::new();
        if let Some(f) = fit.as_ref() {
            let mut matched = Vec::new();
            for t in w.tracks {
                let id = t.id.map(|i| i.0).unwrap_or_default();
                match f.matched.iter().find(|m| m.track == id) {
                    Some(m) => matched.push((t, m.clone())),
                    None => left.push(t),
                }
            }
            let album = &w.album;
            let mut decided = std::pin::pin!(futures_util::stream::iter(matched)
                .map(|(t, m)| async move {
                    let outcome = album_outcome(s, &t, album, f, &m, second_opinion).await;
                    (t, outcome)
                })
                .buffered(workers));
            while let Some((t, outcome)) = decided.next().await {
                settle(s, &t, outcome, found).await?;
            }
        } else {
            left = w.tracks;
        }
        if !left.is_empty() {
            let _ = next.send(Folder {
                tracks: left,
                len: w.files.len(),
                release: fit.map(|f| f.release.to_string()),
            });
        }
    }
    Ok(())
}

/// The release a folder fits best: the one its tags name, else the search's nearest few.
async fn fit_folder(
    mb: &pm_library::musicbrainz::MusicBrainzClient,
    w: &AlbumWork,
    searches: &mut HashMap<String, Option<serde_json::Value>>,
    releases: &mut HashMap<Mbid, Option<serde_json::Value>>,
) -> Result<Option<AlbumFit>, String> {
    let ids = match w.album.release {
        Some(r) => vec![r],
        None => {
            let q = release_query(&w.album);
            let answer = match searches.get(&q) {
                Some(a) => a.clone(),
                None => {
                    let a = mb
                        .get_json("release", &[("query", q.as_str()), ("limit", "10")])
                        .await
                        .map_err(|e| e.to_string())?;
                    searches.insert(q, a.clone());
                    a
                }
            };
            answer
                .map(|a| release_candidates(&w.album, w.files.len(), &a, RELEASES_PER_FOLDER))
                .unwrap_or_default()
        }
    };
    let mut best: Option<AlbumFit> = None;
    for id in ids {
        let rel = match releases.get(&id) {
            Some(r) => r.clone(),
            None => {
                let r = mb
                    .get_json(&format!("release/{id}"), &[("inc", "recordings+artist-credits")])
                    .await
                    .map_err(|e| e.to_string())?;
                releases.insert(id, r.clone());
                r
            }
        };
        if let Some(f) = rel.as_ref().and_then(|r| fit_release(&w.files, r)) {
            if best.as_ref().is_none_or(|b| f.fit > b.fit) {
                let whole = f.fit >= 1.0;
                best = Some(f);
                if whole {
                    break;
                }
            }
        }
    }
    Ok(best)
}

/// A song its folder's release names. A fingerprint looked up before (by an earlier run) is a
/// second opinion: agreeing raises the confidence; naming another song (not another recording
/// of this one) leaves both for the owner to confirm.
async fn album_outcome(
    s: &NodeState,
    t: &Track,
    album: &FolderAlbum,
    fit: &AlbumFit,
    m: &Matched,
    second_opinion: bool,
) -> Outcome {
    let results = if second_opinion {
        cached_results(s, t).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let strong: Vec<(f32, &str, &pm_library::identify::AcRecording)> = results
        .iter()
        .filter(|r| r.score >= Confidence::FINGERPRINT_ACCEPT)
        .flat_map(|r| r.recordings.iter().map(move |rec| (r.score, r.id.as_str(), rec)))
        .collect();
    let agrees = strong
        .iter()
        .find(|(_, _, rec)| rec.id == m.recording.to_string());
    let other_song = !strong.is_empty()
        && agrees.is_none()
        && !strong
            .iter()
            .any(|(_, _, rec)| rec.title.as_deref().is_some_and(|x| titles_agree(x, &m.title)));
    let mine = Candidate {
        recording: m.recording,
        release: Some(fit.release),
        title: Some(m.title.clone()),
        artist: Some(album.artist.clone()),
        release_title: fit.release_title.clone(),
        score: fit.score(),
        source: "album".into(),
    };
    if other_song {
        let mut c = vec![mine];
        for (score, _, rec) in strong.iter().take(4) {
            let Ok(recording) = Mbid::parse(&rec.id) else {
                continue;
            };
            c.push(Candidate {
                recording,
                release: None,
                title: rec.title.clone(),
                artist: rec.artist(),
                release_title: None,
                score: *score,
                source: "fingerprint".into(),
            });
        }
        return Outcome::Suggest(c);
    }
    let (score, acoustid, detail) = match agrees {
        Some((fp, acoustid, _)) => (
            (1.0 - (1.0 - fit.score()) * (1.0 - fp)).min(0.99),
            Some(acoustid.to_string()),
            "album_fit, the fingerprint agrees",
        ),
        None => (fit.score(), None, "album_fit"),
    };
    Outcome::Identified {
        recording: m.recording,
        release: Some(fit.release),
        confidence: Confidence::AlbumFit { score },
        acoustid,
        detail: detail.into(),
    }
}

/// What an earlier run's AcoustID lookup said about the file, without decoding it.
async fn cached_results(s: &NodeState, t: &Track) -> Option<Vec<AcoustIdResult>> {
    let path = t.file.path.clone();
    let key = tokio::task::spawn_blocking(move || Library::file_key(&path))
        .await
        .ok()?
        .ok()?;
    let (_, _, json) = s
        .library
        .lock()
        .expect("library poisoned")
        .cached_fingerprint(&key)
        .ok()??;
    let r: pm_library::identify::LookupResponse = serde_json::from_str(&json?).ok()?;
    Some(r.results)
}

/// The fingerprint lane: songs are fingerprinted on every core and looked up as they come, and
/// each folder is decided once all its songs are in (the folder as a release disambiguates).
async fn fingerprint_lane(
    s: &NodeState,
    client: Option<&pm_library::identify::AcoustIdClient>,
    folders: mpsc::UnboundedReceiver<Folder>,
    misses: mpsc::UnboundedSender<Miss>,
    found: &Mutex<Vec<i64>>,
) -> Result<(), ApiError> {
    struct Meta {
        len: usize,
        release: Option<String>,
        songs: usize,
    }
    let workers = fingerprint_workers();
    let decoding = tokio::sync::Semaphore::new(workers);
    let folders =
        futures_util::stream::unfold(folders, |mut rx| async move { rx.recv().await.map(|f| (f, rx)) });
    let looked = folders
        .flat_map(|f| {
            let meta = Arc::new(Meta {
                len: f.len,
                release: f.release,
                songs: f.tracks.len(),
            });
            futures_util::stream::iter(f.tracks.into_iter().map(move |t| (meta.clone(), t)))
        })
        .map(|(meta, t)| {
            let decoding = &decoding;
            async move {
                let r = lookup(s, client, &t, decoding).await;
                (meta, t, r)
            }
        })
        // Lookups wait for AcoustID's pace while more songs decode
        .buffered(workers * 2 + 4);
    let mut looked = std::pin::pin!(looked);
    let mut folder: Vec<(Track, FileFacts, Result<Vec<AcoustIdResult>, String>)> = Vec::new();
    while let Some((meta, t, r)) = looked.next().await {
        let facts = FileFacts {
            track_no: t.track_no,
            disc_no: t.disc_no,
            duration_secs: t.duration_ms.or(t.file.duration_ms).map(|ms| ms as f64 / 1000.0),
        };
        folder.push((t, facts, r));
        if folder.len() < meta.songs {
            continue;
        }
        let looked_up = std::mem::take(&mut folder);
        let pairs: Vec<(FileFacts, &[AcoustIdResult])> = looked_up
            .iter()
            .filter_map(|(_, f, r)| r.as_ref().ok().map(|r| (*f, r.as_slice())))
            .collect();
        let preferred = meta
            .release
            .clone()
            .or_else(|| best_folder_release(&pairs, meta.len));
        for (t, facts, r) in looked_up {
            let outcome = match r {
                Err(e) => Outcome::Error(e),
                Ok(results) => match decide(&results, &facts, meta.len, preferred.as_deref()) {
                    Decision::Identified {
                        recording,
                        release,
                        confidence,
                        acoustid,
                    } => Outcome::Identified {
                        recording,
                        release,
                        confidence,
                        acoustid: Some(acoustid),
                        detail: confidence.label().into(),
                    },
                    Decision::Suggest(c) => Outcome::Suggest(c),
                    Decision::Unknown => Outcome::Unknown,
                },
            };
            match outcome {
                // Nothing from the fingerprint: the search by tags has a go
                Outcome::Unknown | Outcome::Error(_) => {
                    let _ = misses.send(Miss { track: t, outcome });
                }
                o => settle(s, &t, o, found).await?,
            }
        }
    }
    Ok(())
}

/// The last step, once the album lane is done with MusicBrainz: a search by the tags.
async fn search_lane(
    s: &NodeState,
    mut misses: mpsc::UnboundedReceiver<Miss>,
    after_albums: tokio::sync::oneshot::Receiver<()>,
    found: &Mutex<Vec<i64>>,
) -> Result<(), ApiError> {
    let _ = after_albums.await;
    while let Some(m) = misses.recv().await {
        let outcome = match search_suggestions(s, &m.track).await {
            Ok(c) if !c.is_empty() => Outcome::Suggest(c),
            _ => m.outcome,
        };
        settle(s, &m.track, outcome, found).await?;
    }
    Ok(())
}

/// Store what a song's ladder found and count it.
async fn settle(s: &NodeState, t: &Track, outcome: Outcome, found: &Mutex<Vec<i64>>) -> Result<(), ApiError> {
    let id = t.id.map(|i| i.0).unwrap_or_default();
    // The library first (waiting through a busy database), then the count, never both at once:
    // `status` holds the progress while it reads the library
    match &outcome {
        Outcome::Identified {
            recording,
            release,
            confidence,
            acoustid,
            detail,
        } => {
            let uri = MusicUri::Recording { mbid: *recording };
            write_waiting(s, |lib| {
                lib.set_identity(id, &uri, *confidence, acoustid.as_deref())
            })
            .await?;
            write_waiting(s, |lib| lib.set_identify_state(id, "identified", Some(detail))).await?;
            if let Some(r) = release {
                write_waiting(s, |lib| lib.set_identify_release(id, r)).await?;
            }
            found.lock().expect("found lock").push(id);
        }
        Outcome::Suggest(c) => {
            write_waiting(s, |lib| lib.add_suggestions(id, c)).await?;
            write_waiting(s, |lib| lib.set_identify_state(id, "suggested", None)).await?;
        }
        Outcome::Unknown => write_waiting(s, |lib| lib.set_identify_state(id, "unknown", None)).await?,
        Outcome::Error(e) => write_waiting(s, |lib| lib.set_identify_state(id, "error", Some(e))).await?,
    }
    let mut p = s.identify.lock().expect("identify lock");
    p.done += 1;
    match outcome {
        Outcome::Identified { confidence, .. } => {
            p.identified += 1;
            *p.identified_by.entry(confidence.label().to_string()).or_default() += 1;
        }
        Outcome::Suggest(_) => p.suggested += 1,
        Outcome::Unknown => p.unknown += 1,
        Outcome::Error(e) => {
            p.errors += 1;
            p.last_error = Some(e);
        }
    }
    Ok(())
}

enum Outcome {
    Identified {
        recording: Mbid,
        release: Option<Mbid>,
        confidence: Confidence,
        acoustid: Option<String>,
        /// How, in a word or two (kept with the state).
        detail: String,
    },
    Suggest(Vec<Candidate>),
    Unknown,
    Error(String),
}

/// Fingerprint (cached by file key) and look up (cached answer, else AcoustID when a key is
/// set). Without a key only an answer looked up before counts: nothing is decoded for nothing.
async fn lookup(
    s: &NodeState,
    client: Option<&pm_library::identify::AcoustIdClient>,
    t: &Track,
    decoding: &tokio::sync::Semaphore,
) -> Result<Vec<AcoustIdResult>, String> {
    let path = t.file.path.clone();
    let key = tokio::task::spawn_blocking({
        let p = path.clone();
        move || Library::file_key(&p)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let cached = s
        .library
        .lock()
        .expect("library poisoned")
        .cached_fingerprint(&key)
        .map_err(|e| e.to_string())?;
    if let Some((_, _, Some(j))) = &cached {
        let r: pm_library::identify::LookupResponse = serde_json::from_str(j).map_err(|e| e.to_string())?;
        return Ok(r.results);
    }
    let Some(client) = client else {
        return Ok(Vec::new());
    };
    let (duration, fp) = match cached {
        Some((d, fp, _)) => (d, fp),
        None => {
            if t.file.format.kind == pm_model::SampleKind::Dsd {
                return Err("DSD files are not fingerprinted".into());
            }
            let _turn = decoding.acquire().await.map_err(|e| e.to_string())?;
            let p = path.clone();
            let f = tokio::task::spawn_blocking(move || pm_audio::fingerprint::fingerprint_file(&p))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("fingerprint: {e}"))?;
            // The cache saves work, nothing more: a busy library doesn't fail the song
            if let Err(e) = s.library.lock().expect("library poisoned").save_fingerprint(
                &key,
                f.duration_secs,
                &f.fingerprint,
            ) {
                tracing::debug!("identify: fingerprint not cached: {e}");
            }
            (f.duration_secs, f.fingerprint)
        }
    };
    let (raw, results) = client.lookup(&fp, duration).await.map_err(|e| e.to_string())?;
    if let Err(e) = s
        .library
        .lock()
        .expect("library poisoned")
        .save_lookup(&key, &raw)
    {
        tracing::debug!("identify: AcoustID answer not cached: {e}");
    }
    Ok(results)
}

/// How long the job waits for a write while another connection holds the library (a scan
/// writing) before it stops; each try waits the connection's busy timeout first.
const BUSY_PATIENCE: std::time::Duration = std::time::Duration::from_secs(600);

/// A library write that waits through a busy database instead of ending the run (it stopped at
/// the first "database is locked" while a scan ran): tried again once the scan is done (or a
/// second later, when no scan runs), for up to [`BUSY_PATIENCE`].
async fn write_waiting<T>(
    s: &NodeState,
    write: impl Fn(&Library) -> pm_library::Result<T>,
) -> Result<T, ApiError> {
    let start = std::time::Instant::now();
    loop {
        let r = write(&s.library.lock().expect("library poisoned"));
        match r {
            Err(e) if e.is_busy() && start.elapsed() < BUSY_PATIENCE => {
                tracing::info!("identify waits for the library (another writer holds it): {e}");
                // Without the library meanwhile: a try holds it through SQLite's busy wait, and
                // every request that reads the library would wait with it
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    if !s.is_scanning() || start.elapsed() >= BUSY_PATIENCE {
                        break;
                    }
                }
            }
            Err(e) if e.is_busy() => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "the library stayed busy for {} minutes (a scan writing?): {e}",
                        BUSY_PATIENCE.as_secs() / 60
                    ),
                ))
            }
            r => return r.map_err(ApiError::from),
        }
    }
}

/// MusicBrainz recordings matching the tags' artist and title (± 2 s): suggestions only, ranked
/// by [`rank_hits`]. A file with no artist gets none: a title alone matches strangers' songs.
async fn search_suggestions(s: &NodeState, t: &Track) -> Result<Vec<Candidate>, String> {
    let contact = s.contact();
    let artist = t.artists.first().map(|a| a.name.clone()).unwrap_or_default();
    if contact.is_empty() || t.title.trim().is_empty() || artist.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut cfg = pm_library::musicbrainz::MusicBrainzConfig::new(contact);
    cfg.base_url = s.mb_base.read().expect("mb lock").clone();
    let mb = pm_library::musicbrainz::MusicBrainzClient::new(cfg).map_err(|e| e.to_string())?;
    let found = mb
        .search_recording_hits(&artist, &t.title, t.duration_ms)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rank_hits(
        &t.title,
        t.album.as_ref().map(|a| a.title.as_str()),
        t.duration_ms,
        found,
    ))
}

/// A title as people compare them: lower case, without "(Live)", "[Remastered]" or
/// " - 2011 Remaster", and with punctuation as spaces.
fn plain_title(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    let s = s.split(" - ").next().unwrap_or(s);
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.extend(if c.is_alphanumeric() {
                c.to_lowercase().collect::<Vec<_>>()
            } else {
                vec![' ']
            }),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether two titles name the same song: equal when plain, or one is the other with a few
/// words more ("Bohemian Rhapsody" and "Bohemian Rhapsody Live Aid").
fn same_title(a: &str, b: &str) -> Option<bool> {
    let (a, b) = (plain_title(a), plain_title(b));
    if a.is_empty() || b.is_empty() {
        return None;
    }
    if a == b {
        return Some(true);
    }
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    (long.starts_with(short.as_str()) && short.len() * 10 >= long.len() * 6).then_some(false)
}

/// Rank MusicBrainz's hits for a file (second review #5, #6):
/// - a hit must carry the file's title (MusicBrainz also returns songs that only mention it);
/// - of the releases a recording is on, the one named like the file's album wins, else the
///   first studio release; bootlegs, live albums and compilations rank lower unless the file's
///   album is one;
/// - a length more than 3 s off, a title with extra words or another album lower the score.
fn rank_hits(
    title: &str,
    album: Option<&str>,
    duration_ms: Option<u64>,
    hits: Vec<pm_library::musicbrainz::RecordingHit>,
) -> Vec<Candidate> {
    let album = album.map(plain_title).filter(|a| !a.is_empty());
    let mut out: Vec<Candidate> = hits
        .into_iter()
        .filter(|h| h.score >= 80)
        .filter_map(|h| {
            let exact = same_title(title, &h.title)?;
            let near = match (h.length_ms, duration_ms) {
                (Some(a), Some(b)) => a.abs_diff(b) <= 3000,
                _ => true,
            };
            let on_album = album
                .as_ref()
                .and_then(|al| h.releases.iter().find(|r| plain_title(&r.title) == *al));
            let pick = on_album
                .or_else(|| h.releases.iter().find(|r| !r.is_secondary()))
                .or(h.releases.first());
            let (release, release_title) = match pick {
                Some(r) => (r.mbid, Some(r.title.clone())),
                None => (h.release_mbid, h.release.clone()),
            };
            let mut score = h.score as f32 / 100.0;
            if !near {
                score *= 0.7;
            }
            if !exact {
                score *= 0.9;
            }
            if album.is_some() && on_album.is_none() {
                score *= 0.9;
            }
            if on_album.is_none() && pick.is_some_and(|r| r.is_secondary()) {
                score *= 0.85;
            }
            Some(Candidate {
                recording: h.mbid,
                release,
                title: Some(h.title.clone()),
                artist: Some(h.artist.clone()),
                release_title,
                score,
                source: "search".into(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    // One choice per title, artist and release: identical-looking rows only confuse
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| {
        seen.insert((
            c.title.as_deref().unwrap_or_default().to_lowercase(),
            c.artist.as_deref().unwrap_or_default().to_lowercase(),
            c.release_title.clone(),
        ))
    });
    out.truncate(5);
    out
}

/// `GET /v1/library/suggestions`: open suggestions with their tracks.
pub(crate) async fn suggestions(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SuggestionView>> {
    let lib = s.library.lock().expect("library poisoned");
    let rows = lib.suggestions()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(track) = lib.get_track(pm_model::TrackId(row.track_id))? {
            out.push(SuggestionView { row, track });
        }
    }
    Ok(Json(out))
}

/// "Which song is this?" for one track: what MusicBrainz suggests, searched now.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackCandidates {
    pub track: Track,
    pub suggestions: Vec<pm_library::SuggestionRow>,
    /// MusicBrainz is asked only with a contact (Settings, or Get started on Home).
    pub needs_contact: bool,
}

/// `GET /v1/library/tracks/{id}/candidates`: search MusicBrainz by the track's tags now (with
/// and, failing that, without its length) and list the suggestions to pick from; picking one is
/// `POST /v1/library/suggestions/{id}/accept`.
pub(crate) async fn track_candidates(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
) -> ApiResult<TrackCandidates> {
    let track = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(pm_model::TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    let needs_contact = s.contact().trim().is_empty();
    if !needs_contact {
        let found = search_suggestions(&s, &track)
            .await
            .map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, format!("MusicBrainz: {e}")))?;
        let lib = s.library.lock().expect("library poisoned");
        lib.add_suggestions(id, &found)?;
        if !found.is_empty() {
            lib.set_identify_state(id, "suggested", None)?;
        }
    }
    let suggestions = s.library.lock().expect("library poisoned").suggestions_for(id)?;
    Ok(Json(TrackCandidates {
        track,
        suggestions,
        needs_contact,
    }))
}

/// `POST /v1/library/suggestions/{id}/accept`: the owner confirms; the
/// track's identity becomes that recording (`confirmed_by_user`).
pub(crate) async fn accept(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Track> {
    let (track, t) = {
        let lib = s.library.lock().expect("library poisoned");
        let Some((track, _)) = lib.accept_suggestion(id)? else {
            return Err(ApiError::not_found(format!("no suggestion {id}")));
        };
        let t = lib
            .get_track(pm_model::TrackId(track))?
            .ok_or_else(|| ApiError::not_found("track gone"))?;
        (track, t)
    };
    if write_ids_on(&s) {
        let st = s.clone();
        let _ = tokio::task::spawn_blocking(move || write_found_ids(&st, Some(&[track]))).await;
    }
    let _ = s.events.send(crate::types::ApiEvent::LibraryChanged);
    Ok(Json(t))
}

/// `DELETE /v1/library/suggestions/{id}`: not this one.
pub(crate) async fn reject(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if s.library
        .lock()
        .expect("library poisoned")
        .reject_suggestion(id)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no suggestion {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_library::musicbrainz::{HitRelease, RecordingHit};
    use pm_model::Mbid;

    const R1: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    const R2: &str = "0383dadf-2a4e-4d10-a46a-e9e041da8eb3";
    const R3: &str = "5b11f4ce-a62d-471e-81fc-a69a8278c7da";

    fn rel(title: &str, id: &str, status: &str, kinds: &[&str]) -> HitRelease {
        HitRelease {
            title: title.into(),
            mbid: Mbid::parse(id).ok(),
            status: status.into(),
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
        }
    }

    fn hit(id: &str, title: &str, length_ms: u64, releases: Vec<HitRelease>) -> RecordingHit {
        RecordingHit {
            mbid: Mbid::parse(id).unwrap(),
            score: 100,
            title: title.into(),
            artist: "Queen".into(),
            release: releases.first().map(|r| r.title.clone()),
            release_mbid: releases.first().and_then(|r| r.mbid),
            length_ms: Some(length_ms),
            releases,
        }
    }

    #[test]
    fn the_files_album_and_studio_releases_come_first() {
        // Second review #5: a live bootleg first, "Greatest Hits II" above "A Night at the Opera"
        let hits = vec![
            hit(
                R1,
                "Death on Two Legs",
                223_000,
                vec![rel("Ultimate Houston 1977", R1, "Bootleg", &["Album", "Live"])],
            ),
            hit(
                R2,
                "Death on Two Legs (Dedicated to…)",
                225_000,
                vec![
                    rel("Greatest Hits II", R2, "Official", &["Album", "Compilation"]),
                    rel("A Night at the Opera", R3, "Official", &["Album"]),
                ],
            ),
        ];
        let c = rank_hits(
            "Death on Two Legs",
            Some("A Night at the Opera"),
            Some(223_000),
            hits,
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].recording, Mbid::parse(R2).unwrap());
        assert_eq!(c[0].release_title.as_deref(), Some("A Night at the Opera"));
        assert_eq!(c[0].release, Mbid::parse(R3).ok());
        assert!(c[1].score < c[0].score);
        assert_eq!(c[1].release_title.as_deref(), Some("Ultimate Houston 1977"));
        // Without an album tag, the studio release is picked over the compilation
        let hits = vec![hit(
            R2,
            "Death on Two Legs",
            223_000,
            vec![
                rel("Greatest Hits II", R2, "Official", &["Album", "Compilation"]),
                rel("A Night at the Opera", R3, "Official", &["Album"]),
            ],
        )];
        let c = rank_hits("Death on Two Legs", None, Some(223_000), hits);
        assert_eq!(c[0].release_title.as_deref(), Some("A Night at the Opera"));
        assert_eq!(c[0].score, 1.0);
    }

    #[test]
    fn a_hit_must_carry_the_files_title() {
        // Second review #6: a long, unrelated title that only mentions the file's
        let hits = vec![
            hit(
                R1,
                "HOLY SHIT EACH EAR HAS ITS OWN TRACK?! (fuck mono audio)",
                5_000,
                vec![],
            ),
            hit(R2, "Mono Audio", 5_000, vec![]),
            hit(R3, "Mono Audio Test Tone For Speakers", 5_000, vec![]),
        ];
        let c = rank_hits("Mono audio", None, Some(5_000), hits);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].recording, Mbid::parse(R2).unwrap());
    }

    #[test]
    fn plain_titles() {
        assert_eq!(
            plain_title("Bohemian Rhapsody - Remastered 2011"),
            "bohemian rhapsody"
        );
        assert_eq!(
            plain_title("Get Lucky (Radio Edit) [feat. Pharrell]"),
            "get lucky"
        );
        assert_eq!(same_title("Don't Stop Me Now", "Don’t Stop Me Now"), Some(true));
        assert_eq!(
            same_title("Bohemian Rhapsody", "Bohemian Rhapsody (Live Aid)"),
            Some(true)
        );
        assert_eq!(same_title("Love", "Love of My Life"), None);
    }
}
