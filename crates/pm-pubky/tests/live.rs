//! Read-only checks against the public dev Pinakes and staging Nexus.
//! Run with `PM_LIVE_TESTS=1 cargo test -p pm-pubky --test live`.

use pm_model::MusicUri;
use pm_pubky::nexus::NexusError;
use pm_pubky::{Discovery, NexusClient, PinakesClient, PmPubkyConfig, SearchQuery};

fn live() -> bool {
    std::env::var("PM_LIVE_TESTS").map(|v| v == "1").unwrap_or(false)
}

#[tokio::test]
async fn pinakes_and_nexus_live() {
    if !live() {
        eprintln!("skipped: set PM_LIVE_TESTS=1");
        return;
    }
    let cfg = PmPubkyConfig {
        user_agent_contact: "pm-pubky live test".into(),
        ..Default::default()
    };
    let pinakes = PinakesClient::new(&cfg).unwrap();
    let nexus = NexusClient::new(&cfg).unwrap();

    let info = pinakes.info().await.unwrap();
    assert!(info.counts.resources > 0);

    let search = pinakes
        .search(&SearchQuery {
            limit: Some(5),
            facet: Some("tags".into()),
            ..SearchQuery::tags("music")
        })
        .await
        .unwrap();
    assert!(!search.items.is_empty());
    assert!(search.facets.is_some());

    let cards = pinakes
        .lookup(&["https://open.spotify.com/track/2Foc5Q5nqNiosCNqttzHof".to_string()])
        .await
        .unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].type_.as_deref(), Some("MusicRecording"));
    let detail = pinakes.resource(&cards[0].id).await.unwrap();
    assert_eq!(detail.card.id, cards[0].id);
    let changes = pinakes
        .changes_lookup(&[cards[0].id.clone()], "now-30d", None, None)
        .await
        .unwrap();
    assert!(changes.next_cursor >= 0);

    let view = nexus
        .resource_by_uri("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        .await
        .unwrap();
    assert!(view.taggers_for("music").is_some());
    assert!(matches!(
        nexus.resource_by_uri("https://example.invalid/none").await,
        Err(NexusError::NotFound)
    ));
    assert!(matches!(
        nexus
            .resources_by_uris(std::slice::from_ref(&view.resource.uri), None)
            .await,
        Err(NexusError::RouteUnavailable)
    ));

    let discovery = Discovery::new(pinakes, nexus);
    let (facets, items) = discovery.trends(10).await.unwrap();
    assert!(!facets.is_empty() && !items.is_empty());
    let page = discovery.by_tag("track", 5, None).await.unwrap();
    assert!(page.items.len() <= 5);
    let who = discovery
        .who_else_likes(&MusicUri::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap())
        .await
        .unwrap();
    assert!(who.is_some_and(|u| !u.is_empty()));
}
