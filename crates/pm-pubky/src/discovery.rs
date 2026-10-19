//! The §8.4 queries: friends' activity, tag-based discovery, trends, "who else
//! likes this recording". Pinakes ranks, Nexus hydrates (its design rule), so
//! each query is one Pinakes search and, where the caller wants it, one
//! Nexus read per card for taggers.
//!
//! Every result carries the card's URI parsed as a canonical
//! [`MusicUri`]; cards whose URI is not a music URI (a YouTube video page, a
//! label's site) are dropped, because the node cannot resolve them to a
//! playable item. Until Pinakes step 8 types every music card, the `music`
//! tag is the filter.

use crate::nexus::{NexusClient, NexusError};
use crate::pinakes::{Card, PinakesClient, PinakesError, SearchQuery, TagFacet};
use pm_model::MusicUri;
use serde::{Deserialize, Serialize};

/// The label the staging seed data and the clients agree on.
pub const MUSIC_TAG: &str = "music";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredItem {
    pub uri: MusicUri,
    pub card: Card,
    /// Distinct taggers from Nexus, when hydrated.
    pub taggers: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error(transparent)]
    Pinakes(#[from] PinakesError),
    #[error(transparent)]
    Nexus(#[from] NexusError),
}

/// A page of discovered items with the cursor for the next one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryPage {
    pub items: Vec<DiscoveredItem>,
    pub cursor: Option<String>,
    pub reach_truncated: bool,
}

/// Turn cards into items, dropping the ones that are not music URIs.
pub fn items_from_cards(cards: Vec<Card>) -> Vec<DiscoveredItem> {
    cards
        .into_iter()
        .filter_map(|card| match MusicUri::parse(&card.uri) {
            Ok(uri) => Some(DiscoveredItem {
                uri,
                card,
                taggers: None,
            }),
            Err(e) => {
                tracing::debug!(uri = %card.uri, error = %e, "dropping non-music card");
                None
            }
        })
        .collect()
}

#[derive(Clone)]
pub struct Discovery {
    pinakes: PinakesClient,
    nexus: NexusClient,
}

impl Discovery {
    pub fn new(pinakes: PinakesClient, nexus: NexusClient) -> Self {
        Self { pinakes, nexus }
    }

    pub fn pinakes(&self) -> &PinakesClient {
        &self.pinakes
    }

    pub fn nexus(&self) -> &NexusClient {
        &self.nexus
    }

    async fn page(&self, query: SearchQuery) -> Result<DiscoveryPage, DiscoveryError> {
        let resp = self.pinakes.search(&query).await?;
        Ok(DiscoveryPage {
            items: items_from_cards(resp.items),
            cursor: resp.cursor,
            reach_truncated: resp.reach_truncated,
        })
    }

    /// Music links tagged by people `viewer_z32` follows, ranked by Pinakes.
    /// `reach_truncated` says the follow set was capped.
    pub async fn friends_activity(
        &self,
        viewer_z32: &str,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<DiscoveryPage, DiscoveryError> {
        self.page(SearchQuery {
            reach: Some("following".into()),
            viewer: Some(viewer_z32.to_string()),
            limit: Some(limit),
            cursor,
            ..SearchQuery::tags(MUSIC_TAG)
        })
        .await
    }

    /// Music links carrying `label` (plus `music`).
    pub async fn by_tag(
        &self,
        label: &str,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<DiscoveryPage, DiscoveryError> {
        let tags = if label == MUSIC_TAG {
            MUSIC_TAG.to_string()
        } else {
            format!("{MUSIC_TAG},{label}")
        };
        self.page(SearchQuery {
            tags: Some(tags),
            match_: Some("all".into()),
            limit: Some(limit),
            cursor,
            ..Default::default()
        })
        .await
    }

    /// Trending labels on music links, with the top items by recent activity.
    ///
    /// Pinakes's `from`/`to` bound an event's `start`, not when a link was
    /// tagged, so they exclude every music card (verified live: `from=now-365d`
    /// returns nothing). Until Pinakes offers a reference-time window, "trend"
    /// is the tag facet over all music links ordered by `sort=activity`, which
    /// is Pinakes's own 30-day-half-life ranking.
    pub async fn trends(&self, limit: usize) -> Result<(Vec<TagFacet>, Vec<DiscoveredItem>), DiscoveryError> {
        let resp = self
            .pinakes
            .search(&SearchQuery {
                sort: Some("activity".into()),
                facet: Some("tags".into()),
                limit: Some(limit),
                ..SearchQuery::tags(MUSIC_TAG)
            })
            .await?;
        let mut facets = resp.facets.map(|f| f.tags).unwrap_or_default();
        facets.retain(|f| f.label != MUSIC_TAG);
        Ok((facets, items_from_cards(resp.items)))
    }

    /// Who tagged this recording: distinct taggers from Nexus, `None` if the
    /// link is not indexed there.
    pub async fn who_else_likes(&self, uri: &MusicUri) -> Result<Option<Vec<String>>, DiscoveryError> {
        match self.nexus.resource_by_uri(&uri.to_url()).await {
            Ok(view) => {
                let mut users: Vec<String> = view.tags.into_iter().flat_map(|t| t.taggers).collect();
                users.sort_unstable();
                users.dedup();
                Ok(Some(users))
            }
            Err(NexusError::NotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Fill `taggers` on each item from Nexus; items Nexus does not know keep
    /// `None`. One request per item: keep pages small.
    pub async fn hydrate_taggers(&self, items: &mut [DiscoveredItem]) -> Result<(), DiscoveryError> {
        for item in items.iter_mut() {
            match self.nexus.resource_by_uri(&item.card.uri).await {
                Ok(view) => item.taggers = Some(view.distinct_taggers() as u32),
                Err(NexusError::NotFound) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_model::MusicEntity;

    fn card(uri: &str, type_: &str) -> Card {
        serde_json::from_value(serde_json::json!({ "id": "x", "uri": uri, "status": "ok", "type": type_ }))
            .unwrap()
    }

    #[test]
    fn drops_non_music_uris_and_keeps_order() {
        let items = items_from_cards(vec![
            card(
                "https://open.spotify.com/track/2Foc5Q5nqNiosCNqttzHof",
                "MusicRecording",
            ),
            card("https://luma.com/376whqyw", "Event"),
            card(
                "https://musicbrainz.org/artist/056e4f3e-d505-4dad-8ec1-d04f521cbb56",
                "MusicGroup",
            ),
            card("https://www.youtube.com/watch?v=dQw4w9WgXcQ", "VideoObject"),
        ]);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].uri.entity(), Some(MusicEntity::Recording));
        assert_eq!(items[1].uri.entity(), Some(MusicEntity::Artist));
        assert_eq!(
            items[2].uri.entity(),
            None,
            "a youtube watch page is kept but untyped"
        );
        assert!(items.iter().all(|i| i.taggers.is_none()));
    }
}
