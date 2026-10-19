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
| `pm-audio` | decode (Symphonia), DSF/DFF readers, DSP pipeline with signal path | M1 in progress |
| `pm-library` | scanning, tag reading, SQLite, MusicBrainz matching | M1 in progress |
| `pm-pubky` | `pubky.music` schemas, Pinakes and Nexus clients, discovery, homeserver I/O | M9/M10 groundwork |
| `pm-out-local` | ALSA `hw:` output (feature `alsa`, needs libasound headers) | draft, untested on hardware |
| `pm-node` | the binary: CLI, config, API | skeleton |

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

## Hard constraints (handoff §2)

No Roon protocol cloning. No copyrighted audio under a public `/pub/` path. One streaming account
per user. Canonical IDs (MusicBrainz / service URLs, ADR 0001) for all social data. Bit-perfect
by default with every conversion in the signal path. Reverse-engineered interop goes through
legal review.
