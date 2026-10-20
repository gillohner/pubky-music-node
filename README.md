# Pubky Music Node

A Pubky node that bundles a Pubky homeserver with a music core delivering Roon-level audio
quality: it serves the household's library to phones, desktop, web and TV, drives DACs and
streamers bit-perfect, and keeps the social layer (tags, playlists, follows, listening activity)
on Pubky, indexed by Pinakes.

The implementor handoff is the plan; `docs/PINAKES-CONTEXT.md` is its §12 and overrides §8 where
they conflict. Decisions live in `docs/adr/`.

## Workspace

| Crate | Handoff §5.1 | Status |
| :-- | :-- | :-- |
| `pm-model` | shared types: formats, caps, signal path, canonical IDs, library entities | M0 done |
| `pm-zones` | `Output` / `AudioSink` traits, registry, null and WAV outputs; zone engine (queue, transport, gapless, position, events) | M0 done; zone engine done (single output; groups at M2) |
| `pm-audio` | decode (Symphonia, MD5-verified), DSF/DFF readers, DSP pipeline with signal path | M1 done |
| `pm-library` | scanning, tag reading, SQLite (FTS5), MusicBrainz matching | M1 done (AcoustID/fuzzy later) |
| `pm-pubky` | music objects as v1 `PubkyAppPost` envelopes (private folder by default, ADR 0006), Pinakes and Nexus clients, discovery, homeserver I/O | M9/M10 groundwork (read paths verified live) |
| `pm-out-local` | ALSA `hw:` output (feature `alsa`, needs libasound headers) | M1 hardware test done (FiiO KA17, loopback byte-identical); native DSD silent on KA17 |
| `pm-api` | native `/v1` HTTP + WebSocket API (`docs/api.md`) | M3 server side done |
| `pm-client` | Rust client for `/v1` (HTTP + event stream) | done |
| `pm-app` | native controller and player (egui): Roon-style browse, queue, transport, signal path; plays locally through its own DAC | first cut, needs a desktop to run |
| `pm-node` | the binary: CLI plus `serve` (API, one zone per output) | CLI and daemon |

Deployment: directly on hardware, Docker, or as an Umbrel app (`docs/deploy.md`, `deploy/`).

Not started: `pm-sources`, `pm-out-oaat`, `pm-out-openhome`, `pm-out-slimproto`, `pm-out-airplay`,
`pm-out-cast`, `pm-api`, `pm-remote`, `pm-endpoint`, `sidecars/roon-bridge`, `clients/`.

## Build

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
# with a local `pubky-testnet` running: playlist sync round trip through the private folder
PM_TESTNET=1 cargo test -p pm-api --test testnet_sync
# on the node, with ALSA headers installed:
cargo build -p pm-node --features alsa
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
pm-node resolve https://open.spotify.com/track/<id>   # MusicBrainz URL relations
pm-node uri https://tidal.com/browse/track/251380836  # canonical form (ADR 0001)
pm-node discover trends                     # live Pinakes facets and music cards
pm-node -c node.toml serve                  # API + zones; then: cargo run -p pm-app (PM_NODE, PM_TOKEN)
```

## Upstream etiquette

Every outbound request carries `pubky-music-node/<version> (<contact>)`; the node refuses to
start network commands without a `contact` (email or URL, never a code-host link). MusicBrainz
is called at most once per second; for real catalogue use run a local MusicBrainz replica on the
node (later milestone). Pinakes allows 120 requests per minute per IP, so batch through
`POST /v1/resources/lookup`. The Pinakes owner's answers are in `docs/PINAKES-ANSWERS.md`.

## Hard constraints (handoff §2)

No Roon protocol cloning. No copyrighted audio under a public `/pub/` path. One streaming account
per user. Canonical IDs (MusicBrainz / service URLs, ADR 0001) for all social data. Bit-perfect
by default with every conversion in the signal path. Reverse-engineered interop goes through
legal review.
