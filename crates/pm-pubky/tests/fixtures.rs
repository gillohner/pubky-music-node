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

// ---- Pinakes music step (fixtures re-recorded 2026-10-20 from pubky/pinakes#21) ----

use pm_pubky::pinakes::GroupDetail;

#[test]
fn music_track_card_has_facts_and_one_link_per_service() {
    let r: ResourceDetail =
        serde_json::from_str(&fixture("pinakes/music/card-tidal-track-bohemian.json")).unwrap();
    assert_eq!(r.card.type_.as_deref(), Some("MusicRecording"));
    assert_eq!(r.card.site.as_deref(), Some("TIDAL"));
    assert_eq!(r.card.group.as_ref().map(|g| g.size), Some(5));
    let f = r.facts();
    assert_eq!(f.mbid.as_deref(), Some("b1a9c0e9-d987-4042-ae91-78d6a3267d69"));
    assert_eq!(f.isrcs().len(), 7);
    assert!(f.isrcs().contains(&"GBUM71029604".to_string()));
    assert_eq!(f.duration_secs(), Some(355));
    assert_eq!(f.artist.as_deref(), Some("Queen"));
    assert_eq!(f.original_album.as_deref(), Some("A Night at the Opera"));
    assert_eq!(r.card.artist_hint().as_deref(), Some("Queen"));
    // The card: one link per service, best first, never its own service.
    let sites: Vec<&str> = r.card.also_on.iter().filter_map(|a| a.site.as_deref()).collect();
    assert_eq!(sites, ["Spotify", "YouTube", "Deezer"]);
    assert!(
        !sites.contains(&"TIDAL"),
        "never another page of the card's own service"
    );
    // The details: every link of the group, other TIDAL pages included.
    assert_eq!(r.also_on().len(), 5);
    assert!(r.also_on().iter().any(|a| a.site.as_deref() == Some("TIDAL")));
    let derived: Vec<&pm_pubky::pinakes::AlsoOn> = r.also_on().iter().filter(|a| !a.is_tagged()).collect();
    assert_eq!(derived.len(), 1);
    assert_eq!(derived[0].via.as_deref(), Some("musicbrainz"));
    assert!(derived[0].id.is_none());
    assert_eq!(sites, ["Spotify", "YouTube", "Deezer"]);
    assert!(
        !sites.contains(&"TIDAL"),
        "never another page of the card's own service"
    );
}

#[test]
fn music_artist_card_is_a_group_with_an_mbid() {
    let r: ResourceDetail =
        serde_json::from_str(&fixture("pinakes/music/card-tidal-artist-radiohead.json")).unwrap();
    assert_eq!(r.card.type_.as_deref(), Some("MusicGroup"));
    assert_eq!(r.facts().artist_type.as_deref(), Some("Group"));
    assert!(r.mbid().is_some());
    assert!(r.card.artist_hint().is_none(), "an artist card has no artist");
    let g: GroupDetail = serde_json::from_str(&fixture("pinakes/music/group-radiohead.json")).unwrap();
    assert_eq!(g.type_.as_deref(), Some("MusicGroup"));
    assert_eq!(g.members.len(), 9);
    assert!(g.keys.iter().any(|k| k.starts_with("hub:mb:artist:")));
    assert!(g.keys.iter().any(|k| k.starts_with("hub:wikidata:")));
}

#[test]
fn music_key_search_and_charts_parse() {
    let r: SearchResponse = serde_json::from_str(&fixture("pinakes/music/search-key-isrc.json")).unwrap();
    assert!(r.items.len() >= 2);
    let groups: std::collections::BTreeSet<_> = r
        .items
        .iter()
        .filter_map(|c| c.group.as_ref().map(|g| g.id.clone()))
        .collect();
    assert_eq!(groups.len(), 1, "one ISRC, one group across services");
    let r: SearchResponse = serde_json::from_str(&fixture("pinakes/music/search-key-upc.json")).unwrap();
    assert!(r.items.iter().all(|c| c.type_.as_deref() == Some("MusicAlbum")));
    let r: SearchResponse =
        serde_json::from_str(&fixture("pinakes/music/search-key-mbid-artist.json")).unwrap();
    assert!(!r.items.is_empty());
    let r: SearchResponse =
        serde_json::from_str(&fixture("pinakes/music/search-trending-tracks-7d.json")).unwrap();
    assert_eq!(r.items.len(), 8);
    assert!(r
        .items
        .iter()
        .all(|c| c.type_.as_deref() == Some("MusicRecording")));
    // One row per group: no group id twice.
    let mut ids: Vec<_> = r
        .items
        .iter()
        .filter_map(|c| c.group.as_ref().map(|g| &g.id))
        .collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n);
    let items = items_from_cards(r.items);
    assert_eq!(
        items.len(),
        8,
        "every chart row is a music URL the node understands"
    );
    let r: SearchResponse = serde_json::from_str(&fixture("pinakes/music/search-albums.json")).unwrap();
    assert!(!r.items.is_empty());
    for name in [
        "card-bandcamp-radiohead",
        "card-deezer-album-cut-to-the-feeling",
        "card-spotify-track",
        "detail-tidal-track-bohemian",
    ] {
        let r: ResourceDetail =
            serde_json::from_str(&fixture(&format!("pinakes/music/{name}.json"))).unwrap();
        assert!(r.card.is_ok(), "{name}");
        assert!(r.card.is_music(), "{name}");
    }
}

#[test]
fn maybe_same_parses_on_event_cards() {
    let v = serde_json::json!({"id": "a", "uri": "https://ra.co/events/1", "status": "ok", "type": "MusicEvent",
        "maybe_same": [{"id": "b", "uri": "https://www.songkick.com/concerts/2", "site": "Songkick", "start": "2026-11-01T20:00:00Z", "score": 0.82}]});
    let c: Card = serde_json::from_value(v).unwrap();
    assert_eq!(c.maybe_same.len(), 1);
    assert_eq!(c.maybe_same[0].site.as_deref(), Some("Songkick"));
    assert_eq!(c.maybe_same[0].score, Some(0.82));
}

/// The node calls only routes Pinakes documents (openapi candidate from pubky/pinakes#21).
#[test]
fn every_pinakes_route_the_node_uses_is_documented() {
    let v: serde_json::Value =
        serde_json::from_str(&fixture("pinakes/music/openapi-candidate.json")).unwrap();
    let paths = v["paths"].as_object().expect("paths");
    for route in [
        "/v1/search",
        "/v1/resources/lookup",
        "/v1/resources/by-uri",
        "/v1/resources/{id}",
        "/v1/groups/{id}",
        "/v1/resolve",
        "/v1/changes/lookup",
        "/v1/info",
    ] {
        assert!(
            paths.contains_key(route),
            "{route} missing from the Pinakes OpenAPI"
        );
    }
    let search: Vec<String> = paths["/v1/search"]["get"]["parameters"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for q in ["key", "sort", "window", "collapse", "type"] {
        assert!(
            search.iter().any(|n| n == q),
            "search has no `{q}` parameter: {search:?}"
        );
    }
}
