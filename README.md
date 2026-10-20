# Pubky Music Node

A self-hosted music server and player built on Pubky: the household's library served to desktop,
web, phones and streamers with Roon-class care for the audio path, and the social layer (tags,
playlists, favourites, reviews, listening activity) kept on your Pubky homeserver as v1
`pubky-app-specs` objects, indexed by Pinakes.

The implementor handoff is the plan; `docs/PINAKES-CONTEXT.md` is its §12 and overrides §8 where
they conflict. Decisions live in `docs/adr/`; milestone status in `docs/STATUS.md`; the HTTP API in
`docs/api.md`; deployment (bare metal, Docker, Umbrel) in `docs/deploy.md`. What changed when is in
`docs/CHANGELOG.md`.

## What it does today

**Library.** Scans folders (local disks, network shares, anything mounted), reads tags including
MusicBrainz ids, ISRC, ReplayGain, embedded art and genres, keeps everything in SQLite with
full-text search, watches folders for changes, and can add or remove folders from the UI. Covers
come from the files, a `cover.jpg`, or the Cover Art Archive; artist pages get a portrait and a
biography from Wikidata and Wikipedia. Play history, Home (recently added and played, most played),
favourites, genres, new releases from your artists (MusicBrainz, for artists tagged with an MBID), and search
across tracks, albums, artists and genres. Lyrics come from the
files' tags or a sidecar `.lrc` and follow the position on Now Playing when they are timed.

**Playback.** FLAC, ALAC, WAV, AIFF, DSF/DFF (native DSD or DoP), MP3, AAC and Vorbis. Bit-perfect
by default: every conversion is a named stage with a quality level in the signal path. Zones with
gapless playback, queues that survive restarts (shuffle what comes next, save as a playlist, transfer
to another zone, library radio when they run out, a sleep timer), grouping with drift correction, and a per-zone DSP
chain (parametric EQ, convolution, crossfeed, ReplayGain, headroom, volume, dither, resampling only
when an output needs it). Internet radio by URL with ICY titles, a radio-browser.info directory and
saved stations, and two account-free streaming sources behind one `StreamingService` interface: the
Internet Archive's audio collections (live recordings, netlabels, public domain) and Audius
(independent artists). TIDAL plugs in through its official API: catalogue search and albums with the
node's developer credentials, one user signed in with OAuth for playback (stream manifests over HLS,
FLAC where the subscription allows) and for the collection and playlists, which import as favourites
and Pubky playlists by ISRC. Other subscription services plug in at the same point.

**Outputs.** Local ALSA devices on the node, `pm-endpoint` daemons on other machines (own protocol,
ADR 0007), UPnP/OpenHome renderers, Squeezebox/squeezelite players, Chromecast, and any browser.

**Controllers.** A built-in web UI at `/` (phone-friendly and installable to the home screen, keyboard
shortcuts, media keys, Now Playing with the signal path and lyrics), the Pubky Music app (`pm-desktop`, Tauri 2: that UI in a
native window on macOS, Windows, Linux and Android, connected with Pubky Ring, and on desktops a
player for the computer's DACs), the older egui app (`pm-app`), an
OpenSubsonic server for Symfonium, Feishin, Amperfy and friends (their jukebox mode drives a zone of
your choice), and a `/v1` API with a WebSocket event stream for anything else. Both UIs have a Zones
overview for the whole house, Focus filters by file quality, and new releases from your artists.

**Pubky.** Sign in with Pubky Ring. Playlists, reviews and listening activity are private v1
`PubkyAppPost`s with a JSON envelope (ADR 0006), favourites are private v1 bookmarks (ADR 0010),
tags are public v1 `PubkyAppTag`s written only on your action (ADR 0011); all of it syncs with the
homeserver and comes back on a fresh node. Audio can be uploaded to the homeserver's private
storage, never under `/pub/`, from a track's menu in either UI, listed under Settings, and mirrored
back into a fresh node. Discover reads trending music and tags through Pinakes and shows what
Pubky knows about each album.

**Migration.** `pm-node import-playlist <file>` (also in both UIs) imports M3U/M3U8, CSV and Roon's
Excel export, matching by path, file name, then title and artist, and lists what did not match.
`sidecars/roon-bridge` scrobbles Roon playback and imports Roon playlists through the official
Extension API (not yet run against a Core).

Not there yet: Qobuz, Deezer, Spotify and Apple Music (`docs/STREAMING-PROVIDERS.md` lists what each
needs; the TIDAL module is the template), AirPlay 2, a live Pubky transport for remote access (the node publishes
its address privately on the homeserver instead, ADR 0008). Hardware runs beyond the FiiO KA17 are
pending; see `docs/LAPTOP-TEST-PROMPT.md` and `docs/HARDWARE-TEST-PROMPT.md`.

## Workspace

| Crate | What it is |
| :-- | :-- |
| `pm-model` | shared types: formats, caps, signal path, canonical IDs, library entities |
| `pm-zones` | `Output` / `AudioSink` traits, registry, zone engine, queues, groups, pull streams, null and WAV outputs |
| `pm-audio` | decoding (Symphonia, MD5-verified), DSF/DFF readers, DSP pipeline with signal path, HTTP media sources |
| `pm-library` | scanning, tags, SQLite (FTS5), MusicBrainz matching, playlist file import, history, favourites, tags, genres |
| `pm-pubky` | v1 social objects, homeserver I/O, Pinakes and Nexus clients, discovery |
| `pm-out-local` | ALSA `hw:` output (feature `alsa`, needs libasound headers) |
| `pm-api` | `/v1` HTTP + WebSocket API, OpenSubsonic `/rest`, the web UI, radio, sync, art fetching |
| `pm-client` | Rust client for `/v1` |
| `pm-desktop` | the app: the web UI in a Tauri 2 window for macOS, Windows, Linux and Android, with a built-in player on desktops (outside the workspace; ADR 0012) |
| `pm-app` | native controller and player (egui; frozen, retirement proposed in ADR 0012) |
| `pm-stream`, `pm-endpoint`, `pm-out-net` | the endpoint protocol, daemon and node-side output |
| `pm-out-upnp`, `pm-out-slimproto`, `pm-out-cast` | UPnP/OpenHome, Squeezebox and Chromecast outputs |
| `pm-node` | the binary: CLI plus `serve` |

## Download

| | Latest release | Edge (every push to `main`) |
| :-- | :-- | :-- |
| All files | [releases/latest](https://github.com/gillohner/pubky-music-node/releases/latest) | [releases/tag/edge](https://github.com/gillohner/pubky-music-node/releases/tag/edge) |
| macOS (universal) | [.dmg](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-macos-universal.dmg) | [.dmg](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-macos-universal.dmg) |
| Windows | [setup .exe](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-windows-x86_64-setup.exe) | [setup .exe](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-windows-x86_64-setup.exe) |
| Linux x86_64 | [.AppImage](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-linux-x86_64.AppImage), [.deb](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-linux-x86_64.deb) | [.AppImage](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-linux-x86_64.AppImage), [.deb](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-linux-x86_64.deb) |
| Linux arm64 | [.AppImage](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-linux-arm64.AppImage), [.deb](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-linux-arm64.deb) | [.AppImage](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-linux-arm64.AppImage), [.deb](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-linux-arm64.deb) |
| Android and Google TV | [arm64 .apk](https://github.com/gillohner/pubky-music-node/releases/latest/download/pubky-music-android-arm64.apk) | [arm64 .apk](https://github.com/gillohner/pubky-music-node/releases/download/edge/pubky-music-android-arm64.apk) |
| Node and endpoint | `pubky-music-node-linux-{x86_64,arm64,armv7}.tar.gz` (armv7: 32-bit Raspberry Pi OS), Windows and macOS too | same names under edge |
| Node image | `ghcr.io/gillohner/pubky-music-node:latest` (amd64, arm64) | `…:edge` |

The app (`pm-desktop`) is the node's web UI in a window; on desktops it also plays through the
computer's DACs, bit-perfect through ALSA on Linux. The macOS and Windows builds are not signed
yet: on macOS right-click the app and choose Open the first time; on Windows choose More info,
then Run anyway. Android builds are signed with the project's release key. Every release has a
`SHA256SUMS` file. How releases are made: `docs/RELEASING.md`.

## Build

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
# with a local `pubky-testnet` running: playlist sync round trip through the private folder
PM_TESTNET=1 cargo test -p pm-api --test testnet_sync
# on the node, with ALSA headers installed:
cargo build -p pm-node --features alsa
# headless check of the web UI against a running node (needs `npm install jsdom` somewhere):
node tools/ui-smoke.js <api token>
```

Builds are non-incremental with line-table debug info (`.cargo/config.toml`) to keep the shared
disk small.

## Try it

```sh
cargo build -p pm-node
pm-node info some.flac                      # container, format, duration
pm-node play some.flac --to wav:/tmp/o.wav  # bit-perfect decode, prints the signal path
pm-node play 01.flac 02.flac --to alsa:hw:1,0  # one zone, gapless join when formats match
pm-node play some.flac --to null --volume-db=-6   # shows the float path and dither stages
pm-node -c node.toml scan /music            # SQLite library with FTS5 search
pm-node -c node.toml search "daft punk"
pm-node -c node.toml backup ~/pm-$(date +%F).db   # library snapshot, safe while serving
pm-node -c node.toml restore ~/pm-2026-10-20.db   # with the node stopped
pm-node -c node.toml import-playlist roon-export.xlsx
pm-node resolve https://open.spotify.com/track/<id>   # MusicBrainz URL relations
pm-node uri https://tidal.com/browse/track/251380836  # canonical form (ADR 0001)
pm-node discover trends                     # live Pinakes facets and music cards
pm-node -c node.toml serve                  # API, web UI at /, one zone per output
pm-node -c node.toml login                  # sign in with Pubky Ring (QR), stores the session secret
pm-node -c node.toml upload a.flac b.flac   # to the homeserver's private storage (needs a session)
pm-endpoint --name "Kitchen Pi"            # on another machine; the node finds it over mDNS
cargo run -p pm-app                         # native app (PM_NODE, PM_TOKEN)
```

## Upstream etiquette

Every outbound request carries `pubky-music-node/<version> (<contact>)`; the node refuses to
start network commands without a `contact` (email or URL, never a code-host link), and Discover
stays off until one is set. MusicBrainz is called at most once per second; artist info and cover
art are fetched once per object and cached. Pinakes allows 120 requests per minute per IP, so
batch through `POST /v1/resources/lookup`. The Pinakes owner's answers are in
`docs/PINAKES-ANSWERS.md`.

## Hard constraints (handoff §2)

No Roon protocol cloning. No copyrighted audio under a public `/pub/` path. One streaming account
per user. Canonical IDs (MusicBrainz / service URLs, ADR 0001) for all social data. Bit-perfect
by default with every conversion in the signal path. Reverse-engineered interop goes through
legal review.
