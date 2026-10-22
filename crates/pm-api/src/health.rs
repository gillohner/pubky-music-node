//! Which outside parts answer right now (STEWARD-PLAN follow-up: "the UI says which part is
//! down"). The node keeps playing what it can without them: the library and its files never
//! depend on these. Each part is asked at most every 30 seconds, all at once, a few seconds each.

use crate::state::NodeState;
use crate::types::HealthPart;
use crate::ApiResult;
use axum::extract::State;
use axum::Json;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(30);
const PROBE: Duration = Duration::from_secs(4);

/// The last answer per node (tests run several nodes in one process).
type Cache = Mutex<std::collections::HashMap<String, (Instant, Vec<HealthPart>)>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Any HTTP answer means the part is reachable; a timeout or a refused connection means it is not.
async fn reachable(s: &NodeState, url: &str) -> Result<u64, String> {
    let t = Instant::now();
    match tokio::time::timeout(PROBE, s.http_client().get(url).send()).await {
        Ok(Ok(_)) => Ok(t.elapsed().as_millis() as u64),
        Ok(Err(e)) => Err(if e.is_connect() {
            "cannot connect".into()
        } else {
            "no answer".into()
        }),
        Err(_) => Err(format!("no answer in {} s", PROBE.as_secs())),
    }
}

fn part(id: &str, name: &str, r: Result<u64, String>) -> HealthPart {
    match r {
        Ok(ms) => HealthPart {
            id: id.into(),
            name: name.into(),
            state: "ok".into(),
            detail: None,
            ms: Some(ms),
        },
        Err(e) => HealthPart {
            id: id.into(),
            name: name.into(),
            state: "down".into(),
            detail: Some(e),
            ms: None,
        },
    }
}

fn refresh_in_background(s: &Arc<NodeState>) {
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let s = s.clone();
    tokio::spawn(async move {
        let parts = probe_all(&s).await;
        cache()
            .lock()
            .expect("health cache")
            .insert(s.node_id.clone(), (Instant::now(), parts));
        RUNNING.store(false, std::sync::atomic::Ordering::Release);
    });
}

async fn probe_all(s: &Arc<NodeState>) -> Vec<HealthPart> {
    let pinakes = s
        .discovery()
        .map(|d| d.pinakes().base().trim_end_matches('/').to_string());
    let tidal = crate::tidal::connected(s).then(|| s.tidal.read().expect("tidal").api_base.clone());
    let jamendo = crate::jamendo::configured(s).then(|| s.jamendo.read().expect("jamendo").base.clone());
    let subsonic = s
        .opensubsonic
        .read()
        .expect("opensubsonic")
        .as_ref()
        .map(|v| format!("{}/rest/ping", v.url.trim_end_matches('/')));
    let jellyfin = s
        .jellyfin
        .read()
        .expect("jellyfin")
        .as_ref()
        .map(|v| format!("{}/System/Info/Public", v.url.trim_end_matches('/')));
    let sync = s.sync_handle();
    let (p, t, j, sub, jf, hs) = tokio::join!(
        async {
            match &pinakes {
                Some(b) => Some(part(
                    "pinakes",
                    "Pinakes (Home, links, tags)",
                    reachable(s, &format!("{b}/v1/info")).await,
                )),
                None => None,
            }
        },
        async {
            match &tidal {
                Some(b) => Some(part("tidal", "TIDAL", reachable(s, b).await)),
                None => None,
            }
        },
        async {
            match &jamendo {
                Some(b) => Some(part("jamendo", "Jamendo", reachable(s, b).await)),
                None => None,
            }
        },
        async {
            match &subsonic {
                Some(u) => Some(part("subsonic", "Your server", reachable(s, u).await)),
                None => None,
            }
        },
        async {
            match &jellyfin {
                Some(u) => Some(part("jellyfin", "Jellyfin", reachable(s, u).await)),
                None => None,
            }
        },
        async {
            let sync = sync.as_ref()?;
            if sync.needs_resign() {
                return Some(HealthPart {
                    id: "homeserver".into(),
                    name: "Your homeserver".into(),
                    state: "needs_sign_in".into(),
                    detail: Some(crate::sync::RESIGN_NOTICE.into()),
                    ms: None,
                });
            }
            let t = Instant::now();
            let r = match tokio::time::timeout(PROBE, sync.list_private_files()).await {
                Ok(Ok(_)) => Ok(t.elapsed().as_millis() as u64),
                Ok(Err(e)) if crate::sync::is_refusal(&e.to_string()) => {
                    sync.note_refusal();
                    Err(crate::sync::RESIGN_NOTICE.to_string())
                }
                Ok(Err(_)) => Err("no answer".to_string()),
                Err(_) => Err(format!("no answer in {} s", PROBE.as_secs())),
            };
            Some(part("homeserver", "Your homeserver", r))
        },
    );
    [hs, p, t, j, sub, jf].into_iter().flatten().collect()
}

/// `GET /v1/health`: each outside part the node uses, `ok`, `down` (with why) or
/// `needs_sign_in`. Parts that are not set up are left out.
pub(crate) async fn health(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<HealthPart>> {
    if let Some((at, parts)) = cache().lock().expect("health cache").get(&s.node_id).cloned() {
        if at.elapsed() >= TTL {
            // The last answer now, a fresh look in the background (a cold check took 1.3 s and
            // held one of the browser's few connections to the node)
            refresh_in_background(&s);
        }
        return Ok(Json(parts));
    }
    let parts = probe_all(&s).await;
    cache()
        .lock()
        .expect("health cache")
        .insert(s.node_id.clone(), (Instant::now(), parts.clone()));
    Ok(Json(parts))
}
