//! Getting a Pinakes card for any music link (steward, owner bug "Pinakes
//! has not seen this link").
//!
//! `GET /v1/resources/by-uri` only knows links someone tagged. For any other
//! link the node asks Pinakes to read it now (`POST /v1/resolve`): the card
//! comes back at once (`ok`) or as `pending` while Pinakes reads the page.
//! Resolves are paced under Pinakes' 30 a minute per IP, cached, and the same
//! link is never resolved twice at the same time. Callers show the
//! provider's own data meanwhile and ask again later.

use crate::state::NodeState;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use pm_pubky::pinakes::{PinakesError, ResourceDetail};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Pinakes allows 30 resolves a minute per IP; stay under it.
pub const RESOLVE_INTERVAL: Duration = Duration::from_millis(2100);
/// A card is kept this long before it is asked for again.
const FOUND_TTL: Duration = Duration::from_secs(3600);
/// A pending or unknown link is not resolved again sooner than this.
pub const RETRY_AFTER: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq)]
pub enum Card {
    /// Pinakes has the card.
    Found(Box<ResourceDetail>),
    /// Pinakes is reading the link; the pending card when it sent one.
    Reading(Option<Box<ResourceDetail>>),
    /// Pinakes cannot read this link (or is unreachable).
    Unknown,
}

type Inflight = Shared<BoxFuture<'static, Card>>;

pub struct PinakesReader {
    /// How long a pending or unknown answer stands (tests shorten it).
    pub retry_after: Mutex<Duration>,
    cache: Mutex<HashMap<String, (Instant, Card)>>,
    inflight: Mutex<HashMap<String, Inflight>>,
    last_resolve: tokio::sync::Mutex<Option<Instant>>,
}

impl Default for PinakesReader {
    fn default() -> Self {
        Self {
            retry_after: Mutex::new(RETRY_AFTER),
            cache: Default::default(),
            inflight: Default::default(),
            last_resolve: Default::default(),
        }
    }
}

impl PinakesReader {
    fn cached(&self, uri: &str) -> Option<Card> {
        let c = self.cache.lock().expect("reader cache");
        let (at, card) = c.get(uri)?;
        let ttl = if matches!(card, Card::Found(_)) {
            FOUND_TTL
        } else {
            *self.retry_after.lock().expect("reader retry")
        };
        (at.elapsed() < ttl).then(|| card.clone())
    }

    fn remember(&self, uri: &str, card: &Card) {
        self.cache
            .lock()
            .expect("reader cache")
            .insert(uri.to_string(), (Instant::now(), card.clone()));
    }

    /// Wait for the next resolve slot.
    async fn pace(&self) {
        let mut last = self.last_resolve.lock().await;
        if let Some(t) = *last {
            let e = t.elapsed();
            if e < RESOLVE_INTERVAL {
                tokio::time::sleep(RESOLVE_INTERVAL - e).await;
            }
        }
        *last = Some(Instant::now());
    }
}

/// The card for `uri`: cached, else by-uri, else a (paced, deduplicated)
/// resolve. Never an error: an unreachable Pinakes is `Unknown`.
pub async fn card(s: &std::sync::Arc<NodeState>, uri: &str) -> Card {
    let uri = uri.trim().to_string();
    if crate::activity::is_search_link(&uri) {
        return Card::Unknown;
    }
    if let Some(c) = s.pinakes_reader.cached(&uri) {
        return c;
    }
    let Some(d) = s.discovery() else {
        return Card::Unknown;
    };
    match d.resource_for(&uri).await {
        Ok(Some(r)) => {
            let c = Card::Found(Box::new(r));
            s.pinakes_reader.remember(&uri, &c);
            return c;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::debug!(%uri, "pinakes by-uri: {e}");
            return Card::Unknown;
        }
    }
    resolve(s, &uri).await
}

/// Ask Pinakes to read `uri`, once at a time per link. Never an import's search link: its
/// words are a private playlist's.
pub async fn resolve(s: &std::sync::Arc<NodeState>, uri: &str) -> Card {
    if crate::activity::is_search_link(uri) {
        return Card::Unknown;
    }
    let fut = {
        let mut inflight = s.pinakes_reader.inflight.lock().expect("reader inflight");
        if let Some(f) = inflight.get(uri) {
            f.clone()
        } else {
            let st = s.clone();
            let u = uri.to_string();
            let f: Inflight = async move {
                let card = match st.discovery() {
                    None => Card::Unknown,
                    Some(d) => {
                        st.pinakes_reader.pace().await;
                        match d.pinakes().resolve(&u).await {
                            Ok(r) if r.card.is_ok() => Card::Found(Box::new(r)),
                            Ok(r) => Card::Reading(Some(Box::new(r))),
                            Err(PinakesError::Pending(r)) => Card::Reading(Some(r)),
                            Err(e) => {
                                tracing::debug!(uri = %u, "pinakes resolve: {e}");
                                Card::Unknown
                            }
                        }
                    }
                };
                st.pinakes_reader.remember(&u, &card);
                st.pinakes_reader
                    .inflight
                    .lock()
                    .expect("reader inflight")
                    .remove(&u);
                card
            }
            .boxed()
            .shared();
            inflight.insert(uri.to_string(), f.clone());
            f
        }
    };
    fut.await
}

/// Resolve links in the background (rows that are not in Pinakes yet), so
/// the next look finds their cards. Paced like every resolve.
pub fn resolve_later(s: &std::sync::Arc<NodeState>, uris: Vec<String>) {
    if uris.is_empty() || s.discovery().is_none() {
        return;
    }
    let st = s.clone();
    tokio::spawn(async move {
        for u in uris {
            if st.pinakes_reader.cached(&u).is_none() {
                resolve(&st, &u).await;
            }
        }
    });
}

/// A link's card as `GET /v1/cards` answers it: what a collection shows for an item it doesn't
/// play (a web page, a post, an album the library hasn't got), TASK-pinakes-social-ux.md B3.
#[derive(Debug, serde::Serialize, PartialEq)]
pub struct CardView {
    pub uri: String,
    /// `found`, `reading` (Pinakes is reading the page: ask again shortly) or `unknown`.
    pub status: &'static str,
    /// The schema.org type Pinakes gives it (`Article`, `VideoObject`, `MusicAlbum`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Only ever an http(s) address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct CardQuery {
    pub uri: String,
}

/// `GET /v1/cards?uri=`: the Pinakes card for any link (cached, read on demand, never an error).
pub(crate) async fn get_card(
    axum::extract::State(s): axum::extract::State<std::sync::Arc<NodeState>>,
    axum::extract::Query(q): axum::extract::Query<CardQuery>,
) -> crate::ApiResult<CardView> {
    let uri = q.uri.trim().to_string();
    if uri.is_empty() {
        return Err(crate::ApiError::bad_request("uri is required"));
    }
    // An import's search link holds a private playlist's words: never asked of Pinakes
    if crate::activity::is_search_link(&uri) {
        return Ok(axum::Json(CardView {
            uri,
            status: "unknown",
            kind: None,
            name: None,
            description: None,
            image: None,
            site: None,
        }));
    }
    let web = |u: Option<String>| u.filter(|u| u.starts_with("https://") || u.starts_with("http://"));
    let view = |status, r: Option<Box<ResourceDetail>>| {
        let c = r.map(|r| r.card);
        CardView {
            uri: uri.clone(),
            status,
            kind: c.as_ref().and_then(|c| c.type_.clone()),
            name: c.as_ref().and_then(|c| c.name.clone()),
            description: c.as_ref().and_then(|c| c.description.clone()),
            image: web(c.as_ref().and_then(|c| c.image.clone())),
            site: c.and_then(|c| c.site),
        }
    };
    Ok(axum::Json(match card(&s, &uri).await {
        Card::Found(r) => view("found", Some(r)),
        Card::Reading(r) => view("reading", r),
        Card::Unknown => view("unknown", None),
    }))
}
