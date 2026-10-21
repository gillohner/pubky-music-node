# ADR 0010: Favourites are v1 bookmarks of canonical music URLs

Status: accepted, 2026-10-20 Superseded by [ADR 0014](0014-music-on-social-v1.md) for the bookmark path.

## Context

Roon's hearts (favourite tracks, albums, artists) are a core browsing primitive: the Favourites
screen, "play favourites", Subsonic `star`. The goal says social objects use the v1 pubky-app-specs
types and stay in a private folder until the social primitives are defined.

## Decision

A favourite is a `PubkyAppBookmark { uri, created_at }` whose `uri` is the object's canonical music
URL (`pm_model::MusicUri`: MusicBrainz recording, release, artist, or the ISRC/service fallbacks of
ADR 0001). The id is the spec's hash of the URI, so the same object hearted twice is one bookmark.
It lives at `/priv/pubky.app/bookmarks/<id>` by default; the public path is the spec's own
`/pub/pubky.app/bookmarks/<id>`, one prefix away.

The library keeps the working copy (`favourites` table) with the same push/tombstone/pull sync as
posts. An object without a canonical identity cannot be hearted; the API answers 422 with the
reason, consistent with constraint 4.

## Consequences

* Favourites survive a node rebuild (pulled from the homeserver) and will become public hearts
  when the primitives land, without a schema change.
* Pinakes can index public music bookmarks as bookmarks of URLs; no new type to teach it.
* Albums are hearted by release MBID, so an album with no MusicBrainz match waits for the resolver.
