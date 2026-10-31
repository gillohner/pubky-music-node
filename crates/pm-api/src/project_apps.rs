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
//! No secret is built in. In particular there is no built-in TIDAL device client (owner,
//! 2026-10-31: the one python-tidal publishes was issued by TIDAL to someone else): full tracks
//! need a device client the owner enters (Sources › TIDAL › Sign in › Your own app, or
//! `[tidal] unofficial_client_id` / `_secret`).

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

/// This build carries the project's TheAudioDB key (not only the public test key).
pub fn audiodb_built_in() -> bool {
    option_env!("PM_PROJECT_AUDIODB_KEY").is_some_and(|v| !v.trim().is_empty())
}

/// The project's Google Drive app (client id).
pub fn google_client_id() -> Option<&'static str> {
    option_env!("PM_PROJECT_GOOGLE_CLIENT_ID").filter(|v| !v.trim().is_empty())
}

/// The project's OneDrive app (a public client id: no secret).
pub fn onedrive_client_id() -> Option<&'static str> {
    option_env!("PM_PROJECT_ONEDRIVE_CLIENT_ID").filter(|v| !v.trim().is_empty())
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

    /// No TIDAL device client is built in (owner, 2026-10-31): neither the build script, the
    /// release workflow nor the image takes one, and a node with no client of the owner's has
    /// none, so TIDAL plays the official way until the owner enters one.
    #[test]
    fn no_tidal_device_client_is_built_in() {
        for (name, text) in [
            ("build.rs", include_str!("../build.rs")),
            (
                "release.yml",
                include_str!("../../../.github/workflows/release.yml"),
            ),
            ("Dockerfile", include_str!("../../../Dockerfile")),
        ] {
            assert!(!text.contains("TIDAL_DEVICE"), "{name} feeds in a device client");
        }
        if std::env::var("PM_TIDAL_UNOFFICIAL_CLIENT_ID").is_err() {
            let lib = pm_library::Library::open_in_memory().unwrap();
            assert!(crate::tidal_unofficial::client_of(&lib).is_none());
        }
    }
}
