# ADR 0012: One client: the node's web UI in a Tauri 2 shell; the egui app is frozen

Status: accepted, 2026-10-20 (the shell); retiring `pm-app` accepted by the owner 2026-10-21 and done the same day.

## Context

Two controllers grew side by side: the web UI the node serves at `/` and the native egui app
(`pm-app`). Every feature landed twice, the egui copy later and thinner. A wry shell
(`pm-desktop`) already put the web UI in a native window with a built-in player. The owner
wants installable builds for macOS, Windows, Linux and Android from every push, and an updater
later. wry alone gives a window, not bundles, an Android project, signing or an updater.

## Decision

`crates/pm-desktop` is a **Tauri 2** app (the same wry/WebView underneath) and the one client:

* **The UI comes from the node.** The window starts on a small local connect page (`ui/`):
  Pubky Ring sign-in finds the node through the owner's homeserver (ADR 0008), or an address
  and token are typed. After that the window shows the node's own page. A node upgrade upgrades
  the UI, so the UI and the API never drift apart. When the node does not answer, the connect
  page says so and offers "Try again" or "Connect to another node".
* **The shell stays thin.** It holds a handful of commands (connect, reconnect, Ring, forget).
  It hands the token to the node page through `localStorage`, only on the node's origin.
* **Locked down.** Only the local connect page has IPC; the node's page has none. Navigation
  stays on the app's pages and the node's origin. Anything else ("Open on …" links, a streaming
  service's sign-in) opens in the system browser.
* **Desktops are players too.** On macOS, Windows and Linux the app serves the machine's DACs as
  a `pm-endpoint` (bit-perfect ALSA `hw:` on Linux, the system output elsewhere) and registers
  it with the node, as the wry shell did. Android is a controller first; the phone as an
  endpoint is a later phase.
* **One project, every platform.** `tauri build` gives `.dmg`/`.app`, `.msi`/NSIS `.exe`,
  `.AppImage`/`.deb`; `tauri android build` gives the `.apk` and `.aab`. The release workflow
  builds them on every push to `main` (the `edge` pre-release) and on `v*` tags.

**`pm-app` (egui): retired.** The proposal was to freeze it and keep it one more release cycle as
a fallback for machines without a usable WebView. The owner chose to retire it at once
(2026-10-21): it left the workspace, CI and the release, and its code stays in git history. It is
not needed headless: `pm-endpoint` is the headless player, and any browser reaches the web UI on
machines where the app's WebView is missing.

## Consequences

* The app shows nothing useful without a node. That is the product: the node is the library.
* The browser zone in the WebView is not bit-perfect; bit-perfect playback on a desktop goes
  through the built-in player, a zone with the node's signal path.
* Android needs two things outside Rust: cleartext HTTP to LAN nodes (`http://192.168…`), and
  the `rustls-platform-verifier` init and classes for the pkarr relay client the Ring sign-in
  uses. The Android build sets both up.
* The bundle identifier `to.synonym.pubkymusic` becomes the Android package name. It cannot
  change after the first store upload.
* Builds are unsigned until the owner provides an Apple Developer ID and a Windows certificate.
  macOS users see Gatekeeper's warning, Windows users SmartScreen's.
