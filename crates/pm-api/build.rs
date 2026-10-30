// The project's built-in app values come from the build environment (src/project_apps.rs).
//
// The TIDAL device client (STEWARD-TIDAL-DEFAULT.md) is compiled in lightly masked, so the plain
// values sit neither in the source nor as strings in the binary: `PM_TIDAL_DEVICE_CLIENT_ID` and
// `PM_TIDAL_DEVICE_CLIENT_SECRET` become byte arrays in `$OUT_DIR/tidal_device.rs`. Without them
// the build has no built-in device client.
use std::io::Write;

const MASK: &[u8] = b"pubky-music-node:tidal-device";

fn masked(v: &str) -> String {
    v.bytes()
        .enumerate()
        .map(|(i, b)| (b ^ MASK[i % MASK.len()] ^ (i as u8)).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn main() {
    for v in [
        "PM_PROJECT_TIDAL_CLIENT_ID",
        "PM_PROJECT_JAMENDO_CLIENT_ID",
        "PM_PROJECT_ACOUSTID_KEY",
        "PM_PROJECT_FANART_KEY",
        "PM_PROJECT_AUDIODB_KEY",
        "PM_PROJECT_ONEDRIVE_CLIENT_ID",
        "PM_PROJECT_GOOGLE_CLIENT_ID",
        "PM_TIDAL_DEVICE_CLIENT_ID",
        "PM_TIDAL_DEVICE_CLIENT_SECRET",
    ] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    let id = std::env::var("PM_TIDAL_DEVICE_CLIENT_ID").unwrap_or_default();
    let secret = std::env::var("PM_TIDAL_DEVICE_CLIENT_SECRET").unwrap_or_default();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let mut f = std::fs::File::create(out.join("tidal_device.rs")).expect("write tidal_device.rs");
    if id.trim().is_empty() || secret.trim().is_empty() {
        writeln!(f, "pub(super) const DEVICE: Option<(&[u8], &[u8])> = None;").unwrap();
    } else {
        writeln!(
            f,
            "pub(super) const DEVICE: Option<(&[u8], &[u8])> = Some((&[{}], &[{}]));",
            masked(id.trim()),
            masked(secret.trim())
        )
        .unwrap();
    }
}
