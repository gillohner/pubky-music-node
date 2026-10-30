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

## Amendment (2026-10-21): the tag path

The node first wrote tags under `/pub/pubky.app/tags/<id>`. Nexus (upstream `nexus-watcher`,
`ExtendedParsedUri::UniversalTag`) indexes a tag on an **external** URL only when it is written in
another app's namespace; under `pubky.app/tags/` it is parsed as a pubky.app tag, which must target
`pubky://` objects, so none of the node's tags reached Nexus or Pinakes. Tags now go to
`/pub/pubky.music/tags/<id>` with the same `PubkyAppTag` body and the same id rule
(`pm_pubky::schemas::TAG_DIR`); pubky-social-specs v1 names no music namespace. The sync moves tags on
external links from the old path (idempotent; tags on `pubky://` objects stay) and reads both until
then. The node's sign-in asks for `/pub/pubky.music/:rw`; a session approved before this needs one
new Pubky Ring sign-in. Bookmarks, posts and settings are private (`/priv/`) and not affected.

## Amendment (2026-10-26): social v1 tags; the paths above are superseded

Superseded for the path and the object by ADR 0014 (amended 2026-10-26). A tag is a social v1 tag
(`pubky-social-specs` `PubkySocialTag`, the same `uri`, `label` and `created_at`, the same id: the hash
of `uri:label`) at `/pub/social/v1/tags/<id>.json`. Upstream Nexus #1165 stops indexing
`/pub/<app>/tags/` and indexes a tag there on an external URL or an app's object as a resource tag. The
sync moves the node's tags from `/pub/pubky.app/tags/` and `/pub/pubky.music/tags/` once (settings
`pubky.tags_moved` and `pubky.tags_social_v1`) and reads them until then. What stays: tags are public,
written only on an explicit action, and the label is trimmed and lowercased.
