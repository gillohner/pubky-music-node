# Pubky Music Node

Your own music server for the whole home. It runs on a computer that is always on: a NAS, a mini
PC, a Raspberry Pi or an Umbrel box. It reads your music folder and plays it on the speakers you
already have. That includes network streamers, Chromecast, AirPlay, Sonos, a DAC on a Raspberry
Pi, and your computer or phone. You control it from a web page, the Pubky Music app, or Subsonic
apps on your phone. Playlists, favourites and tags can be kept in your own Pubky account, so
they follow you to a new node.

It is for people who are comfortable installing an app on a NAS or Umbrel and following a few
steps. You don't need to be a developer.

## What you need

- **A computer that stays on**, on your home network: a NAS, an Umbrel box, a mini PC, a
  Raspberry Pi 4 or 5 (64-bit), or any Linux machine. Docker makes it easiest.
- **Your music as files** (FLAC, ALAC, WAV, AIFF, DSD, MP3, AAC, Ogg) in a folder that computer
  can read: a local disk, a USB drive, or a network share mounted on it.
- **Something to play on**: see [Speakers](docs/guides/speakers.md). A web browser works too.
- Optional: the **Pubky Ring** app on your phone, to sign in with your Pubky account and to reach
  the node away from home.

## Choose how to run it

| You have | Use | Guide |
| :-- | :-- | :-- |
| An Umbrel box | the Umbrel community app store | [Install on Umbrel](docs/guides/install-umbrel.md) |
| A NAS or server with Docker (Synology, QNAP, Unraid, TrueNAS, any Linux) | the Docker image | [Install with Docker](docs/guides/install-docker.md) |
| A Raspberry Pi or Linux machine without Docker | the release download and a system service | [Install on Linux without Docker](docs/guides/install-linux.md) |
| A Mac, Windows PC, Linux desktop or Android phone | the Pubky Music app, to control a node and play on that device | [The desktop and Android app](docs/guides/desktop-app.md) |
| A Raspberry Pi with a DAC, as a speaker for your node | squeezelite | [Speakers](docs/guides/speakers.md#a-raspberry-pi-with-a-dac-squeezelite) |

The app and squeezelite don't run a node themselves: install the node with one of the first
three, then connect them to it.

## First steps after installing

1. **Open the web page.** In a browser on the same network, go to `http://<node address>:7788`
   (for example `http://192.168.1.10:7788`). On Umbrel, open the app from the home screen.
2. **Enter the access token.** Click **Or paste the API token**, paste it and press **Connect**.
   Where the token is:
   - Docker: `docker exec pubky-music cat /data/api-token` (unless you set `PM_TOKEN`).
   - Umbrel: the password umbrelOS shows for the app.
   - Linux without Docker: the token the install printed, also in `/etc/pubky-music/node.toml`.

   The browser remembers it. Sign-in with Pubky Ring works once the node itself is signed in
   (step 5).
3. **Add your music.** In **Settings › Library folders**, type the music folder's path and press
   **Add folder** (Home's **Get started** card leads there too). With Docker and Umbrel the path
   is `/music`. The node reads the folder, then keeps watching it: new files show up about 15
   seconds after they land.
4. **Pick a speaker.** Speakers on your network show up by themselves under **Settings › Zones and
   outputs**. Choose one with the zone button at the bottom right of the player bar (it says
   **This browser** at first, which plays in the browser you are using), then press play. If a
   speaker is missing, see [Speakers](docs/guides/speakers.md).
5. **Optional:**
   - Sign the node in to Pubky in **Settings › My accounts › Sign in with Pubky**, using the Pubky
     Ring app. This keeps your playlists, favourites and tags in your Pubky account, and lets
     the Pubky Music app find the node from anywhere.
   - Add an email address or website in **Settings › Discover**. MusicBrainz, Wikipedia and the
     Pubky music index ask every app for a way to reach whoever runs it. This also turns on
     Discover.
   - Connect streaming services under **Sources** (TIDAL, Spotify, Internet Archive, Audius and
     more) and internet radio under **Radio**.

## Away from home

The Pubky Music app reaches your node from anywhere once the node is signed in to Pubky. Nothing
to open on your router. For a browser or Subsonic apps away from home, use Tailscale or a
Cloudflare Tunnel. See [Remote access](docs/guides/remote-access.md).

## Updating and backups

- **Update:** Docker: pull the new image and recreate the container. Umbrel: update from the App
  Store. The desktop app asks when a new version is out (a `.deb` is updated by installing the new one).
- **Back up:** **Settings › Backup** makes one file with your library, playlists, favourites,
  history and settings (never your music files), and can do it every day or week to a folder.

Details: [Backups and updates](docs/guides/backup-and-update.md).

## Downloads

| | Latest release | Edge (newest code, every change to `main`) |
| :-- | :-- | :-- |
| All files | [releases/latest](https://github.com/gillohner/pubky-music-node/releases/latest) | [releases/tag/edge](https://github.com/gillohner/pubky-music-node/releases/tag/edge) |
| macOS (universal) | [.dmg](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-macos-universal.dmg) | [.dmg](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-macos-universal.dmg) |
| Windows | [setup .exe](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-windows-x86_64-setup.exe) | [setup .exe](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-windows-x86_64-setup.exe) |
| Linux x86_64 | [.AppImage](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-linux-x86_64.AppImage), [.deb](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-linux-x86_64.deb) | [.AppImage](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-linux-x86_64.AppImage), [.deb](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-linux-x86_64.deb) |
| Linux arm64 | [.AppImage](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-linux-arm64.AppImage), [.deb](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-linux-arm64.deb) | [.AppImage](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-linux-arm64.AppImage), [.deb](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-linux-arm64.deb) |
| Android and Google TV | [arm64 .apk](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-android-arm64.apk) | [arm64 .apk](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-android-arm64.apk) |
| The node, without Docker | `pubky-music-node-linux-{x86_64,arm64,armv7}.tar.gz`, `…-windows-x86_64.zip`, `…-macos-arm64.tar.gz` | same names under edge |
| Docker image | `ghcr.io/gillohner/pubky-music-node:latest` (amd64, arm64) | `…:edge` |
| Umbrel | community app store [gillohner/pubky-music-umbrel](https://github.com/gillohner/pubky-music-umbrel) | |

Every release has a `SHA256SUMS` file to check downloads against.

## Help

- Something not working: [Troubleshooting](docs/guides/troubleshooting.md).
- Questions and bug reports: [GitHub issues](https://github.com/gillohner/pubky-music-node/issues).
  Include how you installed it, the version (**Settings › About and updates**) and the node's log
  (`docker logs pubky-music`, or `journalctl -u pubky-music-node`).

## What it does

- **Library:** scans folders, reads tags (MusicBrainz ids, ReplayGain, embedded art, lyrics),
  watches for new files, fetches covers and artist pages, full-text search, play history.
- **Playback:** bit-perfect by default, with every conversion shown in the signal path. Gapless
  zones, grouped zones kept in sync, per-zone EQ and other DSP, queues that survive restarts,
  internet radio.
- **Sources:** your files, internet radio, the Internet Archive, Audius and Jamendo without an
  account, TIDAL and Spotify with yours, and more. [Streaming services](docs/STREAMING-PROVIDERS.md).
- **Controls:** the web page (works on phones; add it to the home screen), the Pubky Music app,
  any OpenSubsonic app (Symfonium, Feishin, Amperfy: user `pubky`, the token as password), and
  Home Assistant ([guide](docs/home-assistant.md)).
- **People:** invite others to play your music with their own Pubky sign-in, or start a party
  link for guests (**Settings › People**).
- **Pubky:** playlists, favourites, tags and listening history kept in your Pubky account; Discover
  shows what people on Pubky play.

## For developers

- Building and running from source, every config option and all deployment shapes (including the
  bundle with its own Pubky homeserver): [docs/deploy.md](docs/deploy.md).
- HTTP and WebSocket API: [docs/api.md](docs/api.md). Connection and discovery details:
  [docs/CONNECTION.md](docs/CONNECTION.md). Zones and outputs: [docs/ZONES.md](docs/ZONES.md).
- Design decisions: [docs/adr/](docs/adr/). Status by milestone: [docs/STATUS.md](docs/STATUS.md).
  What changed when: [docs/CHANGELOG.md](docs/CHANGELOG.md). Releases:
  [docs/RELEASING.md](docs/RELEASING.md).
- The app: [crates/pm-desktop/README.md](crates/pm-desktop/README.md).

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p pm-node --features alsa   # needs libasound2-dev
```

The workspace is Rust: `pm-node` is the binary, `pm-api` the HTTP API and web UI, `pm-library`
the library, `pm-zones` and `pm-audio` playback, `pm-out-*` the speaker protocols, `pm-pubky` the
Pubky side, `pm-desktop` the app (outside the workspace).

Every outbound request names the node and its contact; MusicBrainz is called at most once per
second, and Pinakes lookups are batched. No copyrighted audio is ever written under a public
`/pub/` path.
