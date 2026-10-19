//! Deserialization against responses captured from the live Pinakes dev
//! instance and staging Nexus on 2026-10-19 (tests/fixtures/*.json).

use pm_model::MusicEntity;
use pm_pubky::discovery::items_from_cards;
use pm_pubky::nexus::{HotTag, ResourceView, Taggers};
use pm_pubky::pinakes::{Card, ChangePage, Info, ResourceDetail, SearchResponse};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect(name)
}

#[test]
fn pinakes_info() {
    let info: Info = serde_json::from_str(&fixture("pinakes/info.json")).unwrap();
    assert!(info.counts.resources > 0);
    assert!(!info.ingestion.is_empty());
}

#[test]
fn pinakes_search_with_facets() {
    let r: SearchResponse = serde_json::from_str(&fixture("pinakes/search_music.json")).unwrap();
    assert_eq!(r.items.len(), 5);
    assert!(r.cursor.is_some());
    let facets = r.facets.expect("facet=tags requested");
    assert!(facets.tags.iter().any(|t| t.label == "music" && t.count > 0));
    assert!(!r.reach_truncated);
    // Every card has a non-empty id and uri, and the type is a schema.org name.
    for c in &r.items {
        assert_eq!(c.id.len(), 32);
        assert!(c.uri.starts_with("https://"));
        assert!(c.type_.is_some());
    }
}

#[test]
fn pinakes_search_reach_following() {
    let r: SearchResponse = serde_json::from_str(&fixture("pinakes/search_reach.json")).unwrap();
    assert!(!r.items.is_empty());
}

#[test]
fn pinakes_lookup_returns_music_recordings() {
    let cards: Vec<Card> = serde_json::from_str(&fixture("pinakes/lookup.json")).unwrap();
    // Three URIs sent (Spotify, Deezer, an unknown MusicBrainz recording); two known.
    assert_eq!(cards.len(), 2);
    assert!(cards.iter().all(|c| c.is_ok() && c.is_music()));
    assert_eq!(cards[0].type_.as_deref(), Some("MusicRecording"));
    assert!(cards[0].isrc.is_none(), "step 8 music facts not emitted yet");
    assert!(cards[0].also_on.is_empty());
    let items = items_from_cards(cards);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].uri.entity(), Some(MusicEntity::Recording));
    assert_eq!(
        items[0].uri.to_url(),
        "https://open.spotify.com/track/2Foc5Q5nqNiosCNqttzHof"
    );
}

#[test]
fn pinakes_resource_detail_and_resolve() {
    for f in [
        "pinakes/resource.json",
        "pinakes/resource_music.json",
        "pinakes/resolve.json",
    ] {
        let d: ResourceDetail = serde_json::from_str(&fixture(f)).unwrap();
        assert_eq!(d.card.id.len(), 32);
        assert!(d.details.updated_at.is_some(), "{f}");
        assert!(d.details.aliases.is_empty());
        // Round-trip keeps unknown fields.
        let back = serde_json::to_value(&d).unwrap();
        assert_eq!(
            back["details"]["updated_at"],
            serde_json::json!(d.details.updated_at)
        );
    }
}

#[test]
fn pinakes_changes_page() {
    let p: ChangePage = serde_json::from_str(&fixture("pinakes/changes_lookup.json")).unwrap();
    assert_eq!(p.lines.len(), 1);
    assert_eq!(p.lines[0].class, "created");
    assert!(p.lines[0].fields.contains(&"name".to_string()));
    assert!(!p.has_more);
}

#[test]
fn card_preserves_unknown_fields() {
    let c: Card = serde_json::from_str(r#"{"id":"a","uri":"https://x/","status":"ok","future_field":{"k":1},"also_on":[{"uri":"https://tidal.com/track/1","site":"TIDAL","derived":true,"source":"musicbrainz"}],"isrc":"USRC17607839"}"#).unwrap();
    assert_eq!(c.extra["future_field"]["k"], 1);
    assert_eq!(c.also_on[0].source.as_deref(), Some("musicbrainz"));
    assert_eq!(c.isrc.as_deref(), Some("USRC17607839"));
    assert!(c.type_.is_none());
}

#[test]
fn nexus_resource_by_uri_and_tags() {
    let v: ResourceView = serde_json::from_str(&fixture("nexus/resource_by_uri.json")).unwrap();
    assert_eq!(v.resource.id, "0f3820de1c3532e8805674cc996978f9");
    assert_eq!(v.taggers_for("music").unwrap().taggers_count, 8);
    assert!(v.distinct_taggers() >= 5);
    let t: ResourceView = serde_json::from_str(&fixture("nexus/resource_tags.json")).unwrap();
    assert_eq!(t.resource.uri, v.resource.uri);
    let taggers: Taggers = serde_json::from_str(&fixture("nexus/resource_taggers.json")).unwrap();
    assert_eq!(taggers.users.len(), 3);
    let hot: Vec<HotTag> = serde_json::from_str(&fixture("nexus/tags_hot.json")).unwrap();
    assert!(hot.iter().any(|h| h.label == "music"));
}
