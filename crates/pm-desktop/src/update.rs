//! App updates on desktops (Tauri updater). A release build checks the feed
//! of the channel it was built for (`PM_UPDATE_CHANNEL` at build time: `edge`
//! for builds of main, `latest` for tagged releases) once at start. When a
//! newer version is there it asks; on yes it installs and restarts.
//!
//! The feed (`latest.json`, written by the release workflow) lists a signed
//! bundle per platform; the app installs only bundles signed with the
//! project's key, and only for the version the signature names
//! (`tauri.{linux,macos,windows}.conf.json`, `plugins.updater`: kept out of
//! the shared config, which Android builds read too). Linux updates
//! itself only as an AppImage: a .deb is the package manager's to update.
//! Local builds have no channel and never check.

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;

const RELEASES: &str = "https://github.com/gillohner/pubky-music-node/releases";

/// The channel this build follows, if any.
pub fn channel() -> Option<&'static str> {
    option_env!("PM_UPDATE_CHANNEL").filter(|c| !c.is_empty())
}

/// Where a channel's feed is. `latest` is GitHub's latest release, never a pre-release.
pub fn feed(channel: &str) -> String {
    match channel {
        "edge" => format!("{RELEASES}/download/edge/latest.json"),
        _ => format!("{RELEASES}/latest/download/latest.json"),
    }
}

/// Check once in the background; ask before installing anything.
pub fn spawn(app: &AppHandle) {
    let Some(channel) = channel() else {
        tracing::info!("updates: off (a local build)");
        return;
    };
    #[cfg(target_os = "linux")]
    if std::env::var_os("APPIMAGE").is_none() {
        tracing::info!("updates: through the package manager (not an AppImage)");
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = check(&app, channel).await {
            tracing::warn!(channel, "update check failed: {e}");
        }
    });
}

async fn check(app: &AppHandle, channel: &str) -> anyhow::Result<()> {
    let updater = app
        .updater_builder()
        .endpoints(vec![feed(channel).parse()?])?
        .build()?;
    let Some(update) = updater.check().await? else {
        tracing::info!(channel, "updates: this is the newest version");
        return Ok(());
    };
    tracing::info!(channel, version = %update.version, "updates: a newer version is available");
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .message(format!(
            "Pubky Music {} is available (this is {}).\n\nInstall it now? The app restarts.",
            update.version, update.current_version
        ))
        .title("Update Pubky Music")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Install and restart".into(),
            "Later".into(),
        ))
        .show(move |yes| {
            let _ = tx.send(yes);
        });
    if !rx.await.unwrap_or(false) {
        tracing::info!("updates: later");
        return Ok(());
    }
    update.download_and_install(|_, _| {}, || {}).await?;
    tracing::info!(version = %update.version, "updates: installed; restarting");
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeds() {
        assert_eq!(
            feed("edge"),
            "https://github.com/gillohner/pubky-music-node/releases/download/edge/latest.json"
        );
        assert_eq!(
            feed("latest"),
            "https://github.com/gillohner/pubky-music-node/releases/latest/download/latest.json"
        );
        // A local build (no PM_UPDATE_CHANNEL at build time) never checks.
        if option_env!("PM_UPDATE_CHANNEL").is_none() {
            assert_eq!(channel(), None);
        }
    }
}
