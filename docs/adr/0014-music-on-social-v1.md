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
| Tags on a song, album or artist | social `tag` on the canonical music URL | `/pub/social/v1/tags/` (amended 2026-10-26) |
| Bookmarks (once "favourites") | social `bookmark` (private) of a music URL or a public post | `/priv/social/v1/bookmarks/` |
| Comments, threads | social `post`, `parent` = the music URL | `/{pub,priv}/social/v1/posts/` |
| Collections (a playlist is one of songs) | social `collection` post with its `layout`; items `{uri, note?, ext.music.source?, ext.music.added_at}` (songs, albums, artists, public posts); the collection's `ext.music.source_order` and, on a duplicate, `ext.music.original` | `/priv/social/v1/posts/`, also `/pub/` while public |
| Ratings and reviews | music kind `review`: `{rating: 1–10, text?}`, `embed` = the music URL | `/{pub,priv}/music/v1/posts/` |
| Listening history | music kind `listen`: `{played_at, duration_ms?, source?}`, `embed` = the music URL | `/{pub,priv}/music/v1/posts/` (amended 2026-10-26, below) |
| Now playing, sessions | music kinds `listening` and `session`, `embed` = the music playing now | `/{pub,priv}/music/v1/posts/` |

- **`crates/pubky-music-specs`** owns the two music kinds (`MusicPostKind { Review, Listen }` on
  `PostEnvelope`, namespace `music`) and their content rules, plus readers and writers for the
  `ext.music` hints. It depends only on `pubky-social-specs` and serde, never on node code, so it can
  move to its own repository like `mapky-app-specs` and `eventky-app-specs`. `pubky-social-specs` is a git
  dependency pinned to `acde362` until a release is published. (Since 2026-10-26 it is the crates.io
  release `=1.0.0-rc.1`, tag `v1.0.0-rc.1`: the same data model.)
- **Ratings are 1 to 10**, the same shape as Mapky's review. The UI shows five stars with halves; the
  node's existing 1–5 ratings double on migration.
- **`ext.music` is hostile input** (RFC Part C2, rules 10 and 11): the readers return only a
  well-formed service name and an http(s) or pubky URI that passes the reference gate, and drop
  anything else silently. A bad hint never makes a playlist unreadable. Writers keep every other
  member of `ext` and `ext.music`. The universal reference tier is not used for sources because it
  admits any scheme, and a source is a link people open.
- ~~**A listen is private only**; validation refuses it under `/pub/`.~~ A listen validates under
  either root since 2026-10-26 (amendment below).
- **Canonical music URLs** stay ADR 0001: MusicBrainz first, then the ISRC URL, then the service URL.
- ~~**Tags stay at `/pub/pubky.music/tags/`** until the Nexus behind Pinakes indexes `social/v1/tags`
  (question to the steward, 2026-10-21). They then move with the same one-time migration.~~ They
  moved on 2026-10-26 (below).
- **Migration** reads the node's existing `/priv/pubky.app/posts/` envelopes and `/priv/pubky.app/bookmarks/`
  once and writes the v1 objects. The old files are kept (RFC Part C: a migration never deletes).
- If a shared rating kind lands in `social/v1` later, `review` switches to it.

## Consequences

- A generic Pubky client shows a public playlist as a proper collection, not raw JSON.
- Social readers classify `/music/v1/` as foreign data and skip it; a music-aware indexer reads it.
- The node's sign-in asks for `/pub|priv/social/v1/` and `/pub|priv/music/v1/` as well, so one Pubky
  Ring approval covers it.
- `docs/archive/specs/pubky-music-playlist.md` describes the retired envelope and stays only for reading old
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

## Amendment (2026-10-26): a listen may be public

The owner's decision: listen history is an option, private or public. `pubky-music-specs` accepts a
`listen` under `/pub/music/v1/posts/` as well as `/priv/`. Every other listen rule stays: `embed`
names the music, no `parent`, no attachments, no lock, `played_at` from 2000 on (in microseconds,
with older millisecond values still read: see the next amendment), `duration_ms` at most a day, and
`source` well formed. A public listen's `source.uri` passes the
reference gate under the public root, so it cannot point at a private object.

- The person chooses where the node writes listens: off (nothing is written), private (the default)
  or public (`listens.history`, Settings > You). Changing it does not move older listens; moving or
  deleting the history is its own request (`docs/api.md`, Listen history).
- Why not tags? A tag's id is a hash of `uri:label`, so a song can carry the tag "listened" once:
  no play time, no repeat plays, and public only. A history needs one listen post per play.
- Other people's public listens are read through Pinakes (not built yet). Nexus has no per-app
  code: it reads music posts like any other post, in every namespace alike.

## Amendment (2026-10-26): collections, tags, times, new kinds (STEWARD-SOCIAL-V1-MUSIC.md Part A)

**Collections.** A playlist is a collection of songs; a collection may also hold albums, artists and
public Pubky posts (`pubky://<key>/pub/…/posts/<id>`, versionless).
- It has a `layout`: `list` (the default), `grid` or `visual`.
- **The public switch keeps the draft model.** Every version is written under `/priv/`. While the
  collection is public, the same version is also published under `/pub/` (`plan_publish`). Making it
  private deletes every public version; the private versions stay.
- **Reading others' collections.** The node reads someone's public collections straight from their
  homeserver, newest first. Posts are filtered by kind before the cap (50 collections, at most 1000 posts
  read), so someone with many notes still shows theirs. Our Nexus fork indexes them later.
- **Saving someone's collection:**
  - *bookmark* it: a social bookmark of its link, which follows the original;
  - *duplicate* it: a collection of your own, which names the original in `ext.music.original`.
- `/v1/collections` and `/v1/playlists` are one resource (docs/api.md).

**Tags** are social v1 tags at `/pub/social/v1/tags/<id>.json`, with the same id as before (the hash of
`uri:label`). Upstream Nexus #1165 indexes a tag there on an external URL as a resource tag, and stops
indexing `/pub/<app>/tags/`. The sync moves the node's older tags once and reads the old folders until
then. ADR 0011 is amended.

**Times are microseconds** (`*_at`), like social v1's `created_at`: `listen.played_at` and
`ext.music.added_at` included.
- Durations keep the `_ms` suffix and milliseconds.
- Readers take an older millisecond time (anything below 3000-01-01 in milliseconds) as one.
- The node writes its public collections again once, with microseconds.

**New kinds:**
- `listening` (now playing): `{started_at, expires_at, name?, duration_ms?, source?}`. A reader
  stops showing it once `expires_at` has passed. It is an ordinary kind: there is no feed-only
  tier, and Nexus has no per-app code (owner decision, 2026-10-27).
- `session`: `{started_at, expires_at?, host: pubky://<key>, name?, source?}`.
- `name` (both, owner decision 2026-10-27): optional, trimmed, 1 to 100 code points. A client that
  publishes one without a name from the person writes the generic one (`"<display name>'s
  listening session"`, `"<display name> is listening"`), and a reader shows the same when it is
  absent. Vectors: `crates/pubky-music-specs/tests/vectors/live-v1.json`.
- Both `embed` the music playing now, never a content `uri`.

**Service names** are hyphenated (`apple-music`, `youtube-music`; `SERVICE_NAMES` lists the shared
spellings). A listen's `source` is where it played: an imported listen (ListenBrainz) has none.

**Follows and profiles.**
- The node reads `/pub/social/v1/profile.json` first, then `/pub/pubky.app/profile.json`.
- It writes the social v1 follow (`/pub/social/v1/follows/<key>.json`), and the older pubky.app follow
  until Nexus indexes v1 follows.
