//! MusicBrainz ids for library artists and albums whose tags have none (`pm_library::matching`
//! decides; this asks). In the background, a pass every few minutes while something is left:
//! albums first (a tagged id is asked by id, the rest searched by title and artist), then each
//! artist from its albums' credits, else from up to three of its songs. Only MusicBrainz (or the
//! mirror in `PM_MUSICBRAINZ_URL`), through the node's shared gate, a request every two seconds
//! at most so pages and identify keep their share; never Pinakes. Nothing is asked without the
//! node's contact. Tags always win, and there is no manual fix: an unsure answer stays
//! unmatched and is asked again in 30 days or when the library has more of it.
//!
//! - `GET /v1/library/matches`: `{artists, albums, running}`, each `{total, tags, lookup,
//!   unmatched, pending}`.

use crate::state::NodeState;
use crate::ApiResult;
use axum::extract::State;
use axum::Json;
use pm_library::matching::{self, AlbumAsk, AlbumDecision, MatchRow, MatchSource, SongEvidence};
use pm_library::musicbrainz::{MusicBrainzClient, MusicBrainzConfig};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(false);
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

const EVERY: Duration = Duration::from_secs(10 * 60);
/// Between the matcher's requests: half of MusicBrainz's one a second.
const INTERVAL: Duration = Duration::from_secs(2);
/// A pass stops after this many failed requests in a row (MusicBrainz down, no network).
const MAX_FAILURES: u32 = 3;
/// Songs asked for an artist without a matched album.
const SONGS: usize = 3;
/// Opened pages asked first, the newest.
const WANTED: usize = 20;
/// An opened page wakes the matcher at most this often.
const WAKE_EVERY: Duration = Duration::from_secs(60);

/// Artists (plain names) and albums (ids) someone opened: asked first.
fn wanted() -> &'static Mutex<(Vec<String>, Vec<i64>)> {
    static W: std::sync::OnceLock<Mutex<(Vec<String>, Vec<i64>)>> = std::sync::OnceLock::new();
    W.get_or_init(Default::default)
}

/// Keep the newest [`WANTED`] of a list: `x` at the end, the oldest dropped.
fn remember<T: PartialEq>(list: &mut Vec<T>, x: T) {
    list.retain(|y| *y != x);
    list.push(x);
    if list.len() > WANTED {
        list.remove(0);
    }
}

/// Wake the matcher for an opened page, at most once a minute: each pass plans over the whole
/// library, and a page without an id may well be one already answered "not sure".
fn wake() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().expect("last wake");
    if last.is_none_or(|t| t.elapsed() >= WAKE_EVERY) {
        *last = Some(std::time::Instant::now());
        WAKE.notify_one();
    }
}

/// An artist page was opened before the artist has an answer: ask for it (and its albums) next.
pub(crate) fn want_artist(name: &str) {
    remember(
        &mut wanted().lock().expect("wanted").0,
        matching::plain_name(name),
    );
    wake();
}

/// An album page was opened before the album has an answer.
pub(crate) fn want_album(id: i64) {
    remember(&mut wanted().lock().expect("wanted").1, id);
    wake();
}

/// The background matcher.
pub fn spawn(s: &Arc<NodeState>) {
    let s = s.clone();
    tokio::spawn(async move {
        loop {
            if !s.is_scanning() {
                pass(&s, INTERVAL).await;
            }
            let _ = tokio::time::timeout(EVERY, WAKE.notified()).await;
        }
    });
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassReport {
    pub asked: u32,
    pub matched: u32,
    pub unmatched: u32,
    pub failed: u32,
}

fn client(s: &NodeState, interval: Duration) -> Option<MusicBrainzClient> {
    let contact = s.contact();
    if contact.trim().is_empty() {
        return None;
    }
    let mut cfg = MusicBrainzConfig::new(contact);
    cfg.base_url = s.mb_base.read().expect("mb lock").clone();
    cfg.min_interval = interval;
    MusicBrainzClient::new(cfg).ok()
}

/// One pass over what is left. Tests pass a zero interval.
pub(crate) async fn pass(s: &NodeState, interval: Duration) -> PassReport {
    let mut report = PassReport::default();
    let Some(mb) = client(s, interval) else {
        return report;
    };
    if RUNNING.swap(true, Ordering::SeqCst) {
        return report;
    }
    let mut failures = 0;
    let plan = s
        .library
        .lock()
        .expect("library poisoned")
        .match_plan(pm_library::db::now_unix());
    let mut albums = plan.map(|p| p.albums).unwrap_or_default();
    {
        let w = wanted().lock().expect("wanted");
        albums.sort_by_key(|t| {
            !(w.1.contains(&t.album.id) || w.0.contains(&matching::plain_name(&t.album.artist)))
        });
    }
    for t in albums {
        if failures >= MAX_FAILURES {
            break;
        }
        report.asked += 1;
        let answer = match t.ask {
            AlbumAsk::Search => mb
                .get_json(
                    "release",
                    &[("query", &matching::album_query(&t.album)), ("limit", "25")],
                )
                .await
                .map(|v| {
                    v.map(|v| matching::decide_album(&t.album, &v))
                        .map(|d| (d, MatchSource::Lookup))
                }),
            AlbumAsk::Release(r) => mb
                .get_json(
                    &format!("release/{r}"),
                    &[("inc", "release-groups+artist-credits")],
                )
                .await
                .map(|v| v.map(|v| by_id(&v, None))),
            AlbumAsk::ReleaseGroup(g) => mb
                .get_json(&format!("release-group/{g}"), &[("inc", "artist-credits")])
                .await
                .map(|v| v.map(|v| by_id(&v, Some(g)))),
        };
        let (d, source) = match answer {
            Ok(Some(d)) => d,
            Ok(None) => (
                AlbumDecision {
                    evidence: "the tagged id is not on MusicBrainz".into(),
                    ..AlbumDecision::default()
                },
                MatchSource::Tags,
            ),
            Err(e) => {
                tracing::debug!(album = t.album.id, "musicbrainz: {e}");
                failures += 1;
                report.failed += 1;
                continue;
            }
        };
        failures = 0;
        tally(&mut report, d.release_group.is_some());
        let row = MatchRow {
            mbid: d.release_group,
            source,
            evidence: d.evidence,
            credits: d.credits,
            size: t.album.tracks as i64,
            checked_at: pm_library::db::now_unix(),
        };
        if let Err(e) = s
            .library
            .lock()
            .expect("library poisoned")
            .save_match("album", &t.key, &row)
        {
            tracing::warn!("saving an album match: {e}");
        }
        wanted().lock().expect("wanted").1.retain(|id| *id != t.album.id);
    }
    // Artists once their albums are answered
    let plan = s
        .library
        .lock()
        .expect("library poisoned")
        .match_plan(pm_library::db::now_unix());
    let mut artists = plan.map(|p| p.artists).unwrap_or_default();
    {
        let w = wanted().lock().expect("wanted");
        artists.sort_by_key(|t| !w.0.contains(&t.key));
    }
    for t in artists {
        if failures >= MAX_FAILURES {
            break;
        }
        let (credits, tracks) = {
            let lib = s.library.lock().expect("library poisoned");
            (
                lib.album_credits(&t.albums).unwrap_or_default(),
                lib.artist_tracks(t.id).unwrap_or_default(),
            )
        };
        let named = credits
            .iter()
            .flatten()
            .any(|(_, n)| matching::plain_name(n) == t.key);
        let mut songs: Vec<Vec<pm_model::Mbid>> = Vec::new();
        // This artist's own requests: a failure here leaves it for the next pass
        let mut failed = false;
        if !named {
            let mut seen: Vec<String> = Vec::new();
            for tr in &tracks {
                let Some(duration_ms) = tr.duration_ms.filter(|d| *d > 0) else {
                    continue;
                };
                let title = matching::plain_title(&tr.title);
                if title.is_empty() || seen.contains(&title) {
                    continue;
                }
                seen.push(title);
                let song = SongEvidence {
                    title: tr.title.clone(),
                    duration_ms,
                };
                report.asked += 1;
                match mb
                    .get_json(
                        "recording",
                        &[
                            ("query", &matching::recording_query(&t.name, &song)),
                            ("limit", "25"),
                        ],
                    )
                    .await
                {
                    Ok(v) => {
                        failures = 0;
                        songs.push(
                            v.map(|v| matching::recording_artists(&t.name, &song, &v))
                                .unwrap_or_default(),
                        );
                    }
                    Err(e) => {
                        tracing::debug!(artist = t.id, "musicbrainz: {e}");
                        failures += 1;
                        report.failed += 1;
                        failed = true;
                        break;
                    }
                }
                if songs.len() >= SONGS {
                    break;
                }
            }
            if failed {
                continue;
            }
        }
        let (mbid, evidence) = matching::decide_artist(&t.name, &credits, &songs);
        tally(&mut report, mbid.is_some());
        let row = MatchRow {
            mbid,
            source: MatchSource::Lookup,
            evidence,
            credits: Vec::new(),
            size: t.albums.len() as i64,
            checked_at: pm_library::db::now_unix(),
        };
        if let Err(e) = s
            .library
            .lock()
            .expect("library poisoned")
            .save_match("artist", &t.key, &row)
        {
            tracing::warn!("saving an artist match: {e}");
        }
        wanted().lock().expect("wanted").0.retain(|k| *k != t.key);
    }
    RUNNING.store(false, Ordering::SeqCst);
    if report.asked > 0 {
        tracing::info!(?report, "library matching pass");
    }
    report
}

fn by_id(v: &serde_json::Value, group: Option<pm_model::Mbid>) -> (AlbumDecision, MatchSource) {
    let (release_group, credits) = matching::looked_up(v, group);
    (
        AlbumDecision {
            release_group,
            credits,
            evidence: "asked by the tagged id".into(),
        },
        MatchSource::Tags,
    )
}

fn tally(r: &mut PassReport, matched: bool) {
    if matched {
        r.matched += 1;
    } else {
        r.unmatched += 1;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchStatus {
    pub artists: matching::KindCounts,
    pub albums: matching::KindCounts,
    pub running: bool,
}

/// `GET /v1/library/matches`.
pub(crate) async fn status(State(s): State<Arc<NodeState>>) -> ApiResult<MatchStatus> {
    let c = s.library.lock().expect("library poisoned").match_counts()?;
    Ok(Json(MatchStatus {
        artists: c.artists,
        albums: c.albums,
        running: RUNNING.load(Ordering::SeqCst),
    }))
}
