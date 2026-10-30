//! Where each playlist item plays from (STEWARD-UX-BRIEF §3, ADR 0015).
//!
//! The order of resolution, shown as the item's chip so nothing surprises the user:
//! 1. the item's pin, when that source is usable on this node;
//! 2. the playlist's own order (else the node's global order), over every source the item has
//!    here: a library file, each connected service's exact link (the item's own page, then
//!    Pinakes `also_on`);
//! 3. any other usable source (a directly streamable file link);
//! 4. otherwise unavailable, with the reason.
//!
//! The order lists `library` like any service, so "TIDAL before my files" is possible; the
//! default puts the library first.

use crate::discover::{self, Access, ServiceAccess, ServiceLink};
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::Json;
use futures_util::StreamExt;
use pm_library::Library;
use pm_model::{MusicUri, Track};
use pm_pubky::{ItemSource, Playlist};
use std::sync::Arc;

/// The services a play order can name, in the default order.
pub const ORDER_SERVICES: &[&str] = &["library", "tidal", "spotify", "jamendo"];
const ORDER_KEY: &str = "play.provider_order";

/// Known services first in the given order, unknown names dropped, missing ones appended in the
/// default order.
pub fn normalize(order: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in order {
        let s = s.trim().to_ascii_lowercase();
        if ORDER_SERVICES.contains(&s.as_str()) && !out.contains(&s) {
            out.push(s);
        }
    }
    for s in ORDER_SERVICES {
        if !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    }
    out
}

/// The node's global order (Settings).
pub fn global_order(lib: &Library) -> Vec<String> {
    let saved: Vec<String> = lib
        .get_setting(ORDER_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    normalize(&saved)
}

fn service_label(id: &str) -> String {
    match id {
        "library" => "Your library".into(),
        other => discover::service_title(other),
    }
}

/// One way an item could play here.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub service: String,
    pub url: Option<String>,
    pub label: String,
    pub mode: &'static str,
    pub available: bool,
    pub link: Option<ServiceLink>,
    pub track: Option<Track>,
    pub stream: Option<pm_zones::QueueItem>,
}

fn same_url(a: &str, b: &str) -> bool {
    let n = |u: &str| {
        u.trim_end_matches('/')
            .replace("://www.", "://")
            .replace("://listen.", "://")
            .to_lowercase()
    };
    n(a) == n(b)
}

/// Pick a candidate: the pin when it is usable, else the first usable one in `order`, else the
/// first usable one at all. Returns the index and whether the pin decided.
pub(crate) fn choose(
    cands: &[Candidate],
    pin: Option<&ItemSource>,
    order: &[String],
) -> (Option<usize>, bool) {
    if let Some(pin) = pin {
        let hit = cands.iter().position(|c| {
            c.available
                && c.service == pin.service
                && match (&pin.uri, &c.url) {
                    (Some(p), Some(u)) => same_url(p, u),
                    (Some(_), None) => false,
                    (None, _) => true,
                }
        });
        if hit.is_some() {
            return (hit, true);
        }
    }
    for svc in order {
        if let Some(i) = cands.iter().position(|c| c.available && &c.service == svc) {
            return (Some(i), false);
        }
    }
    (cands.iter().position(|c| c.available), false)
}

fn link_candidate(l: ServiceLink, a: Access, it: &DiscoverItem) -> Candidate {
    let pf = discover::plays_from_link(&l, a);
    let (available, mode) = match a {
        Access::Full => (true, "full"),
        Access::External => (true, "connect"),
        Access::Preview => (false, "preview"),
        Access::No => (false, "none"),
    };
    let queue_uri = match l.service {
        "tidal" => Some(crate::tidal::track_uri(&l.id)),
        "jamendo" => Some(crate::jamendo::track_uri(&l.id)),
        _ => None,
    };
    let stream = queue_uri
        .filter(|_| a == Access::Full && l.kind == "track")
        .map(|uri| pm_zones::QueueItem {
            id: 0,
            uri,
            title: it.title.clone(),
            artist: it.artist.clone(),
            album: None,
            duration_ms: None,
            image: it.image.clone(),
            limit: None,
        });
    Candidate {
        service: l.service.to_string(),
        url: Some(l.url.clone()),
        label: match a {
            Access::No => format!("{} · not connected", discover::service_title(l.service)),
            _ => pf.label,
        },
        mode,
        available,
        link: Some(l),
        track: None,
        stream,
    }
}

/// Every source this item has on this node. `track` is the library's file for it, `it` the
/// described row (Pinakes or the provider), `pin` adds the pinned link when nothing else has it.
pub(crate) fn candidates(
    acc: &ServiceAccess,
    uri: &str,
    track: Option<Track>,
    it: &DiscoverItem,
    pin: Option<&ItemSource>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(t) = track.or_else(|| it.in_library.clone()) {
        out.push(Candidate {
            service: "library".into(),
            url: None,
            label: format!("Your library · {}", t.file.format),
            mode: "full",
            available: true,
            link: None,
            track: Some(t),
            stream: None,
        });
    }
    let mut links: Vec<ServiceLink> = Vec::new();
    let mut push = |l: ServiceLink| {
        if l.kind == "track" && !links.iter().any(|x| x.service == l.service && x.id == l.id) {
            links.push(l);
        }
    };
    if let Some(p) = pin
        .and_then(|p| p.uri.as_deref())
        .and_then(|u| discover::service_link(u, true))
    {
        push(p);
    }
    for u in [Some(uri), it.page.as_deref()].into_iter().flatten() {
        if let Some(l) = discover::service_link(u, it.taggers.unwrap_or(0) > 0) {
            push(l);
        }
    }
    for v in &it.also_on {
        if let Some(l) = discover::service_link(&v.url, v.label.contains("tagged")) {
            push(l);
        }
    }
    // One row per service: the pinned link, else the first (own page, then tagged links first).
    let mut seen: Vec<&'static str> = Vec::new();
    for l in links {
        if seen.contains(&l.service) {
            continue;
        }
        seen.push(l.service);
        let a = discover::access(acc, l.service);
        out.push(link_candidate(l, a, it));
    }
    if let Some(q) = crate::playlists::stream_item(uri) {
        out.push(Candidate {
            service: "archive".into(),
            url: Some(uri.to_string()),
            label: "Internet Archive".into(),
            mode: "full",
            available: true,
            link: None,
            track: None,
            stream: Some(q),
        });
    }
    out
}

/// The answer for one item, plus what to queue.
pub(crate) struct Resolved {
    pub view: PlaylistItemPlay,
    pub queue: Option<pm_zones::QueueItem>,
}

fn resolve_item(
    acc: &ServiceAccess,
    order: &[String],
    index: usize,
    item: &pm_pubky::PlaylistItem,
    track: Option<Track>,
    it: &DiscoverItem,
) -> Resolved {
    let pin = item.source.as_ref();
    let cands = candidates(acc, &item.uri, track, it, pin);
    let (chosen, by_pin) = choose(&cands, pin, order);
    let c = chosen.map(|i| &cands[i]);
    let plays_from = c.map(|c| PlaysFrom {
        service: c.service.clone(),
        kind: "track".into(),
        id: match (&c.track, &c.link) {
            (Some(t), _) => t.id.map(|i| i.0.to_string()).unwrap_or_default(),
            (None, Some(l)) => format!("{}:{}", l.kind, l.id),
            _ => String::new(),
        },
        label: c.label.clone(),
        url: c.url.clone(),
        mode: c.mode.into(),
    });
    let connect: Vec<String> = {
        let mut v: Vec<String> = cands
            .iter()
            .filter(|c| c.mode == "none")
            .map(|c| c.service.clone())
            .collect();
        // Each service once ("TIDAL or Spotify", not "TIDAL or Spotify or TIDAL or Spotify")
        let mut seen = std::collections::HashSet::new();
        v.retain(|x| seen.insert(x.clone()));
        v
    };
    let reason = c.is_none().then(|| {
        if cands.is_empty() {
            "No service has this yet".to_string()
        } else {
            "Not on your connected services".to_string()
        }
    });
    let track_of = c.and_then(|c| c.track.clone());
    let queue = match c {
        Some(c) if c.track.is_some() => c.track.as_ref().and_then(crate::sources::NodeSources::item_for),
        Some(c) => c.stream.clone(),
        None => None,
    };
    let title = track_of
        .as_ref()
        .map(|t| t.title.clone())
        .or_else(|| it.title.clone());
    Resolved {
        view: PlaylistItemPlay {
            index,
            uri: item.uri.clone(),
            title,
            artist: track_of
                .as_ref()
                .and_then(|t| t.artists.first().map(|a| a.name.clone()))
                .or_else(|| it.artist.clone()),
            image: it.image.clone(),
            page: it.page.clone().or_else(|| Some(item.uri.clone())),
            site: it.site.clone(),
            duration_ms: track_of.as_ref().and_then(|t| t.duration_ms),
            track_id: track_of.as_ref().and_then(|t| t.id.map(|i| i.0)),
            stream_uri: c.and_then(|c| c.stream.as_ref().map(|q| q.uri.clone())),
            plays_from,
            pin: item.source.clone(),
            pin_unavailable: pin.is_some() && !by_pin,
            sources: cands
                .iter()
                .enumerate()
                .map(|(k, c)| PlaySource {
                    service: c.service.clone(),
                    name: service_label(&c.service),
                    url: c.url.clone(),
                    label: c.label.clone(),
                    mode: c.mode.into(),
                    available: c.available,
                    chosen: Some(k) == chosen,
                })
                .collect(),
            reason,
            connect,
        },
        queue,
    }
}

/// Resolve every item of a playlist (Pinakes answers in one cached batch).
pub(crate) async fn resolve_playlist(s: &Arc<NodeState>, p: &Playlist) -> (Vec<String>, bool, Vec<Resolved>) {
    let acc = discover::service_access(s);
    let (order, own, tracks) = {
        let lib = s.library.lock().expect("library poisoned");
        let own = p.source_order.as_ref().is_some_and(|o| !o.is_empty());
        let order = match &p.source_order {
            Some(o) if !o.is_empty() => normalize(o),
            _ => global_order(&lib),
        };
        let tracks: Vec<Option<Track>> = p
            .items
            .iter()
            .map(|it| {
                MusicUri::parse(&it.uri)
                    .ok()
                    .and_then(|u| lib.tracks_by_canonical(&u).ok())
                    .and_then(|v| v.into_iter().next())
            })
            .collect();
        (order, own, tracks)
    };
    let uris: Vec<String> = p.items.iter().map(|i| i.uri.clone()).collect();
    let mut described = discover::describe_uris(s, &uris).await;
    // MusicBrainz songs Pinakes has no links for yet: ask by key (cached a day)
    let fills = described
        .iter_mut()
        .zip(&tracks)
        .filter(|(it, t)| t.is_none() && it.also_on.is_empty() && it.in_library.is_none())
        .map(|(it, _)| discover::fill_from_key(s, it, None));
    futures_util::future::join_all(fills).await;
    // Still nothing on a connected service: ask each one by the song's ISRC (an ISRC link, the
    // library file's, else MusicBrainz's for a recording)
    let jobs: Vec<(usize, String, Option<String>, Vec<String>)> = described
        .iter()
        .zip(&tracks)
        .enumerate()
        .filter(|(_, (_, t))| t.is_none())
        .filter_map(|(k, (it, t))| {
            let has = |svc: &str| {
                it.also_on
                    .iter()
                    .any(|v| discover::service_link(&v.url, false).is_some_and(|l| l.service == svc))
                    || discover::service_link(&it.uri, false).is_some_and(|l| l.service == svc)
            };
            let mut skip: Vec<String> = Vec::new();
            if discover::access(&acc, "tidal") != Access::Full || has("tidal") {
                skip.push("tidal".into());
            }
            if discover::access(&acc, "spotify") != Access::External || has("spotify") {
                skip.push("spotify".into());
            }
            if skip.len() == 2 {
                return None;
            }
            let isrc = isrc_of(&it.uri, t.as_ref());
            (isrc.is_some() || it.uri.starts_with("https://musicbrainz.org/recording/"))
                .then(|| (k, it.uri.clone(), isrc, skip))
        })
        .collect();
    let found: Vec<(usize, Vec<Version>)> =
        futures_util::stream::iter(jobs.into_iter().map(|(k, uri, isrc, skip)| {
            let s = s.clone();
            async move {
                let isrc = match isrc {
                    Some(i) => Some(i),
                    None => discover::mb_recording(&s, &uri)
                        .await
                        .and_then(|m| m.isrcs.into_iter().next()),
                };
                let skip: Vec<&str> = skip.iter().map(String::as_str).collect();
                let v = match isrc {
                    Some(i) => discover::isrc_versions_for(&s, &i, &skip).await,
                    None => Vec::new(),
                };
                (k, v)
            }
        }))
        .buffer_unordered(6)
        .collect()
        .await;
    for (k, v) in found {
        described[k].also_on.extend(v);
    }
    let mut out: Vec<Resolved> = p
        .items
        .iter()
        .zip(tracks)
        .zip(described.iter())
        .enumerate()
        .map(|(k, ((item, track), it))| resolve_item(&acc, &order, k, item, track, it))
        .collect();
    // Songs known only by an id (an ISRC link, or a placeholder title): name them from the
    // service track they play from, which knows its own title (cached a day)
    let jobs: Vec<(usize, String)> = out
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.view.track_id.is_none()
                && (r.view.uri.starts_with("https://musicbrainz.org/isrc/")
                    || !named(r.view.title.as_deref()))
        })
        .filter_map(|(k, r)| Some((k, r.view.plays_from.as_ref()?.url.clone()?)))
        .collect();
    let names: Vec<(usize, DiscoverItem)> = futures_util::stream::iter(jobs.into_iter().map(|(k, url)| {
        let s = s.clone();
        async move { (k, provider_name(&s, &url).await) }
    }))
    .buffer_unordered(6)
    .filter_map(|(k, n)| async move { n.map(|n| (k, n)) })
    .collect()
    .await;
    for (k, n) in names {
        let r = &mut out[k];
        if named(n.title.as_deref()) {
            r.view.title = n.title.clone();
            r.view.artist = n.artist.clone().or_else(|| r.view.artist.clone());
        }
        r.view.image = n.image.clone().or_else(|| r.view.image.clone());
        if let Some(q) = r.queue.as_mut() {
            q.title = r.view.title.clone();
            q.artist = r.view.artist.clone();
            q.image = r.view.image.clone();
        }
    }
    for r in out.iter_mut().filter(|r| !named(r.view.title.as_deref())) {
        r.view.title = None;
    }
    (order, own, out)
}

/// A real title, not a placeholder ("ISRC DEQ121746047").
fn named(t: Option<&str>) -> bool {
    t.is_some_and(|t| {
        let t = t.trim();
        !t.is_empty()
            && !(t.len() == 17 && t.starts_with("ISRC ") && t[5..].bytes().all(|b| b.is_ascii_alphanumeric()))
    })
}

/// A service track's own title, artist and cover (cached a day per link).
async fn provider_name(s: &NodeState, url: &str) -> Option<DiscoverItem> {
    let key = format!("playfrom.name.{url}");
    let now = pm_library::db::now_unix();
    let cached = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(&key)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str::<(i64, DiscoverItem)>(&j).ok());
    if let Some((at, it)) = cached {
        if now - at < 24 * 3600 {
            return Some(it);
        }
    }
    let it = discover::describe_from_provider(s, url).await?;
    if let Ok(j) = serde_json::to_string(&(now, &it)) {
        let _ = s.library.lock().expect("library poisoned").set_setting(&key, &j);
    }
    Some(it)
}

/// The ISRC behind an item: its own ISRC link, else its library track's.
fn isrc_of(uri: &str, track: Option<&Track>) -> Option<String> {
    if let Some(i) = uri.strip_prefix("https://musicbrainz.org/isrc/") {
        return Some(i.trim_end_matches('/').to_ascii_uppercase());
    }
    track.and_then(|t| t.isrc.as_ref()).map(|i| i.to_string())
}

// ---- handlers ----

/// `GET /v1/play-order`: the node's global order and what each service can do here.
pub(crate) async fn get_order(State(s): State<Arc<NodeState>>) -> ApiResult<PlayOrder> {
    let acc = discover::service_access(&s);
    let order = global_order(&s.library.lock().expect("library poisoned"));
    Ok(Json(play_order(order, &acc)))
}

/// A play order with what each service can do here.
pub(crate) fn play_order_of(order: Vec<String>, acc: &ServiceAccess) -> PlayOrder {
    play_order(order, acc)
}

fn play_order(order: Vec<String>, acc: &ServiceAccess) -> PlayOrder {
    let services = order
        .iter()
        .map(|id| {
            let (connected, note) = match id.as_str() {
                "library" => (true, None),
                other if discover::turned_off(acc, other) => {
                    (false, Some("turned off on Sources".to_string()))
                }
                other => match discover::access(acc, other) {
                    Access::Full => (true, None),
                    Access::External => (true, Some("plays on a Spotify Connect device".to_string())),
                    Access::Preview => (false, Some("30 s previews only".to_string())),
                    Access::No => (false, Some("not connected".to_string())),
                },
            };
            PlayService {
                id: id.clone(),
                name: service_label(id),
                connected,
                note,
            }
        })
        .collect();
    PlayOrder { order, services }
}

/// `PUT /v1/play-order {order}`.
pub(crate) async fn set_order(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<SetPlayOrder>,
) -> ApiResult<PlayOrder> {
    let order = normalize(&body.order);
    s.library.lock().expect("library poisoned").set_setting(
        ORDER_KEY,
        &serde_json::to_string(&order).expect("strings serialize"),
    )?;
    let acc = discover::service_access(&s);
    s.notify_library();
    Ok(Json(play_order(order, &acc)))
}

/// `GET /v1/playlists/{id}/play-from`: every item's chip, pin and sources.
pub(crate) async fn playlist_play_from(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<PlaylistPlayFrom> {
    let p = crate::playlists::load_playlist(&s, &id)?;
    let (order, own_order, items) = resolve_playlist(&s, &p).await;
    let acc = discover::service_access(&s);
    Ok(Json(PlaylistPlayFrom {
        order: play_order(order, &acc),
        own_order,
        items: items.into_iter().map(|r| r.view).collect(),
    }))
}

/// Check a requested order: known services only.
pub(crate) fn checked_order(order: &[String]) -> Result<Vec<String>, ApiError> {
    for s in order {
        if !ORDER_SERVICES.contains(&s.trim().to_ascii_lowercase().as_str()) {
            return Err(ApiError::bad_request(format!(
                "unknown service {s:?}: the order lists {}",
                ORDER_SERVICES.join(", ")
            )));
        }
    }
    Ok(normalize(order))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(service: &str, url: Option<&str>, available: bool) -> Candidate {
        Candidate {
            service: service.into(),
            url: url.map(Into::into),
            label: service.into(),
            mode: if available { "full" } else { "none" },
            available,
            link: None,
            track: None,
            stream: None,
        }
    }

    fn order(o: &[&str]) -> Vec<String> {
        normalize(&o.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_order_is_tidied_and_complete() {
        assert_eq!(
            order(&["Spotify", "nope", "spotify"]),
            ["spotify", "library", "tidal", "jamendo"]
        );
        assert_eq!(order(&[]), ["library", "tidal", "spotify", "jamendo"]);
        assert!(checked_order(&["qobuz".into()]).is_err());
    }

    #[test]
    fn pin_then_order_then_anything_usable() {
        let c = vec![
            cand("library", None, true),
            cand("tidal", Some("https://tidal.com/track/1"), true),
            cand("spotify", Some("https://open.spotify.com/track/x"), false),
            cand("archive", Some("https://archive.org/download/a/b.mp3"), true),
        ];
        // default order: the library wins
        assert_eq!(choose(&c, None, &order(&[])), (Some(0), false));
        // the playlist prefers TIDAL
        assert_eq!(choose(&c, None, &order(&["tidal"])), (Some(1), false));
        // a pin on TIDAL wins over the order, matched by its exact link
        let pin = ItemSource {
            service: "tidal".into(),
            uri: Some("https://listen.tidal.com/track/1/".into()),
        };
        assert_eq!(choose(&c, Some(&pin), &order(&["library"])), (Some(1), true));
        // a pin on a service that is not connected falls back to the order
        let pin = ItemSource {
            service: "spotify".into(),
            uri: None,
        };
        assert_eq!(choose(&c, Some(&pin), &order(&[])), (Some(0), false));
        // nothing in the order is usable: any usable source
        assert_eq!(choose(&c[2..], None, &order(&[])), (Some(1), false));
        // nothing usable at all
        assert_eq!(choose(&c[2..3], None, &order(&[])), (None, false));
    }
}
