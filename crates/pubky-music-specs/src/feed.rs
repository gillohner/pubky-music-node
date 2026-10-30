//! Music feeds: a saved query for music, at `/{priv,pub}/music/v1/feeds/{id}.json`, modelled on
//! the social v1 feed (`PubkySocialFeed`): a name, an icon, tags (`any` or `all` of them), whose
//! tags (`reach`: `me`, `following`, `friends`, `all`, …), a sort, the kind of music it lists and
//! a layout, with the same spellings where they overlap. On top, the rules a smart playlist has:
//! field rules (genre, artist, year, format, …), what to leave out, a limit in songs or minutes,
//! and seed URIs.
//!
//! Two differences from the social feed, both because a music feed is edited where it is (a
//! smart playlist's rules change): its id is a timestamp id like a post's, not a hash of its
//! config, and one config may be saved twice under two names.
//!
//! A feed is private by default; publishing PUTs the same bytes at the public path (nothing in a
//! feed names a root), unpublishing deletes that copy. A feed names no app, and everything in it
//! is generic: what a reader can't apply (a rule field it doesn't know, a sort it doesn't have)
//! it leaves out and says so, it never misreads.

use pubky_social_specs::{
    ascii_fold, canonicalize_universal, code_point_len, frozen_trim, namespace_path, sanitize_tag_label,
    validate_tag_label, validate_timestamp_id_format, PubkySocialFeedLayout, PubkySocialFeedReach, Root,
    VALIDATION_LIMITS,
};
use serde::{Deserialize, Serialize};

/// Most field rules in one feed.
pub const FEED_RULES_MAX: usize = 50;
/// Most seed URIs, and most left-out URIs.
pub const FEED_URIS_MAX: usize = 500;
/// Most songs a feed lists.
pub const FEED_SONGS_MAX: u32 = 1000;
/// Most minutes a feed lists (a day).
pub const FEED_MINUTES_MAX: u32 = 1440;
/// The rule fields this version applies; others are kept and left out by readers.
pub const FEED_RULE_FIELDS: [&str; 9] = [
    "genre",
    "artist",
    "album",
    "year",
    "format",
    "tag",
    "plays",
    "last_played",
    "bookmarked",
];
/// The rule operators this version applies.
pub const FEED_RULE_OPS: [&str; 7] = [
    "is",
    "contains",
    "between",
    "at_least",
    "at_most",
    "within_days",
    "not_within_days",
];

macro_rules! open_enum {
    ($(#[$m:meta])* $name:ident { $($(#[doc = $d:literal])* $v:ident => $s:literal),* $(,)? }) => {
        $(#[$m])*
        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
        #[serde(from = "String", into = "String")]
        #[non_exhaustive]
        pub enum $name {
            $($(#[doc = $d])* $v,)*
            /// A spelling this version doesn't know, kept as read.
            Unknown(String),
        }
        impl From<String> for $name {
            fn from(s: String) -> Self {
                match s.as_str() {
                    $($s => $name::$v,)*
                    _ => $name::Unknown(s),
                }
            }
        }
        impl From<$name> for String {
            fn from(v: $name) -> String {
                match v {
                    $($name::$v => $s.to_string(),)*
                    $name::Unknown(s) => s,
                }
            }
        }
    };
}

open_enum!(
    /// How a feed orders what it finds.
    MusicFeedSort {
        /// Newest first (by when it was tagged or added).
        Recent => "recent",
        /// Most tagged first, by the tags in the feed's reach.
        Popularity => "popularity",
        /// Shuffled.
        Random => "random",
    }
);

open_enum!(
    /// What a feed lists.
    MusicFeedContent {
        /// Songs (recordings).
        Songs => "songs",
        /// Albums (releases).
        Albums => "albums",
        /// Artists.
        Artists => "artists",
        /// Playlists and collections.
        Playlists => "playlists",
        /// Listening sessions others can join.
        Sessions => "sessions",
    }
);

open_enum!(
    /// Whether an item needs `any` of the feed's tags or `all` of them.
    MusicFeedMatch {
        /// Any one of the tags.
        Any => "any",
        /// Every tag.
        All => "all",
    }
);

// The macro builds the enum, so `#[default]` can't go on the variant
#[allow(clippy::derivable_impls)]
impl Default for MusicFeedMatch {
    fn default() -> Self {
        MusicFeedMatch::Any
    }
}

/// One field rule: `{field, op, value, to?}`, as a smart playlist has them (`year between 1990
/// and 1999`, `genre contains techno`, `last_played not_within_days 30`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MusicFeedRule {
    /// What the rule looks at ([`FEED_RULE_FIELDS`]).
    pub field: String,
    /// How it compares ([`FEED_RULE_OPS`]).
    pub op: String,
    /// What it compares with: a text, a number or a year.
    #[serde(default)]
    pub value: serde_json::Value,
    /// `between`: the upper end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<serde_json::Value>,
    /// Unknown members, kept on rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// What a feed leaves out.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct MusicFeedLeaveOut {
    /// Artists by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    /// Genres by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Songs, albums or artists by their canonical URI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uris: Vec<String>,
    /// Songs the reader played within this many days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub played_within_days: Option<u32>,
    /// Unknown members, kept on rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// How much a feed lists: at most `songs`, or until `minutes` are reached (both: whichever comes
/// first).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct MusicFeedLimit {
    /// At most this many songs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub songs: Option<u32>,
    /// Until this many minutes are reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u32>,
}

/// What a music feed finds and how it shows it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MusicFeedConfig {
    /// Canonical as stored: folded labels, deduplicated, sorted by code point, never empty (the
    /// social feed's rules). `None` is no tag filter.
    pub tags: Option<Vec<String>>,
    /// `any` (the default) or `all` of `tags`.
    #[serde(default, rename = "match")]
    pub tags_match: MusicFeedMatch,
    /// Whose tags count: the social feed's reach.
    pub reach: PubkySocialFeedReach,
    /// The order of what it finds.
    pub sort: MusicFeedSort,
    /// What it lists.
    pub content: MusicFeedContent,
    /// The social feed's layouts.
    pub layout: PubkySocialFeedLayout,
    /// Field rules, all of which an item meets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<MusicFeedRule>,
    /// What to leave out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leave_out: Option<MusicFeedLeaveOut>,
    /// How much to list; none is the reader's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<MusicFeedLimit>,
    /// Music to start from (canonical URIs): a reader lists what is like them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeds: Vec<String>,
    /// Unknown members, kept on rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A music feed: its config, a name, an icon and when it was made.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MusicFeed {
    /// What it finds and how it shows it.
    pub feed: MusicFeedConfig,
    /// 1 to 100 characters, trimmed.
    pub name: String,
    /// A Lucide icon name, as on a social feed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Unix microseconds.
    pub created_at: i64,
    /// Unknown members, kept on rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The canonical tag list of a feed: each label sanitized, deduplicated and sorted; empty is
/// none. An invalid label is an error.
pub fn feed_tags(labels: &[String]) -> Result<Option<Vec<String>>, String> {
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        let t = sanitize_tag_label(l);
        validate_tag_label(&t).map_err(|e| format!("Validation Error: feed tag {l:?}: {e}"))?;
        out.push(t);
    }
    out.sort();
    out.dedup();
    Ok((!out.is_empty()).then_some(out))
}

impl MusicFeed {
    /// The path segment under the music namespace.
    pub const PATH_SEGMENT: &'static str = "feeds/";

    /// A new feed made now: trims the name, folds the icon, canonicalizes the tags and the URIs
    /// (seeds, left-out songs). Validate before writing.
    pub fn new(
        mut feed: MusicFeedConfig,
        name: &str,
        icon: Option<&str>,
        created_at: i64,
    ) -> Result<Self, String> {
        if let Some(tags) = &feed.tags {
            feed.tags = feed_tags(tags)?;
        }
        let canon = |uris: &mut Vec<String>| -> Result<(), String> {
            for u in uris.iter_mut() {
                *u = canonicalize_universal(u.trim())
                    .map_err(|()| format!("Validation Error: {u:?} is not a URI a feed can name"))?;
            }
            uris.dedup();
            Ok(())
        };
        canon(&mut feed.seeds)?;
        if let Some(x) = &mut feed.leave_out {
            canon(&mut x.uris)?;
        }
        Ok(Self {
            feed,
            name: frozen_trim(name).to_string(),
            icon: icon.map(|i| ascii_fold(frozen_trim(i))),
            created_at,
            extra: Default::default(),
        })
    }

    /// `/{root}/music/v1/feeds/{id}.json`. A feed is private; the public copy is the published one.
    pub fn path(root: Root, id: &str) -> String {
        namespace_path(
            root,
            crate::MUSIC_NAMESPACE,
            &format!("{}{id}.json", Self::PATH_SEGMENT),
        )
    }

    /// `(private, public)` paths of the feed `id`, after checking the id.
    pub fn paths(id: &str) -> Result<(String, String), String> {
        validate_timestamp_id_format(id)?;
        Ok((Self::path(Root::Priv, id), Self::path(Root::Pub, id)))
    }

    /// The rules a writer keeps to. A reader that finds a value it doesn't know (a reach, a
    /// sort, a rule field) keeps it and leaves that part out; `strict` refuses unknown values too,
    /// as a writer should.
    pub fn validate(&self, id: Option<&str>, strict: bool) -> Result<(), String> {
        if let Some(id) = id {
            validate_timestamp_id_format(id)?;
        }
        let max = VALIDATION_LIMITS.feed_name_max_length;
        let n = code_point_len(&self.name);
        if self.name != frozen_trim(&self.name) || !(1..=max).contains(&n) {
            return Err(format!(
                "Validation Error: a feed name is 1 to {max} characters, trimmed"
            ));
        }
        if let Some(icon) = &self.icon {
            let max = VALIDATION_LIMITS.feed_icon_max_length;
            let ok = (1..=max).contains(&icon.len())
                && icon
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err(format!(
                    "Validation Error: a feed icon is 1 to {max} lowercase letters, digits or -"
                ));
            }
        }
        if self.created_at <= 0 || self.created_at > pubky_social_specs::MAX_SAFE_JSON_INT {
            return Err("Validation Error: created_at is positive Unix microseconds".into());
        }
        let f = &self.feed;
        if let Some(tags) = &f.tags {
            if tags.is_empty() || tags.len() > VALIDATION_LIMITS.feed_tags_max_count {
                return Err(format!(
                    "Validation Error: a feed has 1 to {} tags, or none",
                    VALIDATION_LIMITS.feed_tags_max_count
                ));
            }
            if feed_tags(tags)?.as_ref() != Some(tags) {
                return Err("Validation Error: feed tags are canonical: folded, unique, sorted".into());
            }
        }
        if strict {
            let unknown = matches!(f.tags_match, MusicFeedMatch::Unknown(_))
                || matches!(f.reach, PubkySocialFeedReach::Unknown(_))
                || matches!(f.sort, MusicFeedSort::Unknown(_))
                || matches!(f.content, MusicFeedContent::Unknown(_))
                || matches!(f.layout, PubkySocialFeedLayout::Unknown(_));
            if unknown {
                return Err(
                    "Validation Error: a feed's match, reach, sort, content and layout are known values"
                        .into(),
                );
            }
        }
        if f.rules.len() > FEED_RULES_MAX {
            return Err(format!(
                "Validation Error: a feed has at most {FEED_RULES_MAX} rules"
            ));
        }
        for r in &f.rules {
            let known =
                FEED_RULE_FIELDS.contains(&r.field.as_str()) && FEED_RULE_OPS.contains(&r.op.as_str());
            if strict && !known {
                return Err(format!(
                    "Validation Error: rule {} {} is not one this version applies",
                    r.field, r.op
                ));
            }
            if r.op == "between" && r.to.is_none() {
                return Err("Validation Error: a between rule has a to".into());
            }
        }
        let uris_ok = |uris: &[String]| {
            uris.len() <= FEED_URIS_MAX
                && uris
                    .iter()
                    .all(|u| canonicalize_universal(u).as_deref() == Ok(u.as_str()))
        };
        if !uris_ok(&f.seeds) {
            return Err(format!(
                "Validation Error: at most {FEED_URIS_MAX} seeds, each a canonical URI"
            ));
        }
        if let Some(x) = &f.leave_out {
            if !uris_ok(&x.uris) {
                return Err(format!(
                    "Validation Error: at most {FEED_URIS_MAX} left-out URIs, each canonical"
                ));
            }
            if x.artists.len() > FEED_URIS_MAX || x.genres.len() > FEED_URIS_MAX {
                return Err(format!(
                    "Validation Error: at most {FEED_URIS_MAX} left-out artists or genres"
                ));
            }
        }
        if let Some(l) = &f.limit {
            if l.songs.is_some_and(|n| n == 0 || n > FEED_SONGS_MAX)
                || l.minutes.is_some_and(|n| n == 0 || n > FEED_MINUTES_MAX)
            {
                return Err(format!(
                    "Validation Error: a limit is 1 to {FEED_SONGS_MAX} songs or 1 to {FEED_MINUTES_MAX} minutes"
                ));
            }
        }
        Ok(())
    }
}
