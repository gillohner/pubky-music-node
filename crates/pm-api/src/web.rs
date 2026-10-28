//! What the web UI may link to, and the Content-Security-Policy its pages are served with
//! (security review 2026-10-27, #7).
//!
//! Links in item pages come from other people's data: a MusicBrainz homepage, a radio station's
//! site, a link someone pasted. A `javascript:` one would run in the node's page, with the owner's
//! token at hand. So every [`Fact`](crate::types::Fact) link passes [`web_link`] on its way out,
//! the page checks again before it renders one, and [`PAGE_CSP`] stops scripts that aren't the
//! node's own from running at all.

use base64::Engine as _;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

/// `u` (trimmed) when it is an http or https URL, as a browser parses it (`url` follows the same
/// WHATWG rules: `java\tscript:` is `javascript:`); `None` for `javascript:`, `data:` and every
/// other scheme. The text is kept as it was, so a stored homepage reads the same.
pub(crate) fn web_link(u: &str) -> Option<String> {
    let u = u.trim();
    let url = url::Url::parse(u).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| u.to_string())
}

/// Serde `serialize_with` for a link field: only [`web_link`]s go out.
pub(crate) fn serialize_link<S: serde::Serializer>(v: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
    v.as_deref().and_then(web_link).serialize(s)
}

/// `Content-Security-Policy` for the node's own pages (`index.html`, `party.html`). Scripts run
/// only from the node and the pages' own inline scripts (by hash), so a `javascript:` link, an
/// injected handler or a `<script>` slipped into a page does nothing; no plugins; `<base>` only to
/// the node. Fetches, images and media stay open: covers and streams come from everywhere, and the
/// desktop app's bridge from its own scheme.
pub(crate) static PAGE_CSP: LazyLock<String> = LazyLock::new(|| {
    policy(&[
        include_str!("../static/index.html"),
        include_str!("../static/party.html"),
    ])
});

fn policy(pages: &[&str]) -> String {
    let mut hashes = String::new();
    for script in pages.iter().flat_map(|p| inline_scripts(p)) {
        let digest = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(script.as_bytes()));
        hashes.push_str(&format!(" 'sha256-{digest}'"));
    }
    format!("script-src 'self'{hashes}; object-src 'none'; base-uri 'self'")
}

/// The text of every `<script>` without attributes in a page.
fn inline_scripts(page: &str) -> impl Iterator<Item = &str> {
    page.split("<script>")
        .skip(1)
        .filter_map(|rest| rest.split_once("</script>").map(|(s, _)| s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Fact;

    #[test]
    fn only_web_links_pass() {
        for ok in [
            "https://example.org/a?b=1",
            "http://192.168.1.10:7788/x",
            " HTTPS://Example.org ",
        ] {
            assert!(web_link(ok).is_some(), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            " javascript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:x",
            "file:///etc/passwd",
            "/relative/path",
            "",
        ] {
            assert_eq!(web_link(bad), None, "{bad}");
        }
    }

    /// Every Fact link goes through the one check, however the Fact was built.
    #[test]
    fn a_fact_never_carries_a_script_link() {
        let fact = |href: &str| Fact {
            label: "Homepage".into(),
            value: href.into(),
            href: Some(href.into()),
        };
        let out = serde_json::to_value(fact("javascript:alert(document.cookie)")).unwrap();
        assert!(out["href"].is_null(), "{out}");
        let out = serde_json::to_value(fact("https://band.example/")).unwrap();
        assert_eq!(out["href"], "https://band.example/");
    }

    /// The pages are served with the policy, and its hashes are those of the pages' inline
    /// scripts as they are now (an edit to one changes its hash here by itself).
    #[tokio::test]
    async fn the_pages_forbid_scripts_that_are_not_the_nodes() {
        for (page, html) in [
            (crate::web_ui().await, include_str!("../static/index.html")),
            (crate::party_page().await, include_str!("../static/party.html")),
        ] {
            let csp = page
                .headers()
                .get(axum::http::header::CONTENT_SECURITY_POLICY)
                .expect("the page has a Content-Security-Policy")
                .to_str()
                .unwrap()
                .to_string();
            assert!(csp.starts_with("script-src 'self' 'sha256-"), "{csp}");
            assert!(!csp.contains("unsafe-inline"), "{csp}");
            assert!(csp.contains("object-src 'none'"), "{csp}");
            let scripts: Vec<_> = inline_scripts(html).collect();
            assert!(!scripts.is_empty());
            for s in scripts {
                let h = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(s.as_bytes()));
                assert!(csp.contains(&format!("'sha256-{h}'")), "{csp}");
            }
        }
    }

    /// Inline event handlers would need `'unsafe-inline'` (or hashes): the page uses none.
    #[test]
    fn the_pages_have_no_inline_handlers() {
        for (name, src) in [
            ("app.js", include_str!("../static/app.js")),
            ("index.html", include_str!("../static/index.html")),
            ("party.html", include_str!("../static/party.html")),
        ] {
            for attr in [
                " onerror=",
                " onload=",
                " onclick=",
                " onchange=",
                " oninput=",
                " onsubmit=",
            ] {
                assert!(!src.contains(attr), "{name} has an inline{attr} handler");
            }
        }
    }
}
