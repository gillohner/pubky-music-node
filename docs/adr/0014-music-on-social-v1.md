# ADR 0014: Music data on pubky-social-specs v1, with a separate `pubky-music-specs` crate

Status: accepted (2026-10-21, steward plan step 3). Supersedes [0006](0006-social-objects-as-v1-posts.md),
and [0010](0010-favourites-as-bookmarks.md) for the bookmark path.

## Context

ADR 0006 stored playlists, reviews and activity as legacy `PubkyAppPost`s (`kind: long`) whose
`content` was a `{"type":"pubky.music/<kind>","v":1}` envelope under `/priv/pubky.app/posts/`. That was
a stopgap until social primitives existed. `pubky-social-specs` v1 (branch `v1`, 1.0.0-alpha.5) now has
them: a generic post envelope `PostEnvelope<K: PostKind>` that apps specialize in their own namespace,
a `collection` post kind, private bookmarks, and the reserved `ext` member for extensions.

## Decision

Music uses the shared social objects and only adds what is musical.

| Music thing | v1 object | Path |
| :-- | :-- | :-- |
| Tags on a song, album or artist | social `tag` on the canonical music URL | `/pub/pubky.music/tags/` for now, see below |
| Favourites | social `bookmark` (private) | `/priv/social/v1/bookmarks/` |
| Comments, threads | social `post`, `embed` or `parent` = the music URL | `/{pub,priv}/social/v1/posts/` |
| Playlists | social `collection` post, `layout: list`; items `{uri, note?, ext.music.source?}`; the collection's `ext.music.source_order` | `/{pub,priv}/social/v1/posts/` |
| Ratings and reviews | music kind `review`: `{rating: 1–10, text?}`, `embed` = the music URL | `/{pub,priv}/music/v1/posts/` |
| Listening history | music kind `listen`: `{played_at, duration_ms?, source?}`, `embed` = the music URL | `/priv/music/v1/posts/` only |

- **`crates/pubky-music-specs`** owns the two music kinds (`MusicPostKind { Review, Listen }` on
  `PostEnvelope`, namespace `music`) and their content rules, plus readers and writers for the
  `ext.music` hints. It depends only on `pubky-social-specs` and serde, never on node code, so it can
  move to its own repository like `mapky-app-specs` and `eventky-app-specs`. `pubky-social-specs` is a git
  dependency pinned to `acde362` until a release is published.
- **Ratings are 1 to 10**, the same shape as Mapky's review. The UI shows five stars with halves; the
  node's existing 1–5 ratings double on migration.
- **`ext.music` is hostile input** (RFC Part C2, rules 10 and 11): the readers return only a
  well-formed service name and an http(s) or pubky URI that passes the reference gate, and drop
  anything else silently. A bad hint never makes a playlist unreadable. Writers keep every other
  member of `ext` and `ext.music`. The universal reference tier is not used for sources because it
  admits any scheme, and a source is a link people open.
- **A listen is private only**; validation refuses it under `/pub/`.
- **Canonical music URLs** stay ADR 0001: MusicBrainz first, then the ISRC URL, then the service URL.
- **Tags stay at `/pub/pubky.music/tags/`** until the Nexus behind Pinakes indexes `social/v1/tags`
  (question to the steward, 2026-10-21). They then move with the same one-time migration.
- **Migration** reads the node's existing `/priv/pubky.app/posts/` envelopes and `/priv/pubky.app/bookmarks/`
  once and writes the v1 objects. The old files are kept (RFC Part C: a migration never deletes).
- If a shared rating kind lands in `social/v1` later, `review` switches to it.

## Consequences

- A generic Pubky client shows a public playlist as a proper collection, not raw JSON.
- Social readers classify `/music/v1/` as foreign data and skip it; a music-aware indexer reads it.
- The node's sign-in asks for `/pub|priv/social/v1/` and `/pub|priv/music/v1/` as well, so one Pubky
  Ring approval covers it.
- `docs/specs/pubky-music-playlist.md` describes the retired envelope and stays only for reading old
  data during migration.

## Amendment (2026-10-21): playlists have no size cap

The owner's decision: "make collections but remove the limit". pubky-social-specs caps a collection at
100 items and 40,000 code points of content. A playlist is a v1 collection however long it is; the node
caps it at 10,000 songs.
- `v1::validate_social` checks a longer collection as consecutive parts that each fit the spec's caps,
  so every other rule still applies to every item (references, `ext`, the envelope).
- Edits mint their edit id from the first part (it only has to sort above the head).
- Publishing an oversized collection skips `plan_publish` (which re-validates with the cap). Music
  collections carry no private media, so it writes the same leaf under `/pub/`.
- Readers (the node's pull and others' shared playlists) use the same lifted check. Other v1 clients
  that keep the cap may refuse such a collection; the node never does.
- Playlists kept in the old private format before this are written as v1 collections on the next
  sync; the old files stay.
