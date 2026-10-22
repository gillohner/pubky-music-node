// The project's built-in app values come from the build environment (src/project_apps.rs).
fn main() {
    for v in [
        "PM_PROJECT_TIDAL_CLIENT_ID",
        "PM_PROJECT_JAMENDO_CLIENT_ID",
        "PM_PROJECT_ACOUSTID_KEY",
    ] {
        println!("cargo:rerun-if-env-changed={v}");
    }
}
