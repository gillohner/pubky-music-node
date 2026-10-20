# pm-desktop: the Pubky Music app

The node's web UI in a native window, built with Tauri 2 for macOS, Windows, Linux and Android
(ADR 0012). It starts on a connect page (`ui/`): Pubky Ring finds the node through your
homeserver, or you type an address and token. On desktops it is also a player: this machine's
DACs become zones on the node.

It is outside the Cargo workspace because Tauri needs WebKitGTK on Linux, which the node's build
machines do not have.

## Develop

Linux needs the WebKitGTK headers:

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
  libjavascriptcoregtk-4.1-dev librsvg2-dev libasound2-dev
cd crates/pm-desktop
cargo run                                  # debug window, connect page
PM_NODE=http://nas:7788 PM_TOKEN=… cargo run   # skip the connect page
cargo clippy --all-targets -- -D warnings && cargo test
```

Settings live in the app config directory (`~/.config/to.synonym.pubkymusic/node.json` on
Linux). The wry shell's `~/.config/pubky-music/desktop.json` is read once if present.

## Bundles

The release workflow builds them; locally (slow, release mode):

```sh
npx @tauri-apps/cli@2.12.1 build --bundles deb       # or appimage, dmg, msi, nsis
npx @tauri-apps/cli@2.12.1 android build --apk       # needs the Android SDK and NDK
```

Icons in `icons/` come from `crates/pm-api/static/icon.svg`
(`npx @tauri-apps/cli@2.12.1 icon ../pm-api/static/icon.svg`).
