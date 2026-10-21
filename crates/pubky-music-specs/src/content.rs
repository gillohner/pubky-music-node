//! The JSON envelopes inside a review's and a listen's `content`.
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
/// A service name (`tidal`, `deezer`, `library`), in bytes.
pub const SERVICE_MAX_LENGTH: usize = 32;
/// The earliest `played_at`, 2000-01-01 in Unix milliseconds. It catches a seconds value.
pub const LISTEN_PLAYED_AT_MIN_MS: i64 = 946_684_800_000;
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

/// The content of a `listen`: `{"played_at": ms, "duration_ms"?: ms, "source"?: {service, uri}}`.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct MusicListenContent {
    /// When it started, Unix milliseconds.
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

    /// A source known only by its service (`library`, `roon`).
    pub fn service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            uri: None,
            extra: Map::new(),
        }
    }
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

pub(crate) fn parse_listen(content: &str) -> Result<MusicListenContent, String> {
    let listen: MusicListenContent = parse(content, "listen")?;
    check_extra(&listen.extra, &["played_at", "duration_ms", "source"])?;
    validate_safe_json_int(listen.played_at)?;
    if listen.played_at < LISTEN_PLAYED_AT_MIN_MS {
        return Err("Validation Error: played_at must be Unix milliseconds from 2000 on".into());
    }
    if let Some(d) = listen.duration_ms {
        if !(0..=LISTEN_DURATION_MAX_MS).contains(&d) {
            return Err(format!(
                "Validation Error: duration_ms must be 0 to {LISTEN_DURATION_MAX_MS}"
            ));
        }
    }
    if let Some(source) = &listen.source {
        check_source(source)?;
        let uri = source.uri.as_deref().unwrap_or("");
        if code_point_len(uri) > VALIDATION_LIMITS.reference_uri_max_length {
            return Err("Validation Error: source.uri is too long".into());
        }
    }
    Ok(listen)
}

/// What the reference gate should check, read leniently: an unparsable content is the kind's
/// validation error, reported there.
pub(crate) fn listen_source_uri(content: &str) -> Option<String> {
    let value: Value = serde_json::from_str(content).ok()?;
    Some(value.get("source")?.get("uri")?.as_str()?.to_string())
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
