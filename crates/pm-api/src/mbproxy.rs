//! A MusicBrainz `/ws/2` caching proxy, so Pinakes (and the owner's other
//! tools) can point their `base_url` at this node instead of musicbrainz.org:
//! answers are cached for a week, upstream calls are spaced one second apart
//! and carry the node's contact (MusicBrainz's terms). Read-only, GET only,
//! on when the node has a contact (discovery on), off otherwise.

use crate::state::NodeState;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const MAX_ENTRIES: usize = 20_000;
const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Clone)]
struct Cached {
    at: Instant,
    status: u16,
    content_type: String,
    body: Arc<Vec<u8>>,
}

struct Proxy {
    cache: tokio::sync::Mutex<HashMap<String, Cached>>,
}

fn proxy() -> &'static Proxy {
    static P: std::sync::OnceLock<Proxy> = std::sync::OnceLock::new();
    P.get_or_init(|| Proxy {
        cache: tokio::sync::Mutex::new(HashMap::new()),
    })
}

/// `GET /mb/ws/2/{*path}?…`: the same request against musicbrainz.org.
pub(crate) async fn forward(
    State(s): State<Arc<NodeState>>,
    Path(path): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    if s.contact().trim().is_empty() {
        return (
            StatusCode::PRECONDITION_FAILED,
            "the MusicBrainz proxy needs the node's upstream contact (sign in with Pubky Ring or set one in Settings)",
        )
            .into_response();
    }
    let path = path.trim_start_matches('/');
    if path.is_empty()
        || path.contains("..")
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
    {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let mut q = q;
    q.retain(|(k, _)| !k.is_empty());
    if !q.iter().any(|(k, _)| k == "fmt") {
        q.push(("fmt".into(), "json".into()));
    }
    let url = url::Url::parse_with_params(
        &format!(
            "{}/{path}",
            s.mb_base.read().expect("mb lock").trim_end_matches('/')
        ),
        &q,
    )
    .map(|u| u.to_string())
    .unwrap_or_default();
    if url.is_empty() {
        return (StatusCode::BAD_REQUEST, "bad query").into_response();
    }
    let p = proxy();
    if let Some(c) = p.cache.lock().await.get(&url).cloned() {
        if c.at.elapsed() < TTL {
            return cached_response(&c, true);
        }
    }
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .filter(|a| a.contains("xml"))
        .map(|_| "application/xml")
        .unwrap_or("application/json");
    // MusicBrainz answers 503 when the IP goes over its rate; wait as it asks
    // (Retry-After, else two seconds) and try once more before passing it on.
    // One request at a time with the rest of the node (pm_library::musicbrainz::take_turn)
    let mut attempt = 0;
    let resp = loop {
        attempt += 1;
        let mut turn = pm_library::musicbrainz::take_turn(&url, Duration::from_secs(1)).await;
        let r = match s
            .http_client()
            .get(&url)
            .header(header::ACCEPT, accept)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("musicbrainz: {e}")).into_response(),
        };
        if matches!(r.status().as_u16(), 503 | 429) {
            let wait = pm_library::musicbrainz::retry_after(r.headers(), Duration::from_secs(2));
            turn.back_off(wait);
            if attempt < 2 {
                tracing::debug!(%url, ?wait, "musicbrainz asked to slow down");
                drop(turn);
                continue;
            }
        }
        break r;
    };
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let body = match resp.bytes().await {
        Ok(b) if b.len() <= MAX_BODY => b.to_vec(),
        Ok(_) => return (StatusCode::BAD_GATEWAY, "musicbrainz: answer too large").into_response(),
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("musicbrainz: {e}")).into_response(),
    };
    let entry = Cached {
        at: Instant::now(),
        status,
        content_type,
        body: Arc::new(body),
    };
    if status == 200 || status == 404 {
        let mut cache = p.cache.lock().await;
        if cache.len() >= MAX_ENTRIES {
            cache.retain(|_, c| c.at.elapsed() < TTL / 2);
            if cache.len() >= MAX_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(url, entry.clone());
    }
    cached_response(&entry, false)
}

fn cached_response(c: &Cached, hit: bool) -> Response {
    let mut r = Response::builder()
        .status(StatusCode::from_u16(c.status).unwrap_or(StatusCode::BAD_GATEWAY))
        .header(header::CONTENT_TYPE, c.content_type.clone())
        .header("X-Cache", if hit { "HIT" } else { "MISS" })
        .header(header::CACHE_CONTROL, "public, max-age=604800")
        .body(Body::from(c.body.as_ref().clone()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    r.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    r
}
