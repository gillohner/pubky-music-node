//! The `ext.music` hints on a playlist, which is a social `collection` post.
//!
//! - On an item: `ext.music.source = {service, uri}`, where the curator played it from, and
//!   `ext.music.added_at` (Unix milliseconds). The item's
//!   own `uri` stays the canonical music URL.
//! - On the collection: `ext.music.source_order = ["tidal", "library", …]`, the services to try
//!   first when playing it.
//!
//! Everything under `ext` is third-party data the social spec never validates, so it is hostile
//! input (RFC Part C2, rules 10 and 11). The readers here return only what passes the music rules
//! and drop the rest silently; a bad hint never makes a playlist unreadable. The writers keep every
//! other member of `ext` and of `ext.music`.

use pubky_social_specs::{
    validate_reference, AllowedSchemes, PubkyId, PubkySocialCollectionContent, PubkySocialCollectionItem,
    ValidationCtx, VALIDATION_LIMITS,
};
use serde_json::{Map, Value};

use crate::content::{check_source, is_service_name, MusicSource};

/// The reserved extension member.
pub const EXT: &str = "ext";
/// Music's member inside `ext`.
pub const MUSIC: &str = "music";
/// At most this many services in a `source_order`; the rest are ignored.
pub const SOURCE_ORDER_MAX: usize = 16;

fn music(extra: &Map<String, Value>) -> Option<&Map<String, Value>> {
    extra.get(EXT)?.as_object()?.get(MUSIC)?.as_object()
}

fn music_mut(extra: &mut Map<String, Value>) -> &mut Map<String, Value> {
    let ext = extra.entry(EXT).or_insert_with(|| Value::Object(Map::new()));
    if !ext.is_object() {
        // Not an object is not an extension anyone can read; music takes the slot
        *ext = Value::Object(Map::new());
    }
    let ext = ext.as_object_mut().expect("just made an object");
    let music = ext.entry(MUSIC).or_insert_with(|| Value::Object(Map::new()));
    if !music.is_object() {
        *music = Value::Object(Map::new());
    }
    music.as_object_mut().expect("just made an object")
}

/// Drops `ext.music` when it is empty, then `ext` when that is empty too.
fn tidy(extra: &mut Map<String, Value>) {
    if let Some(Value::Object(ext)) = extra.get_mut(EXT) {
        if matches!(ext.get(MUSIC), Some(Value::Object(m)) if m.is_empty()) {
            ext.remove(MUSIC);
        }
        if ext.is_empty() {
            extra.remove(EXT);
        }
    }
}

/// The item's `ext.music.source`, when it is well formed: a service name and, when present, an
/// http(s) or pubky URI that passes the reference gate (returned canonical). A source without a
/// URI names a service only (`library`: "play my own file"). `owner` is the playlist's author,
/// for private references.
pub fn item_source(
    item: &PubkySocialCollectionItem,
    ctx: &ValidationCtx,
    owner: Option<&PubkyId>,
) -> Option<MusicSource> {
    let value = music(&item.extra)?.get("source")?.clone();
    let mut source: MusicSource = serde_json::from_value(value).ok()?;
    check_source(&source).ok()?;
    if let Some(uri) = source.uri.as_deref() {
        source.uri = Some(
            validate_reference(
                uri,
                AllowedSchemes::PubkyHttpHttps,
                VALIDATION_LIMITS.reference_uri_max_length,
                ctx,
                owner,
            )
            .ok()?,
        );
    }
    Some(source)
}

/// Sets or clears the item's `ext.music.source`.
pub fn set_item_source(item: &mut PubkySocialCollectionItem, source: Option<&MusicSource>) {
    match source {
        Some(source) => {
            let value = serde_json::to_value(source).expect("a source always serializes");
            music_mut(&mut item.extra).insert("source".into(), value);
        }
        None => {
            if let Some(Value::Object(ext)) = item.extra.get_mut(EXT) {
                if let Some(Value::Object(m)) = ext.get_mut(MUSIC) {
                    m.remove("source");
                }
            }
        }
    }
    tidy(&mut item.extra);
}

/// The collection's `ext.music.source_order`: well-formed service names, first occurrence only,
/// at most [`SOURCE_ORDER_MAX`]. Anything else in the list is skipped.
pub fn source_order(collection: &PubkySocialCollectionContent) -> Vec<String> {
    let Some(Value::Array(list)) = music(&collection.extra).and_then(|m| m.get("source_order")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for name in list.iter().filter_map(Value::as_str) {
        if is_service_name(name) && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
            if out.len() == SOURCE_ORDER_MAX {
                break;
            }
        }
    }
    out
}

/// Sets the collection's `ext.music.source_order`; an empty list removes it.
pub fn set_source_order(collection: &mut PubkySocialCollectionContent, order: &[String]) {
    if order.is_empty() {
        if let Some(Value::Object(ext)) = collection.extra.get_mut(EXT) {
            if let Some(Value::Object(m)) = ext.get_mut(MUSIC) {
                m.remove("source_order");
            }
        }
    } else {
        let list = order.iter().cloned().map(Value::String).collect();
        music_mut(&mut collection.extra).insert("source_order".into(), Value::Array(list));
    }
    tidy(&mut collection.extra);
}

/// The item's `ext.music.added_at`, Unix milliseconds, when it is a JSON-safe integer from 2000 on.
pub fn item_added_at(item: &PubkySocialCollectionItem) -> Option<i64> {
    let ms = music(&item.extra)?.get("added_at")?.as_i64()?;
    (pubky_social_specs::validate_safe_json_int(ms).is_ok() && ms >= crate::LISTEN_PLAYED_AT_MIN_MS)
        .then_some(ms)
}

/// Sets the item's `ext.music.added_at` (Unix milliseconds).
pub fn set_item_added_at(item: &mut PubkySocialCollectionItem, ms: i64) {
    music_mut(&mut item.extra).insert("added_at".into(), Value::from(ms));
}
