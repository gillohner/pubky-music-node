# Releasing

`.github/workflows/release.yml` builds everything; nobody builds releases by hand.

| Trigger | Result |
| :-- | :-- |
| push to `main` | the `edge` pre-release is replaced (tag moved to the commit), image `:edge` |
| tag `v1.2.3` | release `v1.2.3` (marked latest), images `:1.2.3`, `:1.2`, `:latest` |
| manual run (Actions → release → Run workflow, any branch) | everything built as workflow artifacts, nothing published |

A release run that is still building finishes and publishes; newer pushes queue behind it, so
`edge` can lag a busy `main` by one build.

## Cutting a release

1. Bump `version` under `[workspace.package]` in `Cargo.toml` (the app's bundles take it from
   there too) and move the CHANGELOG entries under a heading for the version.
2. Merge to `main`, then tag that commit: `git tag -s v0.2.0 -m v0.2.0 && git push origin v0.2.0`.
   The workflow refuses a tag that does not match the workspace version.

## What is built

| Artifact | Runner | Notes |
| :-- | :-- | :-- |
| `pubky-music-macos-universal.dmg` | macos-15 | `tauri build --target universal-apple-darwin`; unsigned |
| `pubky-music-windows-x86_64.msi`, `-setup.exe` | windows-2025 | WiX and NSIS; unsigned |
| `pubky-music-linux-{x86_64,arm64}.{AppImage,deb}` | ubuntu-22.04(-arm) | glibc 2.35, so Debian 12 and Ubuntu 22.04 run them |
| `pubky-music-android-{arm64,universal}.apk`, `.aab` | ubuntu-24.04 | arm64-v8a, armeabi-v7a, x86_64; signed (below) |
| `pubky-music-node-linux-{x86_64,arm64}.tar.gz` | ubuntu-22.04(-arm) | `pm-node`, `pm-endpoint` with ALSA (x86_64 adds the egui `pm-app`) |
| `pubky-music-node-linux-armv7.tar.gz` | ubuntu-24.04 | cross-built in Debian bookworm (`tools/release/build-armv7.sh`), glibc 2.36: Raspberry Pi OS bookworm |
| `pubky-music-node-{windows-x86_64.zip,macos-arm64.tar.gz}` | windows-2025, macos-15 | node, endpoint and `pm-app` |
| `ghcr.io/gillohner/pubky-music-node` | ubuntu-24.04 | amd64 and arm64 (the Dockerfile cross-compiles) |

The app's Android project (`crates/pm-desktop/gen/android`) is generated on every build by
`tauri android init` and adjusted by `tools/release/android-patch.py` (cleartext HTTP to LAN
nodes, the TLS verifier's Kotlin classes). Nothing in `gen/` is committed.

## Signing

- **Android:** a release key made for this project (PKCS12, alias `pubky-music`, RSA 4096,
  valid 10000 days; certificate SHA-256
  `57:79:13:65:F9:A2:D9:C7:79:C6:9C:DB:F8:BE:E3:98:4F:01:7E:0A:47:9F:E3:FA:D5:AE:0D:2C:8B:B4:C2:64`).
  It lives in the repo secrets `ANDROID_KEYSTORE_B64`, `ANDROID_KEYSTORE_PASSWORD`,
  `ANDROID_KEY_ALIAS` and `ANDROID_KEY_PASSWORD`; the steward holds the offline backup. Losing it
  means users must uninstall to take updates signed with a new key. `tools/release/android-sign.sh`
  zip-aligns (16 KB pages), signs with `apksigner` and `jarsigner`; without the secrets (forks)
  it publishes `-unsigned` files.
- **macOS and Windows:** unsigned until the owner provides an Apple Developer ID (signing and
  notarization) and a Windows code-signing certificate. Tauri reads them from
  `APPLE_CERTIFICATE`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID` and
  the Windows `certificateThumbprint` or a sign command; they get added to the app job then.
- **Updater:** not yet. Tauri's updater needs its own signing key and a `latest.json` per
  release; it comes with the signed desktop builds.

## Android version codes

`(major·1 000 000 + minor·1 000 + patch) · 1000`, plus the run number for edge builds and 999 for
releases, so a release installs over every edge build of the same version.
