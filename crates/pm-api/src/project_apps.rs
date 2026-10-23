//! The project's own service apps, built in so a self-hoster configures nothing
//! (STEWARD-SHARED-APPS.md). Only public values: client ids and the AcoustID application key.
//! Client secrets are never built in; every sign-in with a project app is a public-client flow
//! (OAuth 2 authorization code with PKCE, no secret) that returns through one static page, the
//! [`RETURN_PAGE`], which forwards the browser to the node named in `state`.
//!
//! The values come from the build (`PM_PROJECT_TIDAL_CLIENT_ID`, `PM_PROJECT_JAMENDO_CLIENT_ID`,
//! `PM_PROJECT_ACOUSTID_KEY`), set by the release workflow, so the repository holds none of them.
//! A build without them simply has no built-in apps. A node's own `[tidal]`, `[jamendo]` or
//! `[acoustid]` values always win.
//!
//! The one built-in secret is the **TIDAL device client** for Full tracks (STEWARD-TIDAL-DEFAULT.md:
//! the client python-tidal publishes, supplied by the owner), compiled in from
//! `PM_TIDAL_DEVICE_CLIENT_ID` / `PM_TIDAL_DEVICE_CLIENT_SECRET` as masked bytes (build.rs), so the
//! plain values are in neither the source nor the binary's strings. It is not a security boundary;
//! it keeps the values out of casual sight. A device client the owner enters always wins.

mod device {
    include!(concat!(env!("OUT_DIR"), "/tidal_device.rs"));
}

const MASK: &[u8] = b"pubky-music-node:tidal-device";

fn unmask(b: &[u8]) -> String {
    b.iter()
        .enumerate()
        .map(|(i, x)| (x ^ MASK[i % MASK.len()] ^ (i as u8)) as char)
        .collect()
}

/// The built-in TIDAL device client `(id, secret)` for Full tracks, when the build carries one.
pub fn tidal_device_client() -> Option<(String, String)> {
    device::DEVICE.map(|(id, secret)| (unmask(id), unmask(secret)))
}

/// The project's TIDAL app (public client id).
pub fn tidal_client_id() -> Option<&'static str> {
    option_env!("PM_PROJECT_TIDAL_CLIENT_ID").filter(|v| !v.trim().is_empty())
}

/// The project's Jamendo app (public client id; streaming and the catalogue need nothing more).
pub fn jamendo_client_id() -> Option<&'static str> {
    option_env!("PM_PROJECT_JAMENDO_CLIENT_ID").filter(|v| !v.trim().is_empty())
}

/// The project's AcoustID application key (meant to ship inside the application).
pub fn acoustid_key() -> Option<&'static str> {
    option_env!("PM_PROJECT_ACOUSTID_KEY").filter(|v| !v.trim().is_empty())
}

/// The project's fanart.tv key (a public project key; artist portraits and backgrounds).
pub fn fanart_key() -> Option<&'static str> {
    option_env!("PM_PROJECT_FANART_KEY").filter(|v| !v.trim().is_empty())
}

/// The project's TheAudioDB key (a public project key; artist portraits and biographies). Without
/// one, TheAudioDB's public test key `123` is used, which allows a few requests a minute.
pub fn audiodb_key() -> &'static str {
    option_env!("PM_PROJECT_AUDIODB_KEY")
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("123")
}

/// The static sign-in return page registered as the redirect URI on the project's apps
/// (source: github.com/gillohner/pubky-music-auth). `[auth] return_page` overrides it.
pub const RETURN_PAGE: &str = "https://gillohner.github.io/pubky-music-auth/callback";

/// `state` for a sign-in through the return page: `<nonce>.<base64url(callback URL)>`.
pub fn return_state(nonce: &str, callback: &str) -> String {
    use base64::Engine;
    format!(
        "{nonce}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(callback.as_bytes())
    )
}

/// The nonce of a `state` (the whole value when it carries no callback).
pub fn state_nonce(state: &str) -> &str {
    state.split('.').next().unwrap_or(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_client_unmasks_what_the_build_masked() {
        // The same arithmetic as build.rs
        let mask = |v: &str| -> Vec<u8> {
            v.bytes()
                .enumerate()
                .map(|(i, b)| b ^ MASK[i % MASK.len()] ^ (i as u8))
                .collect()
        };
        let m = mask("zU4JXAiEUnGhXp4d");
        assert_ne!(&m[..], b"zU4JXAiEUnGhXp4d");
        assert!(!String::from_utf8_lossy(&m).contains("zU4J"));
        assert_eq!(unmask(&m), "zU4JXAiEUnGhXp4d");
        // A build without the values carries none
        if std::env::var("PM_TIDAL_DEVICE_CLIENT_ID").is_err() {
            assert!(tidal_device_client().is_none());
        }
    }

    #[test]
    fn state_carries_the_nonce_and_the_callback() {
        let st = return_state("n0nce", "http://127.0.0.1:7788/tidal/callback");
        assert_eq!(state_nonce(&st), "n0nce");
        use base64::Engine;
        let back = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(st.split_once('.').unwrap().1)
            .unwrap();
        assert_eq!(back, b"http://127.0.0.1:7788/tidal/callback");
        assert_eq!(state_nonce("plain"), "plain");
    }
}
