//! Where a request came from. The renderer routes take no token (renderers never do), so they
//! answer LAN peers only: a private or loopback source address on the node's own listener, not
//! through iroh ([`Remote`], its own listener) and not through a proxy or tunnel (Cloudflare,
//! a reverse proxy: any forwarding header). A node without an API token answers as the owner
//! only to requests that did not come through iroh or a proxy.

use crate::state::NodeState;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// Marks requests that came through the iroh tunnel (the remote listener adds it).
#[derive(Debug, Clone, Copy)]
pub struct Remote;

/// Headers a proxy or tunnel adds; a request carrying any of them did not come from the LAN
/// peer it seems to (a forged one only makes a LAN request look proxied).
const FORWARDED: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-real-ip",
    "cf-connecting-ip",
    "cf-ray",
];

fn proxied(h: &HeaderMap) -> bool {
    FORWARDED.iter().any(|k| h.contains_key(*k))
}

/// The request reached the node's listener directly: not through iroh, not through a proxy.
pub fn direct(req: &Request) -> bool {
    req.extensions().get::<Remote>().is_none() && !proxied(req.headers())
}

/// The source address of a direct request from the LAN or this machine, if it is one.
pub fn lan_peer(req: &Request) -> Option<IpAddr> {
    if !direct(req) {
        return None;
    }
    let ip = req.extensions().get::<ConnectInfo<SocketAddr>>()?.0.ip();
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    let lan = match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        // Loopback, unique local (fc00::/7) and link-local (fe80::/10)
        IpAddr::V6(v6) => {
            v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    };
    lan.then_some(ip)
}

/// Layer for the renderer routes: LAN peers only, and the handlers learn who asked.
pub async fn lan_only(mut req: Request, next: Next) -> Response {
    let Some(ip) = lan_peer(&req) else {
        return (
            StatusCode::FORBIDDEN,
            "the renderer answers on the local network only",
        )
            .into_response();
    };
    req.extensions_mut().insert(pm_renderer::Peer(ip));
    next.run(req).await
}

/// Layer for the whole API: a node without an API token answers only requests that reached its
/// listener directly, never through iroh or a tunnel (it would make whoever is there the owner).
pub async fn tokenless_direct_only(State(s): State<Arc<NodeState>>, req: Request, next: Next) -> Response {
    if s.token.is_none() && !direct(&req) {
        return (
            StatusCode::UNAUTHORIZED,
            "this node has no API token, so it answers only directly; set [api] token to reach it remotely",
        )
            .into_response();
    }
    next.run(req).await
}

/// Serve the API for the iroh tunnel on its own loopback listener, every request marked
/// [`Remote`]; the tunnel's target. Returns the listener's address.
pub async fn serve_remote(state: Arc<NodeState>) -> std::io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let app = crate::router(state).layer(axum::Extension(Remote));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await {
            tracing::warn!("the remote-access listener stopped: {e}");
        }
    });
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn req(from: Option<&str>, headers: &[(&str, &str)], remote: bool) -> Request {
        let mut b = axum::http::Request::builder().uri("/upnp/zone/z/desc.xml");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut r = b.body(Body::empty()).unwrap();
        if let Some(a) = from {
            r.extensions_mut()
                .insert(ConnectInfo(a.parse::<SocketAddr>().unwrap()));
        }
        if remote {
            r.extensions_mut().insert(Remote);
        }
        r
    }

    #[test]
    fn only_direct_lan_peers_reach_the_renderer() {
        for a in [
            "192.168.1.20:5000",
            "10.0.0.3:1",
            "172.20.0.5:1",
            "127.0.0.1:9",
            "[::1]:9",
            "[fd00::5]:1",
            "[::ffff:192.168.1.2]:1",
        ] {
            assert!(lan_peer(&req(Some(a), &[], false)).is_some(), "{a}");
        }
        for a in [
            "8.8.8.8:53",
            "100.64.0.1:1",
            "[2001:db8::1]:1",
            "[::ffff:1.2.3.4]:1",
        ] {
            assert!(lan_peer(&req(Some(a), &[], false)).is_none(), "{a}");
        }
        // Through iroh (its listener is on loopback), through Cloudflare or a proxy, or unknown
        assert!(lan_peer(&req(Some("127.0.0.1:9"), &[], true)).is_none());
        assert!(lan_peer(&req(
            Some("172.18.0.4:1"),
            &[("cf-connecting-ip", "1.2.3.4")],
            false
        ))
        .is_none());
        assert!(lan_peer(&req(
            Some("127.0.0.1:9"),
            &[("x-forwarded-for", "1.2.3.4")],
            false
        ))
        .is_none());
        assert!(lan_peer(&req(None, &[], false)).is_none());
        assert!(direct(&req(Some("127.0.0.1:9"), &[], false)));
        assert!(!direct(&req(Some("127.0.0.1:9"), &[], true)));
    }
}
