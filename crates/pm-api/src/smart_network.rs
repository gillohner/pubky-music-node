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
use std::collections::HashMap;
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
    /// Their labels on it, in the request's tag order.
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
    let nexus = d.nexus();
    let views =
        futures_util::future::join_all(items.iter().take(100).map(|it| nexus.resource_by_uri(&it.uri))).await;
    let mut out: Vec<NetworkItem> = Vec::new();
    let mut everyone: Vec<String> = Vec::new();
    for (it, view) in items.into_iter().zip(views) {
        let mut by: Vec<(String, Vec<String>)> = Vec::new();
        if let Ok(v) = view {
            for label in &tags {
                if let Some(t) = v.tags.iter().find(|t| &t.label == label) {
                    for who in &t.taggers {
                        match by.iter_mut().find(|(k, _)| k == who) {
                            Some((_, ls)) => ls.push(label.clone()),
                            None => by.push((who.clone(), vec![label.clone()])),
                        }
                    }
                }
            }
        }
        let taggers_count = by.len();
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
    let rules = r
        .rules
        .iter()
        .map(|x| MusicFeedRule {
            field: match x.field.as_str() {
                "bookmark" | "favourite" => "bookmarked".into(),
                f => f.into(),
            },
            op: x.op.clone(),
            value: x.value.clone(),
            to: x.to.clone(),
            extra: Default::default(),
        })
        .collect();
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
