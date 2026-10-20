# Pinakes music: what you get, and when (steward, updated 2026-10-20 17:10 UTC)

This replaces the earlier version of this file. Real answers from the build that's about to merge are in
`~/pinakes-fixtures/` (re-recorded today, with `openapi-candidate.json`). Code against them.

## When

- **Public instance (8092), today:** the official sources and the **same-event hint** (`maybe_same` on
  event cards, see the end of this file) are live. Music matching is not live there yet.
- **The music PR (pubky/pinakes#21) is MERGED and live on 8092 (2026-10-20 17:30 UTC).** All 12 owner music cases pass on the public instance.
  load) and is with Talos now. Expect it on 8092 **later today**; the steward promotes it right after the merge.
- **Coming after it:** podcasts, books and films (`also_on` across Apple Podcasts, Spotify, Podcast Index,
  RSS…), then places, papers, software and **Discogs** (artist ↔ artist, master ↔ album, release ↔
  edition, release barcodes as `upc:` keys). Both are reviewed and restacking on #21.

## What the music PR gives you

- **Types by what the link is**, on every service: track `MusicRecording`, album `MusicAlbum`, artist
  `MusicGroup` (`Person` when MusicBrainz says so), playlist `MusicPlaylist`. `site` is the plain service
  name ("TIDAL", "Apple Music", "Bandcamp"). Every spelling of a link is one resource: TIDAL `/u` share
  links, `/browse/`, locale paths, `intl-xx`, `itunes.apple.com` → `music.apple.com`.
- **`also_on` on every card (your "play on…" buttons):**
  - **one link per music service**, best first: a link someone on Pubky tagged, then the original
    album's link (not a compilation), then others;
  - never another page of the card's own service;
  - `GET /v1/resources/{id}` lists them all (`details.also_on`).
  - Derived links can be unavailable in some countries, which is why a tagged link always wins.
- **Exact matching only, never titles:**
  - MusicBrainz URL relations;
  - Wikidata service IDs (read back);
  - ISRCs and barcodes from iTunes and Deezer, when exactly one object answers;
  - several releases of one release group join at album level.
  - **Of the services' own page ISRCs, only TIDAL's counts as a key**; Bandcamp and SoundCloud
    uploaders type theirs, so those are facts only. An ISRC MusicBrainz gives to several recordings joins
    nothing.
- **`details.facts`:** `mbid`, `isrc` (up to 8), `upc`, `artist`, `artist_type`, `duration` (seconds).
  Use `mbid`/`isrc` to reach your MusicBrainz replica for track lists, credits, dates and cover art.
  Pinakes is not a catalogue.
- **`GET /v1/search?key=`:**
  - `mbid:{uuid}`, `isrc:{code}` (any case, hyphens allowed), `upc:{barcode}` (UPC/EAN/GTIN, any
    length, check digit verified), `wikidata:{Q-id}`;
  - returns every tagged link carrying it, on any service.
  - Fixture: `search-key-isrc.json` (GBUM71029604, Bohemian Rhapsody).
- **Charts:** `GET /v1/search?sort=trending&window=24h|7d|30d&collapse=group&type=MusicRecording`
  (or `MusicAlbum`). `collapse=group` makes one song across services one row.
- **Groups:**
  - a card's `group` is `{id, size}`, and `GET /v1/groups/{id}` lists members and keys
    (`group-radiohead.json`: TIDAL, Bandcamp, Spotify, Apple Music, Deezer, Qobuz, Amazon, SoundCloud…);
  - a group never holds two different MBIDs or Q-ids, so one bad Wikidata claim can't chain two artists.
- **Speed:** hub lookups are batched (MusicBrainz 50 URLs per request), so a new link usually has
  its `also_on` within a minute or two.

## For your MusicBrainz replica

- Pinakes can point at your mirror: `[also_on.sources.musicbrainz] base_url = "https://…/ws/2"`.
- Tell the steward when your replica is reachable from gil/pinakes, and Pinakes will use it instead of
  musicbrainz.org (MusicBrainz allows 1 request/s per IP).

## Events: `maybe_same` (live on 8092 now)

- Event and occurrence cards carry `maybe_same`: up to 3 events that are possibly the same one
  (`{id, uri, site, start, score}`), and `details` lists up to 10.
- The rule: starts within 300 s; within 500 m (venue or organizer only without coordinates); name
  similarity ≥ 0.5 after removing dates and filler words; numbers keep editions apart.
- It's a hint: never merged or counted. Use it to show "also listed on…" for concerts and gigs.

## Use

1. Store canonical URIs: the MusicBrainz URL first, then `https://musicbrainz.org/isrc/<ISRC>`, then the
   service URL.
2. Cards: `POST /v1/resources/lookup` (≤500 links) or `GET /v1/resources/by-uri`. Escape all text.
3. Limits: 120 requests/min per IP on the public API.

## Node side (2026-10-20, after the merge)

What this repository does with each item of the steward's note:

- **`also_on`, one link per service, tagged first:** `pm_pubky::pinakes::AlsoOn` (`id`, `derived`, `via`,
  `is_tagged()`); cards keep the one-per-service list, `ResourceDetail::also_on()` the full one. Shown as
  "Versions" on library albums and tracks and as "Play on…" rows on Discover items, tagged links first
  (`pm-api/src/discover.rs::also_on_rows`); TIDAL and Deezer rows carry a queue URI.
- **Only TIDAL page ISRCs are keys:** the node never derives a canonical URI from a Bandcamp or SoundCloud
  ISRC; its own canonical order stays MusicBrainz recording URL, then `https://musicbrainz.org/isrc/<ISRC>`
  from the file's tags, then the service URL (ADR 0001).
- **`key=` search:** `SearchQuery::key`, `Discovery::tagged_anywhere`; Versions ask Pinakes by
  `mbid:` / `isrc:` / `upc:` because Pinakes has no card for a MusicBrainz URL itself (verified live).
- **Charts:** `SearchQuery::trending(type, window)`, `Discovery::trending_of`, `GET /v1/discover/charts`.
- **Conflict rule (a group never holds two MBIDs or Q-ids):** nothing to implement; the node never merges
  groups itself and joins only on the exact ids Pinakes returns (`group.id`, `facts.mbid`).
- **Batched lookups:** `PinakesClient::lookup` already sends `POST /v1/resources/lookup` in chunks of
  `LOOKUP_CHUNK` (≤ 500 links), under the client-side 120 requests/min budget.
- **`base_url` for a MusicBrainz replica:** not used by this node. The owner asked that Pinakes is never
  pointed at the owner's own host. The node's MusicBrainz proxy (`GET /mb/ws/2/<path>`, `pm-api/src/mbproxy.rs`:
  the owner's contact, one request per second, a week of cache) serves the node itself only.
  Cross-service identity goes through the generic MusicBrainz page of the song (its canonical URL), tagged
  like an OSM place, so any Pinakes and any client join on it without knowing this node.
- **`maybe_same` on event cards:** `Card::maybe_same` / `Details::maybe_same` (parsed, not yet shown: the
  node has no events view).
- **Matching the owner's own library:** `POST /v1/tags/bulk` (Settings → Pubky → "Tag my whole library on
  Pubky") writes one v1 `PubkyAppTag` per identified track on its canonical MusicBrainz URL; Pinakes then
  joins each to its group. Everything the node writes stays on the v1 specs: bookmarks for favourites
  (ADR 0010), tags (ADR 0011), posts with custom content for playlists, reviews and activity (ADR 0006).
