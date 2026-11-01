// The project's built-in app values come from the build environment (src/project_apps.rs): public
// client ids and keys only. No TIDAL device client is built in (owner, 2026-10-31): full tracks
// need one the owner enters.

fn main() {
    for v in [
        "PM_PROJECT_TIDAL_CLIENT_ID",
        "PM_PROJECT_JAMENDO_CLIENT_ID",
        "PM_PROJECT_ACOUSTID_KEY",
        "PM_PROJECT_FANART_KEY",
        "PM_PROJECT_AUDIODB_KEY",
        "PM_PROJECT_ONEDRIVE_CLIENT_ID",
        "PM_PROJECT_GOOGLE_CLIENT_ID",
    ] {
        println!("cargo:rerun-if-env-changed={v}");
    }
}
