//! The JSON envelopes inside the `content` of each music kind.
//!
//! **Times and durations.** A field ending in `_at` is a Unix time in **microseconds**, like the
//! social specs' `created_at`. A field ending in `_ms` is a duration in milliseconds (media
//! playback). Older writers put `played_at` and `ext.music.added_at` in milliseconds; readers take
//! a time below [`MILLIS_BEFORE`] (3000-01-01 in milliseconds) as milliseconds ([`micros_of`]).
//! Every microsecond time from 2000 on is above it, so the rule is unambiguous.
//!
//! Like the social envelopes they never deny unknown fields: unknown members land in `extra` and
//! are carried through a rewrite. `extra` may not shadow a known field, and its integers must be
//! JSON-safe, the same two rules the social specs put on every `extra`.

use pubky_social_specs::{code_point_len, frozen_trim, validate_safe_json_int, VALIDATION_LIMITS};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The lowest rating.
pub const REVIEW_RATING_MIN: u8 = 1;
/// The highest rating: ten half stars.
pub const REVIEW_RATING_MAX: u8 = 10;
/// A review's text, in code points: a social note's length.
pub const REVIEW_TEXT_MAX_LENGTH: usize = 2000;
/// A service name (`tidal`, `deezer`, `library` for the curator's own files), in bytes.
pub const SERVICE_MAX_LENGTH: usize = 32;
/// The earliest time a music post carries: 2000-01-01 in Unix microseconds.
pub const TIME_MIN_MICROS: i64 = 946_684_800_000_000;
/// 3000-01-01 in Unix milliseconds: a time below it was written in milliseconds ([`micros_of`]).
pub const MILLIS_BEFORE: i64 = 32_503_680_000_000;
/// 2000-01-01 in Unix milliseconds: the earliest millisecond time read. It catches seconds.
const TIME_MIN_MILLIS: i64 = 946_684_800_000;
/// The longest a `listening` or a `session` announces itself for: 24 hours.
pub const LIVE_WINDOW_MAX_MICROS: i64 = 24 * 60 * 60 * 1_000_000;
/// The services everyone spells the same way (an open vocabulary: other well-formed names are
/// fine, and a reader that does not know one treats the source as unknown). Hyphenated, never
/// underscored: Pinakes maps its `apple_music` to `apple-music`.
pub const SERVICE_NAMES: &[&str] = &[
    "library",
    "roon",
    "tidal",
    "spotify",
    "apple-music",
    "amazon-music",
    "youtube",
    "youtube-music",
    "deezer",
    "qobuz",
    "soundcloud",
    "bandcamp",
    "beatport",
    "audiomack",
    "audius",
    "jamendo",
    "internet-archive",
];
/// The longest listen: 24 hours.
pub const LISTEN_DURATION_MAX_MS: i64 = 24 * 60 * 60 * 1000;
/// The whole `content` string, in code points.
const CONTENT_MAX_LENGTH: usize = 8 * 1024;

/// The content of a `review`: `{"rating": 1–10, "text"?: "…"}`.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct MusicReviewContent {
    /// 1 to 10. A five-star UI shows half stars (7 is three and a half).
    pub rating: u8,
    /// What the reviewer wrote, at most [`REVIEW_TEXT_MAX_LENGTH`] code points, never blank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Unknown members, preserved on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The content of a `listen`: `{"played_at": µs, "duration_ms"?: ms, "source"?: {service, uri}}`.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct MusicListenContent {
    /// When it started, Unix microseconds (an older millisecond value reads as microseconds).
    pub played_at: i64,
    /// How long it played, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// Where it played from, when that differs from the canonical URL in `embed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MusicSource>,
    /// Unknown members, preserved on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A service and the item's own link there: `{"service": "tidal", "uri": "https://tidal.com/…"}`.
/// Used by a listen (where the link may be unknown) and by the `ext.music.source` hint on a
/// playlist item (where it is required).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct MusicSource {
    /// Lowercase name: ASCII letters, digits and `-`, at most [`SERVICE_MAX_LENGTH`] bytes.
    pub service: String,
    /// The item on that service: an http(s) or pubky URI, so it is safe to open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Unknown members, preserved on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl MusicSource {
    /// A source with a link and no unknown members.
    pub fn new(service: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            uri: Some(uri.into()),
            extra: Map::new(),
        }
    }

    /// A source known only by its service (`library`: the curator's own files).
    pub fn service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            uri: None,
            extra: Map::new(),
        }
    }
}

/// The content of a `listening` (now playing):
/// `{"started_at": µs, "expires_at": µs, "duration_ms"?: ms, "source"?: {service, uri?}}`.
/// `embed` is the music playing now.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct MusicListeningContent {
    /// When it started, Unix microseconds.
    pub started_at: i64,
    /// When the announcement lapses (at most [`LIVE_WINDOW_MAX_MICROS`] after `started_at`).
    pub expires_at: i64,
    /// The music's length, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// Where it plays from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MusicSource>,
    /// Unknown members, preserved on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The content of a `session` (a listening session others can join):
/// `{"started_at": µs, "expires_at"?: µs, "host": "pubky://<key>", "source"?: {service, uri?}}`.
/// `embed` is the music playing now.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct MusicSessionContent {
    /// When it started, Unix microseconds.
    pub started_at: i64,
    /// When it ends if nobody renews it (at most [`LIVE_WINDOW_MAX_MICROS`] after `started_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Who hosts it: a Pubky user, `pubky://<key>`.
    pub host: String,
    /// Where it plays from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MusicSource>,
    /// Unknown members, preserved on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A time field by the rule in the module docs: microseconds as they are, an older millisecond
/// value (below [`MILLIS_BEFORE`]) times 1000. `None` before 2000 or outside the JSON-safe range.
pub fn micros_of(t: i64) -> Option<i64> {
    validate_safe_json_int(t).ok()?;
    if t >= TIME_MIN_MICROS {
        Some(t)
    } else if (TIME_MIN_MILLIS..MILLIS_BEFORE).contains(&t) {
        Some(t * 1000)
    } else {
        None
    }
}

/// A service name as the vocabulary spells it: trimmed, lowercase, `_` and spaces as `-`
/// (`Apple_Music` is `apple-music`). `None` when it is not a well-formed name after that.
pub fn service_name(raw: &str) -> Option<String> {
    let name: String = raw
        .trim()
        .chars()
        .map(|c| match c {
            '_' | ' ' => '-',
            c => c.to_ascii_lowercase(),
        })
        .collect();
    is_service_name(&name).then_some(name)
}

/// `true` for a well-formed service name: 1 to [`SERVICE_MAX_LENGTH`] lowercase ASCII letters,
/// digits or `-`.
pub fn is_service_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= SERVICE_MAX_LENGTH
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The source's own shape; its `uri` is checked by the reference gate, not here.
pub(crate) fn check_source(source: &MusicSource) -> Result<(), String> {
    check_extra(&source.extra, &["service", "uri"])?;
    if !is_service_name(&source.service) {
        return Err(format!(
            "Validation Error: source.service must be 1 to {SERVICE_MAX_LENGTH} lowercase letters, digits or -"
        ));
    }
    Ok(())
}

fn parse<T: serde::de::DeserializeOwned>(content: &str, what: &str) -> Result<T, String> {
    if code_point_len(content) > CONTENT_MAX_LENGTH {
        return Err(format!(
            "Validation Error: {what} content must be at most {CONTENT_MAX_LENGTH} code points"
        ));
    }
    serde_json::from_str(content)
        .map_err(|e| format!("Validation Error: {what} content must be a valid JSON envelope: {e}"))
}

pub(crate) fn parse_review(content: &str) -> Result<MusicReviewContent, String> {
    let review: MusicReviewContent = parse(content, "review")?;
    check_extra(&review.extra, &["rating", "text"])?;
    if !(REVIEW_RATING_MIN..=REVIEW_RATING_MAX).contains(&review.rating) {
        return Err(format!(
            "Validation Error: rating must be {REVIEW_RATING_MIN} to {REVIEW_RATING_MAX}"
        ));
    }
    if let Some(text) = &review.text {
        if frozen_trim(text).is_empty() {
            return Err("Validation Error: review text must not be blank; leave it out".into());
        }
        if code_point_len(text) > REVIEW_TEXT_MAX_LENGTH {
            return Err(format!(
                "Validation Error: review text must be at most {REVIEW_TEXT_MAX_LENGTH} code points"
            ));
        }
    }
    Ok(review)
}

fn time(field: &str, t: i64) -> Result<i64, String> {
    micros_of(t).ok_or_else(|| format!("Validation Error: {field} must be Unix microseconds from 2000 on"))
}

fn duration(d: Option<i64>) -> Result<(), String> {
    match d {
        Some(d) if !(0..=LISTEN_DURATION_MAX_MS).contains(&d) => Err(format!(
            "Validation Error: duration_ms must be 0 to {LISTEN_DURATION_MAX_MS}"
        )),
        _ => Ok(()),
    }
}

fn source(source: Option<&MusicSource>) -> Result<(), String> {
    if let Some(source) = source {
        check_source(source)?;
        let uri = source.uri.as_deref().unwrap_or("");
        if code_point_len(uri) > VALIDATION_LIMITS.reference_uri_max_length {
            return Err("Validation Error: source.uri is too long".into());
        }
    }
    Ok(())
}

/// A listen, with `played_at` in microseconds (an older millisecond value converted).
pub(crate) fn parse_listen(content: &str) -> Result<MusicListenContent, String> {
    let mut listen: MusicListenContent = parse(content, "listen")?;
    check_extra(&listen.extra, &["played_at", "duration_ms", "source"])?;
    listen.played_at = time("played_at", listen.played_at)?;
    duration(listen.duration_ms)?;
    source(listen.source.as_ref())?;
    Ok(listen)
}

fn window(started_at: i64, expires_at: i64) -> Result<(), String> {
    if expires_at <= started_at || expires_at - started_at > LIVE_WINDOW_MAX_MICROS {
        return Err("Validation Error: expires_at must be after started_at, within 24 hours".into());
    }
    Ok(())
}

pub(crate) fn parse_listening(content: &str) -> Result<MusicListeningContent, String> {
    let l: MusicListeningContent = parse(content, "listening")?;
    check_extra(&l.extra, &["started_at", "expires_at", "duration_ms", "source"])?;
    window(
        time("started_at", l.started_at)?,
        time("expires_at", l.expires_at)?,
    )?;
    duration(l.duration_ms)?;
    source(l.source.as_ref())?;
    Ok(l)
}

pub(crate) fn parse_session(content: &str) -> Result<MusicSessionContent, String> {
    let s: MusicSessionContent = parse(content, "session")?;
    check_extra(&s.extra, &["started_at", "expires_at", "host", "source"])?;
    let started = time("started_at", s.started_at)?;
    if let Some(e) = s.expires_at {
        window(started, time("expires_at", e)?)?;
    }
    // A user reference: the host's key and nothing after it (the gate checks the key)
    if !s.host.starts_with("pubky://") || s.host["pubky://".len()..].contains('/') {
        return Err("Validation Error: host must be a Pubky user, pubky://<key>".into());
    }
    source(s.source.as_ref())?;
    Ok(s)
}

/// What the reference gate should check, read leniently: an unparsable content is the kind's
/// validation error, reported there.
pub(crate) fn listen_source_uri(content: &str) -> Option<String> {
    let value: Value = serde_json::from_str(content).ok()?;
    Some(value.get("source")?.get("uri")?.as_str()?.to_string())
}

/// A session's `host`, read the same way.
pub(crate) fn session_host(content: &str) -> Option<String> {
    let value: Value = serde_json::from_str(content).ok()?;
    Some(value.get("host")?.as_str()?.to_string())
}

/// `extra` must not shadow a known field, and every integer in it must be JSON-safe.
pub(crate) fn check_extra(extra: &Map<String, Value>, known: &[&str]) -> Result<(), String> {
    if let Some(k) = extra.keys().find(|k| known.contains(&k.as_str())) {
        return Err(format!("Validation Error: extra must not shadow the field {k}"));
    }
    extra.values().try_for_each(check_safe_numbers)
}

fn check_safe_numbers(v: &Value) -> Result<(), String> {
    match v {
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => validate_safe_json_int(i),
            (None, Some(_)) => Err(format!(
                "Validation Error: integer {n} outside the JSON-safe range"
            )),
            (None, None) => Ok(()),
        },
        Value::Array(items) => items.iter().try_for_each(check_safe_numbers),
        Value::Object(map) => map.values().try_for_each(check_safe_numbers),
        _ => Ok(()),
    }
}
