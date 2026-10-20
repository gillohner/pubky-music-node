# ADR 0006: Social objects are v1 `PubkyAppPost`s in a private folder

Status: accepted (2026-10-20).

## Context

The handoff (§8.3) sketched own object types under `/pub/pubky.music/`. The project owner's
direction is to use the v1 pubky-app-specs instead, with `PubkyAppPost` carrying custom JSON
content, and to keep playlists and similar data private until the music-specific social
primitives are defined with the Pinakes owner.

## Decision

- Playlists, reviews and activity records are `PubkyAppPost`s (`kind: long`) whose `content` is a
  JSON envelope `{"type":"pubky.music/<kind>","v":1,...}`. Reviews and activity carry an
  `embed` link to their canonical music URL. Validation is both layers: the spec crate's post
  rules and the envelope's rules (`docs/specs/pubky-music-playlist.md`).
- They are written to `/priv/pubky.app/posts/<timestamp id>` by default. Anything outside
  `/pub/` is private on a homeserver; `/priv/` mirrors the public layout so publishing an object
  is a path prefix change, never a format change.
- Ids are Timestamp IDs everywhere (reviews lose their hash id; "one review per target" becomes a
  reader rule).
- Music tags stay plain public `PubkyAppTag`s on canonical URLs.
- pubky-app-specs is a hard dependency of `pm-pubky` (no feature flag).

## Consequences

- Nothing of ours needs indexing yet; a public music post is an ordinary post that Nexus already
  indexes, recognizable by its content prefix.
- Generic Pubky clients render a public playlist as a long post with raw JSON. Acceptable for
  now; the social primitives will replace the envelope for public objects when they exist.
- The node keeps working copies locally and syncs the private folder when a session exists
  (next step: playlist store in the node and API).
