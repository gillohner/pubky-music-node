# ADR 0015: Where playlist songs play from (pins and play order)

Status: accepted (2026-10-21, steward plan step 4, STEWARD-UX-BRIEF §3).

## Context

Playlists mix sources: a song can be a local file, a TIDAL or Spotify link, or only an id
(MusicBrainz recording or ISRC link) that any service may have. The owner wants to see where each song
will play from, to pin a song to one source, and to set an order of services per playlist and for the
node. Items stay canonical (ADR 0001), so a shared playlist plays for anyone; a pin is a suggestion.

## Decision

**Data.** Two optional fields, nothing else changes:
- per item, `source: {service, uri?}`: the pin (`service` is `[a-z0-9-]{1,32}`; `uri`, when present,
  an http(s) or pubky link: the exact page to play; `library` has no uri);
- per playlist, `source_order: [service, …]` (at most 16): the playlist's own order.

On the homeserver (ADR 0014) they are the collection item's `ext.music.source` and the collection's
`ext.music.source_order`, read and written by `pubky-music-specs::ext` (hostile input dropped, other
`ext` members kept). In the node's local envelope they are optional fields with serde defaults, so the
envelope stays at version 2: older rows read with no pin and no order. `pubky-music-specs` now also
accepts a source with no `uri` (a service only).

**Resolution** (`crates/pm-api/src/play_from.rs`), shown as the row's chip:
1. the pin, when that exact source is usable on this node;
2. the playlist's order, else the node's global order (setting `play.provider_order`;
   default `library › TIDAL › Spotify`), over the item's sources here: the library file (by canonical
   id), each service's link (the item's own page, Pinakes `also_on`, Pinakes `key=mbid:`/`key=isrc:`,
   then TIDAL's and Spotify's own ISRC lookups);
3. any other usable source (a directly streamable file link);
4. else unavailable, with the reason and the services to connect.

`library` is a member of the order like any service (so "TIDAL before my files" is possible); with
the default order a local file wins. Deezer gives previews only and is never chosen. A pin whose
source is not usable here falls back to the order, and the row says so.

Lookups are cached per ISRC and per link (a day), so a playlist resolves in about 0.5 s once warm;
the first resolution of a long playlist can take tens of seconds, so the page shows the songs at once
and fills the chips when the answer arrives.

**API.** `GET /v1/playlists/{id}/play-from`, `PUT /v1/playlists/{id}/items/{i}/source`,
`PUT /v1/playlists/{id}/order`, `GET|PUT /v1/play-order`. `POST /v1/playlists/{id}/play` queues each
item from its resolved source.

## Consequences

- Play is the same everywhere: the chip, the zone queue and the browser player use one resolution.
- Re-resolution happens on every page load (accounts may have changed); only lookups are cached.
- A per-zone order is out of scope (owner: one provider order for the node).
