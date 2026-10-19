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
| `pm-zones` | `Output` / `AudioSink` traits, registry, null and WAV outputs | M0 done |
| `pm-audio` | decode (Symphonia, MD5-verified), DSF/DFF readers, DSP pipeline with signal path | M1 software path done; hardware test pending |
| `pm-library` | scanning, tag reading, SQLite (FTS5), MusicBrainz matching | M1 done (AcoustID/fuzzy later) |
| `pm-pubky` | `pubky.music` schemas, Pinakes and Nexus clients, discovery, homeserver I/O | M9/M10 groundwork (read paths verified live) |
| `pm-out-local` | ALSA `hw:` output (feature `alsa`, needs libasound headers) | draft, untested on hardware |
| `pm-node` | the binary: CLI over the services (scan, play, resolve, discover); API at M3 | CLI done |

Not started: `pm-sources`, `pm-out-oaat`, `pm-out-openhome`, `pm-out-slimproto`, `pm-out-airplay`,
`pm-out-cast`, `pm-api`, `pm-remote`, `pm-endpoint`, `sidecars/roon-bridge`, `clients/`.

## Build

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
# on the node, with ALSA headers installed:
cargo build -p pm-node --features pm-out-local/alsa
```

Builds are non-incremental with line-table debug info (`.cargo/config.toml`) to keep the shared
disk small.

## Try it

```sh
cargo build -p pm-node
pm-node info some.flac                      # container, format, duration
pm-node play some.flac --to wav:/tmp/o.wav  # bit-perfect decode, prints the signal path
pm-node play some.flac --to null --volume-db=-6   # shows the float path and dither stages
pm-node -c node.toml scan /music            # SQLite library with FTS5 search
pm-node -c node.toml search "daft punk"
pm-node resolve https://open.spotify.com/track/<id>   # MusicBrainz URL relations
pm-node uri https://tidal.com/browse/track/251380836  # canonical form (ADR 0001)
pm-node discover trends                     # live Pinakes facets and music cards
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
