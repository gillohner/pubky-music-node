//! Smart playlists with tags from the network (TASK-pinakes-social-ux.md E2): "Tags from" (`reach`:
//! `following`, `friends` or `all`) with `tags` and `match` finds music people in that reach
//! tagged, through Pinakes' generic search (`/v1/search?tags=…&reach=…&viewer=…`), and who tagged
//! it through Nexus' resource views by URI. Each item says where it would play from here (the
//! library, a connected service), else it is a card. `reach: me` (the default) is the library
//! alone, as before.
//!
//! Saving such a smart playlist also writes it as a music feed (`pubky-music-specs` `MusicFeed`,
//! `/priv/music/v1/feeds/{id}.json`) to the owner's homeserver, so other clients can read it.

use crate::smart::{Rules, SmartPlaylist};
use crate::state::NodeState;
use crate::types::PlaysFrom;
use crate::ApiError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The reaches "Tags from" offers.
pub const REACHES: [&str; 4] = ["me", "following", "friends", "all"];
/// How long one network answer is reused (the editor previews on every change).
const CACHE: Duration = Duration::from_secs(300);
/// Most taggers named per item.
const TAGGERS_SHOWN: usize = 5;

/// Someone who tagged a network item.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Tagger {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Their labels on it, in the tags' canonical (sorted) order.
    pub labels: Vec<String>,
}

/// One item from the network.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NetworkItem {
    pub uri: String,
    /// `track`, `album`, `artist` or `playlist`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The service the card is from (`tidal.com`, `open.spotify.com`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// The card's own page (a service URL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// Where Play takes it from here; none: show it as a card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plays_from: Option<PlaysFrom>,
    /// Up to five, with their labels.
    pub tagged_by: Vec<Tagger>,
    /// Everyone who tagged it with one of the labels.
    pub taggers_count: usize,
}

/// The smart playlist's music feed on the homeserver.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeedRef {
    pub id: String,
    /// `pubky://<owner>/priv/music/v1/feeds/<id>.json`, or the `/pub/` copy once published.
    pub uri: String,
    pub public: bool,
}

/// Checks `reach`, `tags`, `match` and `network_limit`.
pub(crate) fn check(r: &Rules) -> Result<(), ApiError> {
    if !REACHES.contains(&r.reach.as_str()) {
        return Err(ApiError::bad_request("Tags from: me, following, friends or all"));
    }
    if r.reach == "me" && !r.tags.is_empty() {
        return Err(ApiError::bad_request(
            "With your own tags only, use a tag rule; tags go with people you follow, friends or everyone",
        ));
    }
    if r.reach != "me" && r.tags.is_empty() {
        return Err(ApiError::bad_request(
            "Tags from the network need at least one tag",
        ));
    }
    if pubky_music_specs::feed_tags(&r.tags)
        .map_err(ApiError::bad_request)?
        .is_some_and(|t| t.len() > pubky_social_specs::VALIDATION_LIMITS.feed_tags_max_count)
    {
        return Err(ApiError::bad_request("Up to 5 tags"));
    }
    if !matches!(r.tags_match.as_str(), "any" | "all") {
        return Err(ApiError::bad_request("match is any or all"));
    }
    if r.tags_match == "all" && r.tags.len() > 3 {
        return Err(ApiError::bad_request("Every tag at once works with up to 3 tags"));
    }
    if r.network_limit.is_some_and(|n| n == 0 || n > 100) {
        return Err(ApiError::bad_request("network_limit is 1 to 100"));
    }
    Ok(())
}

type CacheKey = (Vec<String>, String, String, Option<String>);
type Cache = Mutex<HashMap<CacheKey, (Instant, Vec<NetworkItem>)>>;

fn cache() -> &'static Cache {
    static C: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

fn raw_key(z32: &str) -> &str {
    z32.strip_prefix("pubky").filter(|k| k.len() == 52).unwrap_or(z32)
}

/// Who tagged an item, each with the labels they used.
type TaggedBy = Vec<(String, Vec<String>)>;

/// The music people in the rules' reach tagged with the rules' tags; `Err` says in plain words
/// why there is none (no Pubky sign-in for a personal reach, Pinakes unreachable).
pub(crate) async fn network(s: &Arc<NodeState>, r: &Rules) -> Result<Vec<NetworkItem>, String> {
    if r.reach == "me" || r.tags.is_empty() {
        return Ok(Vec::new());
    }
    let tags = pubky_music_specs::feed_tags(&r.tags)?.unwrap_or_default();
    let viewer = match r.reach.as_str() {
        "all" => None,
        _ => Some(
            s.sync_handle()
                .map(|h| raw_key(h.z32()).to_string())
                .ok_or("Sign in with Pubky to use tags from people you follow")?,
        ),
    };
    let limit = r.network_limit.unwrap_or(30);
    let key: CacheKey = (
        tags.clone(),
        r.tags_match.clone(),
        r.reach.clone(),
        viewer.clone(),
    );
    if let Some((at, items)) = cache().lock().expect("network cache").get(&key) {
        if at.elapsed() < CACHE {
            return Ok(items.iter().take(limit).cloned().collect());
        }
    }
    let d = s
        .discovery()
        .ok_or("Pinakes isn't set up on this node (Settings › Discover)")?;
    let query = pm_pubky::pinakes::SearchQuery {
        tags: Some(tags.join(",")),
        match_: (r.tags_match == "all").then(|| "all".to_string()),
        reach: (r.reach != "all").then(|| r.reach.clone()),
        viewer: viewer.clone(),
        limit: Some(100),
        ..Default::default()
    };
    let found = d
        .pinakes()
        .search(&query)
        .await
        .map_err(|e| format!("Pinakes didn't answer: {e}"))?;
    let cards: Vec<_> = found.items.into_iter().filter(|c| c.is_music()).collect();
    let items = crate::discover::items_for(s, cards);
    // Who tagged each, with which of the labels (Nexus, by URI)
    // At most 8 at a time, each cut short: a slow Nexus never holds the preview long
    let nexus: pm_pubky::nexus::NexusClient = d.nexus().clone();
    let uris: Vec<String> = items.iter().take(100).map(|it| it.uri.clone()).collect();
    let views: Vec<Option<pm_pubky::nexus::ResourceView>> = {
        use futures_util::StreamExt;
        let lookups = uris.into_iter().map(|uri: String| {
            let nexus = nexus.clone();
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_millis(1500),
                    nexus.resource_by_uri(&uri),
                )
                .await
                .ok()
                .and_then(Result::ok)
            }
        });
        futures_util::stream::iter(lookups).buffered(8).collect().await
    };
    // Who tagged what, per item: (tagger, labels), and the most taggers any one label has
    let mut found: Vec<(TaggedBy, u64)> = Vec::new();
    for view in &views {
        let mut by: TaggedBy = Vec::new();
        let mut most = 0;
        if let Some(v) = view {
            for label in &tags {
                if let Some(t) = v.tags.iter().find(|t| &t.label == label) {
                    most = most.max(t.taggers_count.max(t.taggers.len() as u64));
                    for who in &t.taggers {
                        match by.iter_mut().find(|(k, _)| k == who) {
                            Some((_, ls)) => ls.push(label.clone()),
                            None => by.push((who.clone(), vec![label.clone()])),
                        }
                    }
                }
            }
        }
        found.push((by, most));
    }
    // Only people in the reach are named: Nexus's tagger lists are everyone's
    let in_reach: Option<HashSet<String>> = match (r.reach.as_str(), viewer.as_deref()) {
        ("following" | "friends", Some(me)) => {
            let mut ids: Vec<String> = found
                .iter()
                .flat_map(|(by, _)| by.iter().map(|(k, _)| k.clone()))
                .collect();
            ids.sort();
            ids.dedup();
            ids.truncate(500);
            let users = tokio::time::timeout(
                std::time::Duration::from_secs(4),
                nexus.users_by_ids(&ids, Some(me)),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
            Some(
                users
                    .into_iter()
                    .filter(|u| {
                        u.relationship
                            .as_ref()
                            .is_some_and(|rel| rel.following && (r.reach == "following" || rel.followed_by))
                    })
                    .map(|u| u.details.id)
                    .collect(),
            )
        }
        _ => None,
    };
    let mut out: Vec<NetworkItem> = Vec::new();
    let mut everyone: Vec<String> = Vec::new();
    let mut found = found.into_iter();
    for it in items.into_iter() {
        let (mut by, most) = found.next().unwrap_or_default();
        // In a reach: the people in it who tagged it; everyone: the label's own count (Nexus
        // lists only some of the taggers)
        let taggers_count = match &in_reach {
            Some(keep) => {
                by.retain(|(k, _)| keep.contains(k));
                by.len()
            }
            None => (most as usize).max(by.len()),
        };
        let shown: Vec<Tagger> = by
            .into_iter()
            .take(TAGGERS_SHOWN)
            .map(|(key, labels)| Tagger {
                key,
                name: None,
                labels,
            })
            .collect();
        everyone.extend(shown.iter().map(|t| t.key.clone()));
        out.push(NetworkItem {
            uri: it.uri,
            kind: it.kind,
            title: it.title,
            artist: it.artist,
            image: it.image,
            site: it.site,
            page: it.page,
            plays_from: it.plays_from,
            tagged_by: shown,
            taggers_count,
        });
    }
    // Names, in one call
    everyone.sort();
    everyone.dedup();
    if !everyone.is_empty() {
        let names: HashMap<String, String> = nexus
            .users_by_ids(&everyone, viewer.as_deref())
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|u| !u.details.name.trim().is_empty())
            .map(|u| (u.details.id, u.details.name))
            .collect();
        for it in &mut out {
            for t in &mut it.tagged_by {
                t.name = names.get(&t.key).cloned();
            }
        }
    }
    // Playable here first, then the rest (cards)
    out.sort_by_key(|it| it.plays_from.is_none());
    cache()
        .lock()
        .expect("network cache")
        .insert(key, (Instant::now(), out.clone()));
    Ok(out.into_iter().take(limit).collect())
}

/// The smart playlist as a music feed: its rules in the spec's terms (library ids become the
/// songs' canonical URIs; what has none is left out of the feed, not of the playlist).
pub(crate) fn as_feed(
    s: &NodeState,
    p: &SmartPlaylist,
    created_at: i64,
) -> Result<pubky_music_specs::MusicFeed, String> {
    use pubky_music_specs::{
        MusicFeedConfig, MusicFeedContent, MusicFeedLeaveOut, MusicFeedLimit, MusicFeedMatch, MusicFeedRule,
        MusicFeedSort, PubkySocialFeedLayout, PubkySocialFeedReach,
    };
    let r = &p.rules;
    let lib = s.library.lock().expect("library poisoned");
    let track_uri = |id: i64| {
        lib.get_track(pm_model::TrackId(id))
            .ok()
            .flatten()
            .and_then(|t| t.publishable_uri().map(|u| u.to_url()))
    };
    let seeds = r
        .seeds
        .iter()
        .filter_map(|sd| match sd.kind.as_str() {
            "track" => track_uri(sd.id),
            "album" => lib
                .get_album(sd.id)
                .ok()
                .flatten()
                .and_then(|a| a.release_mbid)
                .map(|m| pm_model::MusicUri::Release { mbid: m }.to_url()),
            "artist" => lib
                .get_artist(sd.id)
                .ok()
                .flatten()
                .and_then(|a| a.mbid)
                .map(|m| pm_model::MusicUri::Artist { mbid: m }.to_url()),
            _ => None,
        })
        .collect();
    let leave_out = (!r.exclude.artists.is_empty()
        || !r.exclude.genres.is_empty()
        || !r.exclude.tracks.is_empty()
        || r.exclude.played_within_days.is_some())
    .then(|| MusicFeedLeaveOut {
        artists: r.exclude.artists.clone(),
        genres: r.exclude.genres.clone(),
        uris: r.exclude.tracks.iter().filter_map(|id| track_uri(*id)).collect(),
        played_within_days: r.exclude.played_within_days,
        extra: Default::default(),
    });
    drop(lib);
    let rules: Vec<MusicFeedRule> = r.rules.iter().map(feed_rule).collect();
    // Any of several rules: said so, or a reader would apply them all
    let rules_match = (r.matches == "any" && rules.len() > 1).then_some(MusicFeedMatch::Any);
    let config = MusicFeedConfig {
        tags: Some(r.tags.clone()).filter(|t| !t.is_empty()),
        tags_match: if r.tags_match == "all" {
            MusicFeedMatch::All
        } else {
            MusicFeedMatch::Any
        },
        reach: PubkySocialFeedReach::from(r.reach.clone()),
        sort: match r.sort.as_str() {
            "newest" => MusicFeedSort::Recent,
            "most_played" => MusicFeedSort::Popularity,
            _ => MusicFeedSort::Random,
        },
        content: MusicFeedContent::Songs,
        layout: PubkySocialFeedLayout::List,
        rules,
        rules_match,
        leave_out,
        limit: Some(MusicFeedLimit {
            songs: Some(r.limit.clamp(1, 1000) as u32),
            minutes: r.minutes.map(|m| m.clamp(1, 1440)),
        }),
        seeds,
        extra: Default::default(),
    };
    let feed = pubky_music_specs::MusicFeed::new(config, &p.name, Some("list-music"), created_at)?;
    Ok(feed)
}

/// A rule as the feed says it, with the op the node applies: the node takes a rule without one
/// (or a range with no upper end), the spec doesn't, and a save must not fail on it.
fn feed_rule(x: &crate::smart::Rule) -> pubky_music_specs::MusicFeedRule {
    let field = match x.field.as_str() {
        "bookmark" | "favourite" => "bookmarked".to_string(),
        f => f.to_string(),
    };
    let to = x.to.clone().filter(|t| !t.is_null());
    let (op, value, to) = match (field.as_str(), x.op.as_str()) {
        // A year range: both ends, from, or up to
        ("year", "" | "between" | "at_least" | "at_most") => match (x.value.is_null(), to) {
            (false, Some(to)) => ("between", x.value.clone(), Some(to)),
            (true, Some(to)) => ("at_most", to, None),
            _ if x.op == "at_most" => ("at_most", x.value.clone(), None),
            _ => ("at_least", x.value.clone(), None),
        },
        (_, "between") if to.is_none() => ("at_least", x.value.clone(), None),
        (_, "") => (
            match field.as_str() {
                "bookmarked" | "tag" | "format" => "is",
                "plays" => "at_least",
                "last_played" => "within_days",
                _ => "contains",
            },
            x.value.clone(),
            to,
        ),
        (_, op) => (op, x.value.clone(), to),
    };
    pubky_music_specs::MusicFeedRule {
        field,
        op: op.to_string(),
        value,
        to,
        extra: Default::default(),
    }
}

/// Writes (or rewrites) the smart playlist's music feed; answers where it is.
pub(crate) async fn save_feed(s: &Arc<NodeState>, p: &SmartPlaylist) -> Result<FeedRef, String> {
    let sync = s
        .sync_handle()
        .ok_or("Sign in with Pubky to save tags from the network as a feed")?;
    let micros = pm_pubky::ids::now_micros();
    let id = p
        .feed
        .as_ref()
        .map(|f| f.id.clone())
        .unwrap_or_else(|| pm_pubky::v1::id_of(micros));
    let feed = as_feed(s, p, micros)?;
    feed.validate(Some(&id), true)?;
    let (private, _) = pubky_music_specs::MusicFeed::paths(&id)?;
    sync.homeserver()
        .write_object(sync.session(), &private, &feed)
        .await
        .map_err(|e| format!("your homeserver: {e}"))?;
    Ok(FeedRef {
        uri: format!("pubky://{}{private}", raw_key(sync.z32())),
        id,
        public: false,
    })
}

/// Deletes the smart playlist's music feed (both copies).
pub(crate) async fn delete_feed(s: &NodeState, f: &FeedRef) -> Result<(), String> {
    let sync = s.sync_handle().ok_or("not signed in with Pubky")?;
    let (private, public) = pubky_music_specs::MusicFeed::paths(&f.id)?;
    for path in [private, public] {
        if let Err(e) = sync.homeserver().delete(sync.session(), &path).await {
            if !e.to_string().contains("404") {
                return Err(format!("your homeserver: {e}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::feed_rule;
    use crate::smart::Rule;
    use serde_json::json;

    fn rule(v: serde_json::Value) -> Rule {
        serde_json::from_value(v).unwrap()
    }

    /// Review of #159, M1: a rule the node takes (no op; a range with no upper end) is one the
    /// spec takes too, with the op the node applies, so a save doesn't fail.
    #[test]
    fn every_rule_the_node_takes_is_one_the_feed_takes() {
        for (given, op, value, to) in [
            (
                json!({"field": "genre", "value": "techno"}),
                "contains",
                json!("techno"),
                None,
            ),
            (
                json!({"field": "favourite", "value": true}),
                "is",
                json!(true),
                None,
            ),
            (json!({"field": "plays", "value": 3}), "at_least", json!(3), None),
            (
                json!({"field": "last_played", "value": 30}),
                "within_days",
                json!(30),
                None,
            ),
            (
                json!({"field": "year", "op": "between", "value": 1990, "to": null}),
                "at_least",
                json!(1990),
                None,
            ),
            (
                json!({"field": "year", "op": "between", "value": 1990, "to": 1999}),
                "between",
                json!(1990),
                Some(json!(1999)),
            ),
            (
                json!({"field": "year", "value": null, "to": 1999}),
                "at_most",
                json!(1999),
                None,
            ),
            (
                json!({"field": "plays", "op": "between", "value": 2}),
                "at_least",
                json!(2),
                None,
            ),
            (
                json!({"field": "plays", "op": "at_most", "value": 2}),
                "at_most",
                json!(2),
                None,
            ),
        ] {
            let r = feed_rule(&rule(given.clone()));
            assert_eq!((r.op.as_str(), &r.value, &r.to), (op, &value, &to), "{given}");
            let feed: pubky_music_specs::MusicFeed = serde_json::from_value(json!({
                "feed": {"reach": "following", "sort": "random", "content": "songs", "layout": "list",
                         "tags": ["night"], "rules": [r]},
                "name": "x", "created_at": 1
            }))
            .unwrap();
            feed.validate(None, true)
                .unwrap_or_else(|e| panic!("{given}: {e}"));
        }
    }
}
