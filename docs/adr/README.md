# Architecture decision records

One file per decision, numbered. Handoff §11 asks for open questions to be resolved here.

| # | Decision |
| :-- | :-- |
| [0001](0001-canonical-music-uris.md) | Canonical music URIs are MusicBrainz / service URLs, not a `music:` scheme |
| [0002](0002-sqlite-library-store.md) | SQLite for the node's library and local state |
| [0003](0003-decoding-and-dsd.md) | Symphonia for PCM containers, own readers for DSF/DFF |
| [0004](0004-sample-representation.md) | One in-memory sample layout for bit-perfect playback |
| [0005](0005-zone-engine.md) | Zone engine: one audio thread, gapless by format, pause as flush-and-seek |
| [0006](0006-social-objects-as-v1-posts.md) | Social objects are v1 `PubkyAppPost`s with a JSON envelope, in a private folder by default |
| [0007](0007-endpoint-protocol.md) | Own endpoint protocol (`pm-stream`): endpoint-paced push over TCP, mDNS discovery |
| [0008](0008-remote-access-position.md) | Remote access step one: the node publishes its address privately on the homeserver; Noise transport deferred |

Open (handoff §11), not yet decided: live Pubky transport / relay for M5; app namespace name (`pubky.music` is a
placeholder constant); homeserver private-data support and quotas; relay design; streaming-service
terms; client UI tech; legal review of RE-derived work.
