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
| [0009](0009-group-sync.md) | Grouped outputs stay in time by sample slip; a `sync` stage keeps the signal path honest |
| [0010](0010-favourites-as-bookmarks.md) | Favourites are v1 `PubkyAppBookmark`s of canonical music URLs, private by default |
| [0011](0011-tags-are-public.md) | Tags are public v1 `PubkyAppTag`s on canonical URLs, written only on explicit action |
| [0012](0012-one-client-tauri-shell.md) | One client: the node's web UI in a Tauri 2 shell for desktops and Android; the egui app is retired (owner, 2026-10-21) |

Open (handoff §11), not yet decided: live Pubky transport / relay for M5; app namespace name (`pubky.music` is a
placeholder constant); homeserver private-data support and quotas; relay design; streaming-service
terms; legal review of RE-derived work.