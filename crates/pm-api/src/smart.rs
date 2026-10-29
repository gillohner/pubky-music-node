//! Smart playlists (UX review #9): songs chosen by rules over your library and your own Pubky
//! tags, worked out each time they're opened or played, so they change as the library does.
//! A playlist can also start from **seeds** (songs, artists, albums, genres; compare-music-assistant
//! #13): the pool is then the songs the library finds similar to them (the "keep playing" logic),
//! the rules narrow it, and a length in minutes and exclusions (artists, genres, songs, recently
//! played) trim it.
//! The rules are kept on the node (`smart.<id>`) and travel to the homeserver with the owner's
//! other settings; no new public object type.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::Json;
use pm_model::Track;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const KEY_PREFIX: &str = "smart.";

/// One rule. `field`: `genre`, `artist`, `year`, `bookmark`, `plays`, `last_played`, `tag`.
/// `favourite` is the older name of `bookmark` (rules saved before 2026-10-26 keep it).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub field: String,
    /// `is`, `contains`, `between`, `at_least`, `at_most`, `within_days`, `not_within_days`.
    #[serde(default)]
    pub op: String,
    #[serde(default)]
    pub value: serde_json::Value,
    /// `between`: the upper end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<serde_json::Value>,
}

/// Something a playlist grows from: `{kind: track|artist|album|genre, id}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Seed {
    pub kind: String,
    pub id: i64,
    /// What the page showed for it (kept for the editor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// What to leave out.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Exclude {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<i64>,
    /// Songs played within this many days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub played_within_days: Option<u32>,
}

impl Exclude {
    fn is_empty(&self) -> bool {
        self.artists.is_empty()
            && self.genres.is_empty()
            && self.tracks.is_empty()
            && self.played_within_days.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rules {
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Start from these; the pool is then what the library finds similar to them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeds: Vec<Seed>,
    /// Stop adding songs once this many minutes are reached (after sorting).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Exclude::is_empty")]
    pub exclude: Exclude,
    /// `all` (default) or `any`.
    #[serde(default = "all")]
    pub matches: String,
    /// `random` (default), `most_played`, `least_recent`, `newest`, `title`.
    #[serde(default = "random")]
    pub sort: String,
    /// At most this many songs (default 100, up to 1000).
    #[serde(default = "hundred")]
    pub limit: usize,
    /// Whose tags count (TASK-pinakes-social-ux.md E2): `me` (the default: the library alone),
    /// `following`, `friends` or `all`, with `tags` and `match` (smart_network.rs).
    #[serde(default = "me", skip_serializing_if = "is_me")]
    pub reach: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `any` (default) or `all` of `tags`.
    #[serde(default = "any", rename = "match", skip_serializing_if = "is_any")]
    pub tags_match: String,
    /// How many network items a preview lists (30 by default, up to 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_limit: Option<usize>,
}

fn me() -> String {
    "me".into()
}
fn is_me(r: &str) -> bool {
    r == "me"
}
fn any() -> String {
    "any".into()
}
fn is_any(m: &str) -> bool {
    m == "any"
}

fn all() -> String {
    "all".into()
}
fn random() -> String {
    "random".into()
}
fn hundred() -> usize {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SmartPlaylist {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub rules: Rules,
    /// Its music feed on the homeserver, for one with tags from the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<crate::smart_network::FeedRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmartResult {
    /// How many songs match before the limit.
    pub matched: usize,
    pub tracks: Vec<Track>,
    /// Music people in the reach tagged (`reach` other than `me`): playable here first, then
    /// cards.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<crate::smart_network::NetworkItem>,
    /// Why `network` is empty when it was asked for, in plain words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_note: Option<String>,
}

/// What each rule looks at, gathered once per evaluation.
struct Facts {
    bookmarks: HashSet<i64>,
    plays: HashMap<i64, (u64, i64)>,
    tags: HashMap<String, HashSet<String>>,
    now: i64,
}

fn num(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn text(v: &serde_json::Value) -> String {
    v.as_str().unwrap_or_default().trim().to_lowercase()
}

fn matches(rule: &Rule, t: &Track, f: &Facts) -> bool {
    let id = t.id.map(|i| i.0).unwrap_or_default();
    match rule.field.as_str() {
        "genre" => {
            let want = text(&rule.value);
            !want.is_empty()
                && t.genres.iter().any(|g| {
                    let g = g.to_lowercase();
                    if rule.op == "is" {
                        g == want
                    } else {
                        g.contains(&want)
                    }
                })
        }
        "artist" => {
            let want = text(&rule.value);
            !want.is_empty() && t.artists.iter().any(|a| a.name.to_lowercase().contains(&want))
        }
        "year" => {
            let y = t.album.as_ref().and_then(|a| a.year).map(i64::from);
            let (lo, hi) = (num(&rule.value), rule.to.as_ref().and_then(num));
            y.is_some_and(|y| lo.is_none_or(|lo| y >= lo) && hi.is_none_or(|hi| y <= hi))
        }
        "bookmark" | "favourite" => {
            let on = rule.value.as_bool().unwrap_or(true);
            f.bookmarks.contains(&id) == on
        }
        "plays" => {
            let n = f.plays.get(&id).map(|p| p.0).unwrap_or(0) as i64;
            let v = num(&rule.value).unwrap_or(0);
            if rule.op == "at_most" {
                n <= v
            } else {
                n >= v
            }
        }
        "last_played" => {
            let last = f.plays.get(&id).map(|p| p.1);
            let days = num(&rule.value).unwrap_or(30);
            let within = last.is_some_and(|l| f.now - l <= days * 86_400);
            if rule.op == "not_within_days" {
                !within
            } else {
                within
            }
        }
        "tag" => {
            let want = text(&rule.value).trim_start_matches('#').to_string();
            t.publishable_uri()
                .and_then(|u| f.tags.get(&u.to_url()))
                .is_some_and(|labels| labels.contains(&want))
        }
        _ => false,
    }
}

fn check(r: &Rules) -> Result<(), ApiError> {
    crate::smart_network::check(r)?;
    if r.rules.is_empty() && r.seeds.is_empty() && r.reach == "me" {
        return Err(ApiError::bad_request(
            "A smart playlist needs at least one rule or a seed",
        ));
    }
    if r.rules.len() > 20 {
        return Err(ApiError::bad_request("Up to 20 rules"));
    }
    for rule in &r.rules {
        if !matches!(
            rule.field.as_str(),
            "genre" | "artist" | "year" | "bookmark" | "favourite" | "plays" | "last_played" | "tag"
        ) {
            return Err(ApiError::bad_request(format!(
                "No rule about \u{201c}{}\u{201d}",
                rule.field
            )));
        }
    }
    Ok(())
}

/// The songs these rules pick right now.
pub fn evaluate(s: &NodeState, r: &Rules) -> Result<SmartResult, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let marked = crate::bookmarks::collect(&lib)?;
    let facts = Facts {
        bookmarks: marked.tracks.iter().filter_map(|t| t.id.map(|i| i.0)).collect(),
        plays: lib.play_stats()?,
        tags: {
            let mut m: HashMap<String, HashSet<String>> = HashMap::new();
            for t in lib.list_tags(None)? {
                m.entry(t.uri).or_default().insert(t.label);
            }
            m
        },
        now: pm_library::db::now_unix(),
    };
    let any = r.matches == "any";
    let keep = |t: &Track| -> bool {
        let rules_ok = r.rules.is_empty()
            || if any {
                r.rules.iter().any(|rule| matches(rule, t, &facts))
            } else {
                r.rules.iter().all(|rule| matches(rule, t, &facts))
            };
        rules_ok && !excluded(&r.exclude, t, &facts)
    };
    let mut picked: Vec<Track> = Vec::new();
    if r.seeds.is_empty() {
        let mut after = None;
        loop {
            let page = lib.list_tracks(after, 1000)?;
            if page.is_empty() {
                break;
            }
            after = page.last().and_then(|t| t.id);
            for t in page {
                if keep(&t) {
                    picked.push(t);
                }
            }
        }
    } else {
        // The pool: what the library finds similar to the seeds (the "keep playing" logic), the
        // seeds themselves first; each seed contributes up to its share of the limit
        let seed_tracks = seed_tracks(&lib, &r.seeds)?;
        let per_seed = (r.limit.clamp(1, 1000) * 2 / seed_tracks.len().max(1)).clamp(5, 200);
        let mut seen: HashSet<i64> = HashSet::new();
        let mut consider = |t: Track, picked: &mut Vec<Track>| {
            let Some(id) = t.id.map(|i| i.0) else { return };
            if seen.insert(id) && !r.exclude.tracks.contains(&id) && keep(&t) {
                picked.push(t);
            }
        };
        for t in &seed_tracks {
            consider(t.clone(), &mut picked);
        }
        let exclude_ids: Vec<pm_model::TrackId> = seed_tracks.iter().filter_map(|t| t.id).collect();
        for seed in &seed_tracks {
            for t in lib.similar_tracks(seed, &exclude_ids, per_seed)? {
                consider(t, &mut picked);
            }
        }
    }
    let matched = picked.len();
    let plays = |t: &Track| {
        facts
            .plays
            .get(&t.id.map(|i| i.0).unwrap_or_default())
            .copied()
            .unwrap_or((0, 0))
    };
    match r.sort.as_str() {
        "most_played" => picked.sort_by_key(|t| std::cmp::Reverse(plays(t).0)),
        "least_recent" => picked.sort_by_key(|t| plays(t).1),
        "newest" => {
            picked.sort_by_key(|t| std::cmp::Reverse(t.album.as_ref().and_then(|a| a.year).unwrap_or(0)))
        }
        "title" => picked.sort_by_key(|a| a.title.to_lowercase()),
        _ => picked.shuffle(&mut rand::thread_rng()),
    }
    picked.truncate(r.limit.clamp(1, 1000));
    if let Some(min) = r.minutes.filter(|m| *m > 0) {
        let budget = u64::from(min) * 60_000;
        let mut total = 0u64;
        let mut n = 0;
        for t in &picked {
            if total >= budget {
                break;
            }
            total += t.duration_ms.unwrap_or(240_000);
            n += 1;
        }
        picked.truncate(n.max(1));
    }
    Ok(SmartResult {
        matched,
        tracks: picked,
        network: Vec::new(),
        network_note: None,
    })
}

/// The songs behind the seeds: a song as itself; an artist's, an album's or a genre's songs (a
/// handful of each, so one big artist doesn't drown the rest).
fn seed_tracks(lib: &pm_library::Library, seeds: &[Seed]) -> Result<Vec<Track>, ApiError> {
    let mut out = Vec::new();
    for s in seeds {
        match s.kind.as_str() {
            "track" => out.extend(lib.get_track(pm_model::TrackId(s.id))?),
            "artist" => out.extend(lib.artist_tracks(s.id)?.into_iter().take(8)),
            "album" => out.extend(lib.album_tracks(s.id)?.into_iter().take(8)),
            "genre" => out.extend(lib.genre_tracks(s.id, 8)?),
            k => {
                return Err(ApiError::bad_request(format!(
                    "seed kind {k}: track, artist, album or genre"
                )))
            }
        }
    }
    Ok(out)
}

fn excluded(x: &Exclude, t: &Track, facts: &Facts) -> bool {
    if x.is_empty() {
        return false;
    }
    if let Some(id) = t.id.map(|i| i.0) {
        if x.tracks.contains(&id) {
            return true;
        }
        if let Some(days) = x.played_within_days {
            if let Some((_, last)) = facts.plays.get(&id) {
                if *last > 0 && facts.now - *last < i64::from(days) * 86_400 {
                    return true;
                }
            }
        }
    }
    let lower = |v: &str| v.to_lowercase();
    if x.artists.iter().any(|a| {
        let a = lower(a);
        t.artists.iter().any(|ta| lower(&ta.name) == a)
            || t.album
                .as_ref()
                .and_then(|al| al.album_artist.as_deref())
                .is_some_and(|aa| lower(aa) == a)
    }) {
        return true;
    }
    if x.genres.iter().any(|g| {
        let g = lower(g);
        t.genres.iter().any(|tg| lower(tg) == g)
    }) {
        return true;
    }
    false
}

fn load_all(s: &NodeState) -> Vec<SmartPlaylist> {
    let mut v: Vec<SmartPlaylist> = s
        .library
        .lock()
        .expect("library poisoned")
        .settings_with_prefix(KEY_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, j)| serde_json::from_str(&j).ok())
        .collect();
    v.sort_by_key(|a| a.name.to_lowercase());
    v
}

fn load(s: &NodeState, id: &str) -> Result<SmartPlaylist, ApiError> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(&format!("{KEY_PREFIX}{id}"))?
        .and_then(|j| serde_json::from_str(&j).ok())
        .ok_or_else(|| ApiError::not_found("no such smart playlist"))
}

fn store(s: &NodeState, p: &SmartPlaylist) -> Result<(), ApiError> {
    s.library.lock().expect("library poisoned").set_setting(
        &format!("{KEY_PREFIX}{}", p.id),
        &serde_json::to_string(p).expect("serializes"),
    )?;
    Ok(())
}

/// `GET /v1/smart`.
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SmartPlaylist>> {
    Ok(Json(load_all(&s)))
}

#[derive(Debug, Deserialize)]
pub struct SaveSmart {
    pub name: String,
    #[serde(flatten)]
    pub rules: Rules,
}

/// `POST /v1/smart {name, rules, matches, sort, limit}`.
pub(crate) async fn create(
    State(s): State<Arc<NodeState>>,
    Json(b): Json<SaveSmart>,
) -> ApiResult<SmartPlaylist> {
    check(&b.rules)?;
    let name = b.name.trim().chars().take(100).collect::<String>();
    if name.is_empty() {
        return Err(ApiError::bad_request("Give it a name"));
    }
    let mut p = SmartPlaylist {
        id: crate::tidal::random_urlsafe(9),
        name,
        rules: b.rules,
        feed: None,
    };
    sync_feed(&s, &mut p).await?;
    store(&s, &p)?;
    Ok(Json(p))
}

/// With tags from the network the smart playlist is also a music feed on the homeserver; back to
/// `me` its feed goes.
async fn sync_feed(s: &Arc<NodeState>, p: &mut SmartPlaylist) -> Result<(), ApiError> {
    if p.rules.reach != "me" {
        let f = crate::smart_network::save_feed(s, p)
            .await
            .map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, e))?;
        p.feed = Some(f);
    } else if let Some(f) = p.feed.take() {
        if let Err(e) = crate::smart_network::delete_feed(s, &f).await {
            tracing::warn!(error = %e, "removing a smart playlist's music feed");
        }
    }
    Ok(())
}

/// `PUT /v1/smart/{id}`.
pub(crate) async fn update(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(b): Json<SaveSmart>,
) -> ApiResult<SmartPlaylist> {
    check(&b.rules)?;
    let mut p = load(&s, &id)?;
    p.name = b.name.trim().chars().take(100).collect();
    p.rules = b.rules;
    sync_feed(&s, &mut p).await?;
    store(&s, &p)?;
    Ok(Json(p))
}

/// `DELETE /v1/smart/{id}`.
pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let p = load(&s, &id)?;
    if let Some(f) = &p.feed {
        if let Err(e) = crate::smart_network::delete_feed(&s, f).await {
            tracing::warn!(error = %e, "removing a smart playlist's music feed");
        }
    }
    s.library
        .lock()
        .expect("library poisoned")
        .delete_setting(&format!("{KEY_PREFIX}{id}"))?;
    Ok(Json(serde_json::json!({ "deleted": id })))
}

/// `GET /v1/smart/{id}/tracks`: its songs right now.
pub(crate) async fn tracks(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let p = load(&s, &id)?;
    let r = with_network(&s, &p.rules).await?;
    Ok(Json(
        serde_json::json!({ "playlist": p, "matched": r.matched, "tracks": r.tracks, "network": r.network, "network_note": r.network_note }),
    ))
}

/// `POST /v1/smart/preview {rules, matches, sort, limit}`: what the rules pick, while editing.
pub(crate) async fn preview(State(s): State<Arc<NodeState>>, Json(r): Json<Rules>) -> ApiResult<SmartResult> {
    check(&r)?;
    Ok(Json(with_network(&s, &r).await?))
}

/// The library's songs, and with a network reach what people in it tagged.
async fn with_network(s: &Arc<NodeState>, r: &Rules) -> Result<SmartResult, ApiError> {
    let mut out = if r.rules.is_empty() && r.seeds.is_empty() {
        SmartResult {
            matched: 0,
            tracks: Vec::new(),
            network: Vec::new(),
            network_note: None,
        }
    } else {
        evaluate(s, r)?
    };
    match crate::smart_network::network(s, r).await {
        Ok(items) => out.network = items,
        Err(note) => out.network_note = Some(note),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_parse_with_defaults() {
        let r: Rules = serde_json::from_value(
            serde_json::json!({"rules": [{"field": "genre", "op": "contains", "value": "techno"}]}),
        )
        .unwrap();
        assert_eq!(
            (r.matches.as_str(), r.sort.as_str(), r.limit),
            ("all", "random", 100)
        );
        assert!(check(&r).is_ok());
        let bad: Rules =
            serde_json::from_value(serde_json::json!({"rules": [{"field": "mood", "value": "x"}]})).unwrap();
        assert!(check(&bad).is_err());
    }
}
