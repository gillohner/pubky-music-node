//! Every path Pubky Music keeps on a person's homeserver (docs/APP-DATA.md has the formats). A test
//! checks that the doc lists each one.

/// The app's namespace on the homeserver.
pub const APP_NAMESPACE: &str = "pubky.music";
/// The app's private folder.
pub const PRIVATE_DIR: &str = "/priv/pubky.music/";
/// How the person's own devices find their node (ADR 0008).
pub const NODE_RECORD_PATH: &str = "/priv/pubky.music/node.json";
/// The node's settings backup, restored after one Ring sign-in on a new node. Never a sign-in or
/// a token (plain JSON): those travel in the vault or the encrypted backup file.
pub const NODE_SETTINGS_PATH: &str = "/priv/pubky.music/settings.json";
/// The person's own preferences ("You" in Settings: Home layout, quality, appearance).
pub const PREFS_PATH: &str = "/priv/pubky.music/prefs.json";
/// The person's own music files (uploads, copies, imports).
pub const FILES_DIR: &str = "/priv/pubky.music/files/";
/// The sources vault (VAULT.md).
pub const VAULT_DIR: &str = crate::vault::VAULT_DIR;
/// The vault's device list.
pub const VAULT_INDEX_PATH: &str = crate::vault::INDEX_PATH;
/// Written into [`FILES_DIR`] once to find out how the homeserver lays out its files.
pub const FILES_LAYOUT_PROBE: &str = "/priv/pubky.music/files/.pubky-music-layout-check";
/// What the desktop app asks Ring for: read the person's node record.
pub const DESKTOP_RING_CAPS: &str = "/priv/pubky.music/:r";
/// Where the node kept music tags (`PubkyAppTag`s) from 2026-10-21 to 2026-10-26. Tags are social v1
/// tags in `/pub/social/v1/tags/` now; the node only reads this folder until it has moved them out.
pub const TAG_DIR: &str = "/pub/pubky.music/tags/";

/// Every path above, for the doc check.
pub const ALL: &[&str] = &[
    PRIVATE_DIR,
    NODE_RECORD_PATH,
    NODE_SETTINGS_PATH,
    PREFS_PATH,
    FILES_DIR,
    VAULT_DIR,
    VAULT_INDEX_PATH,
    FILES_LAYOUT_PROBE,
    TAG_DIR,
];

#[cfg(test)]
mod tests {
    #[test]
    fn every_path_is_the_apps_and_in_the_doc() {
        let doc = include_str!("../../../docs/APP-DATA.md");
        for p in super::ALL {
            assert!(
                p.starts_with("/priv/pubky.music/") || p.starts_with("/pub/pubky.music/"),
                "{p} is outside the app's namespace"
            );
            assert!(
                doc.contains(&format!("`{p}`")),
                "docs/APP-DATA.md doesn't list {p}"
            );
        }
    }
}
