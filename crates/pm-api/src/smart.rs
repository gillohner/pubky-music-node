//! Smart playlists (UX review #9): songs chosen by rules over your library and your own Pubky
//! tags, worked out each time they're opened or played, so they change as the library does.
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

/// One rule. `field`: `genre`, `artist`, `year`, `favourite`, `plays`, `last_played`, `tag`.
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rules {
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// `all` (default) or `any`.
    #[serde(default = "all")]
    pub matches: String,
    /// `random` (default), `most_played`, `least_recent`, `newest`, `title`.
    #[serde(default = "random")]
    pub sort: String,
    /// At most this many songs (default 100, up to 1000).
    #[serde(default = "hundred")]
    pub limit: usize,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct SmartResult {
    /// How many songs match before the limit.
    pub matched: usize,
    pub tracks: Vec<Track>,
}

/// What each rule looks at, gathered once per evaluation.
struct Facts {
    favourites: HashSet<i64>,
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
        "favourite" => {
            let on = rule.value.as_bool().unwrap_or(true);
            f.favourites.contains(&id) == on
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
    if r.rules.is_empty() {
        return Err(ApiError::bad_request("A smart playlist needs at least one rule"));
    }
    if r.rules.len() > 20 {
        return Err(ApiError::bad_request("Up to 20 rules"));
    }
    for rule in &r.rules {
        if !matches!(
            rule.field.as_str(),
            "genre" | "artist" | "year" | "favourite" | "plays" | "last_played" | "tag"
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
    let favs = crate::favourites::collect(&lib)?;
    let facts = Facts {
        favourites: favs.tracks.iter().filter_map(|t| t.id.map(|i| i.0)).collect(),
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
    let mut picked: Vec<Track> = Vec::new();
    let mut after = None;
    loop {
        let page = lib.list_tracks(after, 1000)?;
        if page.is_empty() {
            break;
        }
        after = page.last().and_then(|t| t.id);
        for t in page {
            let ok = if any {
                r.rules.iter().any(|rule| matches(rule, &t, &facts))
            } else {
                r.rules.iter().all(|rule| matches(rule, &t, &facts))
            };
            if ok {
                picked.push(t);
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
    Ok(SmartResult {
        matched,
        tracks: picked,
    })
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
    let p = SmartPlaylist {
        id: crate::tidal::random_urlsafe(9),
        name,
        rules: b.rules,
    };
    store(&s, &p)?;
    Ok(Json(p))
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
    store(&s, &p)?;
    Ok(Json(p))
}

/// `DELETE /v1/smart/{id}`.
pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    load(&s, &id)?;
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
    let r = evaluate(&s, &p.rules)?;
    Ok(Json(
        serde_json::json!({ "playlist": p, "matched": r.matched, "tracks": r.tracks }),
    ))
}

/// `POST /v1/smart/preview {rules, matches, sort, limit}`: what the rules pick, while editing.
pub(crate) async fn preview(State(s): State<Arc<NodeState>>, Json(r): Json<Rules>) -> ApiResult<SmartResult> {
    check(&r)?;
    Ok(Json(evaluate(&s, &r)?))
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
