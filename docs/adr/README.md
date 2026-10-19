# Architecture decision records

One file per decision, numbered. Handoff §11 asks for open questions to be resolved here.

| # | Decision |
| :-- | :-- |
| [0001](0001-canonical-music-uris.md) | Canonical music URIs are MusicBrainz / service URLs, not a `music:` scheme |
| [0002](0002-sqlite-library-store.md) | SQLite for the node's library and local state |
| [0003](0003-decoding-and-dsd.md) | Symphonia for PCM containers, own readers for DSF/DFF |
| [0004](0004-sample-representation.md) | One in-memory sample layout for bit-perfect playback |
| [0005](0005-zone-engine.md) | Zone engine: one audio thread, gapless by format, pause as flush-and-seek |

Open (handoff §11), not yet decided: OAAT adopt/fork; app namespace name (`pubky.music` is a
placeholder constant); homeserver private-data support and quotas; relay design; streaming-service
terms; client UI tech; legal review of RE-derived work.
