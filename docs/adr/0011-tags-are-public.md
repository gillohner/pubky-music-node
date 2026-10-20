# ADR 0011: Tags are public v1 `PubkyAppTag`s, written only on explicit action

Status: accepted, 2026-10-20

## Context

Pinakes indexes pubky.app tags; its music discovery (trends, by tag, friends) is built on them.
The goal keeps new social objects private until the primitives are defined, but tags are an
existing v1 primitive and only make sense in public: a private tag feeds nobody's discovery.

## Decision

A tag the user adds in the node is a `PubkyAppTag { uri, label, created_at }` on the object's
canonical music URL (ADR 0001), id = the spec's hash of `uri:label`, written to the spec's own path
`/pub/pubky.app/tags/<id>` by the sync, exactly as pubky.app would. The library keeps the working
copy (`tags` table) with the same push/tombstone/pull bookkeeping as favourites; the node never tags
anything by itself (no auto-tagging from genres or plays). Labels are trimmed and lowercased and
validated with the spec's rules before storage.

## Consequences

* Tags appear in Pinakes once its indexer sees the homeserver write, and come back through
  Discover by tag; the node shows the user's own tags immediately from the local table.
* Objects without a MusicBrainz identity cannot be tagged (constraint 4), with a 422 that says so.
* Removing a tag deletes the public object; until the delete is pushed the tag is hidden locally.
