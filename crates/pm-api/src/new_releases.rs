//! "New releases from your artists": release groups MusicBrainz lists for the
//! library's artists (those with an artist MBID in their tags), refreshed in
//! the background one artist per second and cached in the settings table.

use crate::state::NodeState;
use crate::types::{NewRelease, NewReleasesPage};
use pm_model::Mbid;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How long one artist's list is trusted before it is fetched again.
const ARTIST_TTL_SECS: i64 = 7 * 24 * 3600;
/// How far back a release still counts as new.
const WINDOW_DAYS: i64 = 365;
/// Pause between MusicBrainz requests (their limit is one per second).
const PACE: std::time::Duration = std::time::Duration::from_millis(1100);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedGroups {
    at: i64,
    groups: Vec<Group>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Group {
    mbid: String,
    title: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    kind: String,
}

fn key(artist_id: i64) -> String {
    format!("mb.rg.{artist_id}")
}

fn refreshing() -> &'static AtomicBool {
    static R: AtomicBool = AtomicBool::new(false);
    &R
}

fn days_ago(date: &str, now: i64) -> Option<i64> {
    // "YYYY", "YYYY-MM" or "YYYY-MM-DD"; partial dates count from their first day.
    let mut it = date.split('-');
    let y: i64 = it.next()?.trim().parse().ok()?;
    let m: i64 = it.next().and_then(|m| m.parse().ok()).unwrap_or(1);
    let d: i64 = it.next().and_then(|d| d.parse().ok()).unwrap_or(1);
    let date =
        time::Date::from_calendar_date(y as i32, time::Month::try_from(m as u8).ok()?, d as u8).ok()?;
    let unix = date.with_hms(0, 0, 0).ok()?.assume_utc().unix_timestamp();
    Some((now - unix) / 86_400)
}

/// The page clients show: everything cached, newest first. Kicks a
/// background refresh when some artist's list is missing or stale.
pub(crate) fn page(s: &Arc<NodeState>) -> Result<NewReleasesPage, pm_library::LibraryError> {
    let now = pm_library::db::now_unix();
    let lib = s.library.lock().expect("library poisoned");
    let artists = lib.list_artists()?;
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut checked = 0usize;
    let mut stale = 0usize;
    let mut refreshed_at: Option<i64> = None;
    let mut total = 0usize;
    for a in &artists {
        if a.mbid.is_none() {
            continue;
        }
        total += 1;
        let Some(c) = lib
            .get_setting(&key(a.id))?
            .and_then(|j| serde_json::from_str::<CachedGroups>(&j).ok())
        else {
            stale += 1;
            continue;
        };
        checked += 1;
        if now - c.at > ARTIST_TTL_SECS {
            stale += 1;
        }
        refreshed_at = Some(refreshed_at.map_or(c.at, |r| r.max(c.at)));
        for g in &c.groups {
            let Some(age) = days_ago(&g.date, now) else {
                continue;
            };
            if !(0..=WINDOW_DAYS).contains(&age) || !seen.insert(g.mbid.clone()) {
                continue;
            }
            let in_library = Mbid::parse(&g.mbid)
                .ok()
                .and_then(|m| lib.album_by_release_group_mbid(m).ok().flatten());
            items.push(NewRelease {
                artist_id: a.id,
                artist: a.name.clone(),
                release_group_mbid: g.mbid.clone(),
                title: g.title.clone(),
                first_release_date: g.date.clone(),
                kind: g.kind.clone(),
                url: format!("https://musicbrainz.org/release-group/{}", g.mbid),
                in_library,
            });
        }
    }
    drop(lib);
    items.sort_by(|a, b| b.first_release_date.cmp(&a.first_release_date));
    if stale > 0 && !s.contact().is_empty() {
        kick_refresh(Arc::clone(s));
    }
    Ok(NewReleasesPage {
        items,
        artists_total: total,
        artists_checked: checked,
        refreshing: refreshing().load(Ordering::SeqCst),
        refreshed_at,
    })
}

/// Start the background refresh unless one is running.
pub(crate) fn kick_refresh(s: Arc<NodeState>) -> bool {
    if refreshing().swap(true, Ordering::SeqCst) {
        return false;
    }
    tokio::spawn(async move {
        if let Err(e) = refresh(&s).await {
            tracing::warn!("new releases refresh: {e}");
        }
        refreshing().store(false, Ordering::SeqCst);
    });
    true
}

async fn refresh(s: &Arc<NodeState>) -> Result<(), pm_library::LibraryError> {
    let now = pm_library::db::now_unix();
    let todo: Vec<(i64, Mbid)> = {
        let lib = s.library.lock().expect("library poisoned");
        let mut todo = Vec::new();
        for a in lib.list_artists()? {
            let Some(mbid) = a.mbid else { continue };
            let fresh = lib
                .get_setting(&key(a.id))?
                .and_then(|j| serde_json::from_str::<CachedGroups>(&j).ok())
                .is_some_and(|c| now - c.at <= ARTIST_TTL_SECS);
            if !fresh {
                todo.push((a.id, mbid));
            }
        }
        todo
    };
    if todo.is_empty() {
        return Ok(());
    }
    tracing::info!(artists = todo.len(), "new releases: refreshing from MusicBrainz");
    let base = s.info_bases.read().expect("info bases").musicbrainz.clone();
    let http = s.http_client();
    for (artist_id, mbid) in todo {
        match fetch_groups(&http, &base, mbid).await {
            Ok(groups) => {
                let c = CachedGroups {
                    at: pm_library::db::now_unix(),
                    groups,
                };
                let lib = s.library.lock().expect("library poisoned");
                lib.set_setting(&key(artist_id), &serde_json::to_string(&c).expect("serializes"))?;
            }
            Err(e) => tracing::warn!(artist = artist_id, "new releases: {e}"),
        }
        tokio::time::sleep(PACE).await;
    }
    s.notify_library();
    Ok(())
}

/// Browse an artist's release groups (albums, EPs and singles; no
/// compilations or live records), up to three pages of 100.
async fn fetch_groups(http: &reqwest::Client, base: &str, artist: Mbid) -> Result<Vec<Group>, String> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for _ in 0..3 {
        // The ws/2 API: without it this asked MusicBrainz's website and never had an answer
        let url = format!(
            "{}/ws/2/release-group?artist={artist}&limit=100&offset={offset}&fmt=json",
            base.trim_end_matches('/')
        );
        let mut turn = pm_library::musicbrainz::take_turn(&url, std::time::Duration::from_secs(1)).await;
        let resp = http
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if matches!(resp.status().as_u16(), 503 | 429) {
            turn.back_off(pm_library::musicbrainz::retry_after(
                resp.headers(),
                std::time::Duration::from_secs(2),
            ));
        }
        drop(turn);
        let v: serde_json::Value = resp
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let groups = v["release-groups"].as_array().cloned().unwrap_or_default();
        let n = groups.len();
        for g in groups {
            let secondary = g["secondary-types"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            let kind = g["primary-type"].as_str().unwrap_or("").to_string();
            if secondary || !matches!(kind.as_str(), "Album" | "EP" | "Single") {
                continue;
            }
            let Some(mbid) = g["id"].as_str() else { continue };
            out.push(Group {
                mbid: mbid.to_string(),
                title: g["title"].as_str().unwrap_or("").to_string(),
                date: g["first-release-date"].as_str().unwrap_or("").to_string(),
                kind,
            });
        }
        offset += n;
        let total = v["release-group-count"].as_u64().unwrap_or(0) as usize;
        if n == 0 || offset >= total {
            break;
        }
        tokio::time::sleep(PACE).await;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_dates_count_from_their_first_day() {
        let now = pm_library::db::now_unix();
        assert_eq!(days_ago("1970-01-02", 86_400 * 2), Some(1));
        assert!(days_ago("2020", now).unwrap() > 365);
        assert!(days_ago("", now).is_none());
        assert!(days_ago("2026-13", now).is_none());
    }
}
