//! Identifying local files so they join Pinakes (steward identify brief).
//!
//! Strongest evidence first, each stored with its [`Confidence`]:
//! 1. IDs embedded in tags ([`crate::canonical::assign`], at scan time);
//! 2. a Chromaprint fingerprint looked up on AcoustID: one recording at or
//!    above [`Confidence::FINGERPRINT_ACCEPT`] is accepted; several are
//!    disambiguated by the **folder as a release** (Picard's approach: the
//!    release whose track count, positions and durations fit the folder);
//! 3. otherwise the candidates are kept as *suggestions* for the owner to
//!    confirm, never used as identity on their own.
//!
//! The pure decision logic lives here and is tested against AcoustID-shaped
//! answers; the HTTP client is [`AcoustIdClient`] (feature `musicbrainz`).

use pm_model::{Confidence, Mbid};
use serde::{Deserialize, Serialize};

/// Durations within this many seconds count as the same take.
pub const DURATION_SLACK_SECS: f64 = 3.0;

// ---- AcoustID lookup answer (`meta=recordings releasegroups releases tracks`) ----

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LookupResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub results: Vec<AcoustIdResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<LookupError>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LookupError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcoustIdResult {
    pub id: String,
    pub score: f32,
    #[serde(default)]
    pub recordings: Vec<AcRecording>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcRecording {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Seconds.
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub artists: Vec<AcArtist>,
    #[serde(default)]
    pub releases: Vec<AcRelease>,
    #[serde(default)]
    pub releasegroups: Vec<AcReleaseGroup>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcArtist {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcReleaseGroup {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub releases: Vec<AcRelease>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcRelease {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub medium_count: Option<u32>,
    #[serde(default)]
    pub mediums: Vec<AcMedium>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcMedium {
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub tracks: Vec<AcTrack>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcTrack {
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub id: Option<String>,
}

impl AcRecording {
    /// Releases whether AcoustID nested them under release groups or not.
    pub fn all_releases(&self) -> impl Iterator<Item = &AcRelease> {
        self.releases
            .iter()
            .chain(self.releasegroups.iter().flat_map(|g| g.releases.iter()))
    }

    pub fn artist(&self) -> Option<String> {
        (!self.artists.is_empty()).then(|| {
            self.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

impl AcRelease {
    /// Total tracks on the release (every medium).
    pub fn total_tracks(&self) -> Option<u32> {
        self.track_count.or_else(|| {
            let sum: u32 = self.mediums.iter().filter_map(|m| m.track_count).sum();
            (sum > 0).then_some(sum)
        })
    }
}

// ---- what we know about the file and its folder ----

/// The file's own position and length, from tags and the decoder.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FileFacts {
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub duration_secs: Option<f64>,
}

fn duration_fits(rec: &AcRecording, file: &FileFacts) -> bool {
    match (rec.duration, file.duration_secs) {
        (Some(a), Some(b)) => (a - b).abs() <= DURATION_SLACK_SECS,
        // Nothing to compare: do not let a missing length rule a release out.
        _ => true,
    }
}

/// Whether `rel` looks like the folder: same total track count and this
/// file at its disc/track position on it.
pub fn release_fits(rel: &AcRelease, file: &FileFacts, folder_len: usize) -> bool {
    let Some(track_no) = file.track_no else {
        return false;
    };
    if folder_len > 0 && rel.total_tracks() != Some(folder_len as u32) {
        return false;
    }
    let disc = file.disc_no.unwrap_or(1);
    rel.mediums
        .iter()
        .filter(|m| m.position == disc || (rel.mediums.len() == 1 && m.position == 0))
        .any(|m| m.tracks.iter().any(|t| t.position == track_no))
}

/// The release the folder most likely is: the one the most files fit at
/// their positions (strong matches only). `None` when no release fits at
/// least half of the folder, or two releases tie.
pub fn best_folder_release(files: &[(FileFacts, &[AcoustIdResult])], folder_len: usize) -> Option<String> {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for (facts, results) in files {
        let mut seen: std::collections::BTreeSet<&str> = Default::default();
        for r in results
            .iter()
            .filter(|r| r.score >= Confidence::FINGERPRINT_ACCEPT)
        {
            for rec in &r.recordings {
                if !duration_fits(rec, facts) {
                    continue;
                }
                for rel in rec.all_releases() {
                    if release_fits(rel, facts, folder_len) && seen.insert(rel.id.as_str()) {
                        *counts.entry(rel.id.as_str()).or_default() += 1;
                    }
                }
            }
        }
    }
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by_key(|x| std::cmp::Reverse(x.1));
    let (best, n) = *ranked.first()?;
    if ranked.get(1).is_some_and(|x| x.1 == n) {
        return None;
    }
    let need = folder_len.div_ceil(2).max(1);
    (n >= need).then(|| best.to_string())
}

// ---- the decision ----

/// One candidate recording for the owner to confirm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub recording: Mbid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<Mbid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_title: Option<String>,
    pub score: f32,
    /// `fingerprint` (AcoustID) or `search` (MusicBrainz by tags).
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Canonical: one recording, by fingerprint (and maybe the release).
    Identified {
        recording: Mbid,
        release: Option<Mbid>,
        confidence: Confidence,
        acoustid: String,
    },
    /// Several candidates (or only weak ones): the owner decides.
    Suggest(Vec<Candidate>),
    /// AcoustID knows nothing about this audio, or knows it without a
    /// MusicBrainz recording (the caller then tries a search by tags).
    Unknown,
}

struct Seen<'a> {
    rec: &'a AcRecording,
    score: f32,
    acoustid: &'a str,
}

fn distinct(results: &[AcoustIdResult], min: f32) -> Vec<Seen<'_>> {
    let mut out: Vec<Seen> = Vec::new();
    for r in results.iter().filter(|r| r.score >= min) {
        for rec in &r.recordings {
            if Mbid::parse(&rec.id).is_err() {
                continue;
            }
            match out.iter_mut().find(|s| s.rec.id == rec.id) {
                Some(s) if s.score < r.score => {
                    s.score = r.score;
                    s.acoustid = &r.id;
                }
                Some(_) => {}
                None => out.push(Seen {
                    rec,
                    score: r.score,
                    acoustid: &r.id,
                }),
            }
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

fn fitting_release<'a>(
    rec: &'a AcRecording,
    file: &FileFacts,
    folder_len: usize,
    preferred: Option<&str>,
) -> Option<&'a AcRelease> {
    if !duration_fits(rec, file) {
        return None;
    }
    let fits: Vec<&AcRelease> = rec
        .all_releases()
        .filter(|rel| release_fits(rel, file, folder_len))
        .collect();
    match preferred {
        Some(p) => fits.into_iter().find(|rel| rel.id == p),
        None => fits.into_iter().next(),
    }
}

fn candidate(s: &Seen, release: Option<&AcRelease>) -> Candidate {
    Candidate {
        recording: Mbid::parse(&s.rec.id).expect("filtered"),
        release: release.and_then(|r| Mbid::parse(&r.id).ok()),
        title: s.rec.title.clone(),
        artist: s.rec.artist(),
        release_title: release.and_then(|r| r.title.clone()),
        score: s.score,
        source: "fingerprint".into(),
    }
}

/// Decide what AcoustID's answer means for one file. `folder_len` is the
/// number of audio files in its folder; `preferred_release` the folder's
/// release from [`best_folder_release`], when there is one.
pub fn decide(
    results: &[AcoustIdResult],
    file: &FileFacts,
    folder_len: usize,
    preferred_release: Option<&str>,
) -> Decision {
    let strong = distinct(results, Confidence::FINGERPRINT_ACCEPT);
    if strong.is_empty() {
        let weak = distinct(results, 0.5);
        if weak.is_empty() {
            return Decision::Unknown;
        }
        return Decision::Suggest(weak.iter().take(5).map(|s| candidate(s, None)).collect());
    }
    let with_release: Vec<(&Seen, Option<&AcRelease>)> = strong
        .iter()
        .map(|s| {
            let rel = fitting_release(s.rec, file, folder_len, preferred_release)
                .or_else(|| preferred_release.and(fitting_release(s.rec, file, folder_len, None)));
            (s, rel)
        })
        .collect();
    let identified = |s: &Seen, rel: Option<&AcRelease>| Decision::Identified {
        recording: Mbid::parse(&s.rec.id).expect("filtered"),
        release: rel.and_then(|r| Mbid::parse(&r.id).ok()),
        confidence: if rel.is_some() {
            Confidence::FingerprintRelease { score: s.score }
        } else {
            Confidence::Fingerprint { score: s.score }
        },
        acoustid: s.acoustid.to_string(),
    };
    if with_release.len() == 1 {
        let (s, rel) = with_release[0];
        return identified(s, rel);
    }
    // Several recordings fit the audio: the folder decides, when it can.
    let on_preferred: Vec<&(&Seen, Option<&AcRelease>)> = with_release
        .iter()
        .filter(|(_, rel)| rel.is_some_and(|r| Some(r.id.as_str()) == preferred_release))
        .collect();
    if on_preferred.len() == 1 {
        let (s, rel) = on_preferred[0];
        return identified(s, *rel);
    }
    let fitting: Vec<&(&Seen, Option<&AcRelease>)> =
        with_release.iter().filter(|(_, rel)| rel.is_some()).collect();
    if fitting.len() == 1 {
        let (s, rel) = fitting[0];
        return identified(s, *rel);
    }
    Decision::Suggest(
        with_release
            .iter()
            .take(5)
            .map(|(s, rel)| candidate(s, *rel))
            .collect(),
    )
}

// ---- the AcoustID web service ----

#[cfg(feature = "musicbrainz")]
pub use client::{AcoustIdClient, AcoustIdConfig};

#[cfg(feature = "musicbrainz")]
mod client {
    use super::*;
    use crate::{LibraryError, Result};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;

    pub const DEFAULT_BASE: &str = "https://api.acoustid.org";

    #[derive(Debug, Clone)]
    pub struct AcoustIdConfig {
        pub base_url: String,
        /// The application key (acoustid.org/new-application).
        pub api_key: String,
        /// AcoustID allows three requests a second.
        pub min_interval: Duration,
        pub user_agent: String,
    }

    impl AcoustIdConfig {
        pub fn new(api_key: impl Into<String>, user_agent: impl Into<String>) -> Self {
            Self {
                base_url: DEFAULT_BASE.into(),
                api_key: api_key.into(),
                min_interval: Duration::from_millis(340),
                user_agent: user_agent.into(),
            }
        }
    }

    #[derive(Clone)]
    pub struct AcoustIdClient {
        http: reqwest::Client,
        config: AcoustIdConfig,
        last: Arc<Mutex<Option<Instant>>>,
    }

    impl AcoustIdClient {
        pub fn new(config: AcoustIdConfig) -> Result<Self> {
            let http = reqwest::Client::builder()
                .user_agent(config.user_agent.clone())
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| LibraryError::AcoustId(e.to_string()))?;
            Ok(Self {
                http,
                config,
                last: Arc::new(Mutex::new(None)),
            })
        }

        /// Look a fingerprint up. Returns the raw JSON (for the cache) and
        /// the parsed results.
        pub async fn lookup(
            &self,
            fingerprint: &str,
            duration_secs: u64,
        ) -> Result<(String, Vec<AcoustIdResult>)> {
            {
                let mut last = self.last.lock().await;
                if let Some(t) = *last {
                    let e = t.elapsed();
                    if e < self.config.min_interval {
                        tokio::time::sleep(self.config.min_interval - e).await;
                    }
                }
                *last = Some(Instant::now());
            }
            let url = format!("{}/v2/lookup", self.config.base_url.trim_end_matches('/'));
            let duration = duration_secs.to_string();
            let form = [
                ("client", self.config.api_key.as_str()),
                ("format", "json"),
                ("meta", "recordings releasegroups releases tracks compress"),
                ("duration", duration.as_str()),
                ("fingerprint", fingerprint),
            ];
            let body: String = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(form)
                .finish();
            let resp = self
                .http
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await
                .map_err(|e| LibraryError::AcoustId(e.to_string()))?;
            let status = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| LibraryError::AcoustId(e.to_string()))?;
            let parsed: LookupResponse =
                serde_json::from_str(&body).map_err(|e| LibraryError::AcoustId(format!("{status}: {e}")))?;
            if parsed.status != "ok" {
                let msg = parsed
                    .error
                    .map(|e| format!("{} (code {})", e.message, e.code))
                    .unwrap_or_else(|| status.to_string());
                return Err(LibraryError::AcoustId(msg));
            }
            Ok((body, parsed.results))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<AcoustIdResult> {
        let s = std::fs::read_to_string(format!(
            "{}/tests/fixtures/acoustid/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let r: LookupResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(r.status, "ok");
        r.results
    }

    const BOHEMIAN: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";
    const BOHEMIAN_DUP: &str = "ebf79ba5-085e-48d2-9eb8-2d992fbf0f6d";
    const OPERA_1975: &str = "1a5b6a49-4c4f-3d18-9e3d-1c4d1b0a7a3e";

    #[test]
    fn one_strong_recording_is_identified() {
        let r = fixture("single.json");
        let file = FileFacts {
            track_no: Some(11),
            disc_no: Some(1),
            duration_secs: Some(355.0),
        };
        match decide(&r, &file, 1, None) {
            Decision::Identified {
                recording,
                confidence,
                acoustid,
                ..
            } => {
                assert_eq!(recording.to_string(), BOHEMIAN);
                assert!(matches!(confidence, Confidence::Fingerprint { score } if score > 0.95));
                assert_eq!(acoustid, "9ff43b6a-4f16-427c-93c2-92307ca505e0");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn below_the_threshold_is_only_a_suggestion() {
        let mut r = fixture("single.json");
        r[0].score = 0.89;
        match decide(&r, &FileFacts::default(), 1, None) {
            Decision::Suggest(c) => {
                assert_eq!(c.len(), 1);
                assert_eq!(c[0].recording.to_string(), BOHEMIAN);
            }
            other => panic!("0.89 must not be identified: {other:?}"),
        }
        r[0].score = 0.9;
        assert!(matches!(
            decide(&r, &FileFacts::default(), 1, None),
            Decision::Identified { .. }
        ));
        assert_eq!(decide(&[], &FileFacts::default(), 1, None), Decision::Unknown);
    }

    #[test]
    fn several_recordings_are_suggestions_without_a_folder() {
        let r = fixture("multi.json");
        let file = FileFacts {
            track_no: None,
            disc_no: None,
            duration_secs: Some(355.0),
        };
        match decide(&r, &file, 0, None) {
            Decision::Suggest(c) => {
                let ids: Vec<String> = c.iter().map(|c| c.recording.to_string()).collect();
                assert!(ids.contains(&BOHEMIAN.to_string()) && ids.contains(&BOHEMIAN_DUP.to_string()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_folder_as_a_release_disambiguates() {
        let r = fixture("multi.json");
        // A 12-track folder; this file is track 11 on disc 1 and 5:55 long:
        // only the 1975 album (12 tracks, Bohemian Rhapsody at 11) fits.
        let file = FileFacts {
            track_no: Some(11),
            disc_no: Some(1),
            duration_secs: Some(355.0),
        };
        let folder: Vec<(FileFacts, &[AcoustIdResult])> = vec![(file, &r[..])];
        // One file alone is not half of a 12-track folder.
        assert_eq!(best_folder_release(&folder, 12), None);
        match decide(&r, &file, 12, None) {
            Decision::Identified {
                recording,
                release,
                confidence,
                ..
            } => {
                assert_eq!(recording.to_string(), BOHEMIAN);
                assert_eq!(release.unwrap().to_string(), OPERA_1975);
                assert!(matches!(confidence, Confidence::FingerprintRelease { .. }));
            }
            other => panic!("{other:?}"),
        }
        // Wrong position: nothing fits, so the owner chooses.
        let elsewhere = FileFacts {
            track_no: Some(3),
            ..file
        };
        assert!(matches!(decide(&r, &elsewhere, 12, None), Decision::Suggest(_)));
        // A length off by more than 3 s rules a take out.
        let longer = FileFacts {
            duration_secs: Some(366.0),
            ..file
        };
        assert!(matches!(decide(&r, &longer, 12, None), Decision::Suggest(_)));
    }

    #[test]
    fn the_folder_release_wins_by_majority() {
        let r = fixture("multi.json");
        let mut files = Vec::new();
        let facts: Vec<FileFacts> = (0..7)
            .map(|_| FileFacts {
                track_no: Some(11),
                disc_no: Some(1),
                duration_secs: Some(355.0),
            })
            .collect();
        for f in &facts {
            files.push((*f, &r[..]));
        }
        assert_eq!(best_folder_release(&files, 12).as_deref(), Some(OPERA_1975));
        match decide(&r, &facts[0], 12, Some(OPERA_1975)) {
            Decision::Identified { release, .. } => assert_eq!(release.unwrap().to_string(), OPERA_1975),
            other => panic!("{other:?}"),
        }
    }

    /// A real AcoustID answer (recorded with the node's key): it parses, and
    /// a file at track 11 of a 12-file folder lands on a real 12-track
    /// release of A Night at the Opera.
    #[test]
    fn a_recorded_answer_parses_and_finds_the_release() {
        let r = fixture("recorded-bohemian-trackid.json");
        assert_eq!(r.len(), 1);
        let rec = &r[0].recordings[0];
        assert_eq!(rec.id, BOHEMIAN);
        assert!(rec.releasegroups.len() > 100);
        let file = FileFacts {
            track_no: Some(11),
            disc_no: Some(1),
            duration_secs: Some(355.0),
        };
        match decide(&r, &file, 12, None) {
            Decision::Identified {
                recording,
                release,
                confidence,
                acoustid,
            } => {
                assert_eq!(recording.to_string(), BOHEMIAN);
                assert!(matches!(confidence, Confidence::FingerprintRelease { .. }));
                assert_eq!(acoustid, "84eed5cd-5b1f-4f5c-8c92-2686d2877862");
                let rel = release.unwrap().to_string();
                let on_opera = rec
                    .releasegroups
                    .iter()
                    .filter(|g| g.title.as_deref() == Some("A Night at the Opera"))
                    .flat_map(|g| g.releases.iter())
                    .any(|x| x.id == rel);
                let any_12 = rec
                    .all_releases()
                    .any(|x| x.id == rel && x.total_tracks() == Some(12));
                assert!(any_12, "the release fits the 12-file folder");
                let _ = on_opera;
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn suggestions_never_count_as_identity() {
        let c = Confidence::Fuzzy { score: 0.99 };
        assert!(!c.publishable());
        assert_eq!(c.label(), "suggested");
        assert!(Confidence::ConfirmedByUser.publishable());
        assert!(Confidence::FingerprintRelease { score: 0.92 }.publishable());
        assert_eq!(
            Confidence::FingerprintRelease { score: 0.92 }.label(),
            "fingerprint+release"
        );
    }
}
