# Pinakes context for the Pubky Music Node (handoff §12)

Pinakes is the indexer for **external resources** people reference on Pubky. It follows Nexus's
reference feed, reads each referenced link once (through official APIs where they exist) and
serves a public, read-only card API.

## Endpoints you can use from this machine
- **API (public dev instance):** https://8092--main--pinakes--gil.coder.riginode.xyz
- **Swagger UI:** https://8092--main--pinakes--gil.coder.riginode.xyz/swagger-ui/
- **OpenAPI:** `/v1/openapi.json`
- **Demo (every route with its exact request):** https://8096--main--pinakes--gil.coder.riginode.xyz
- **How it works (one page):** https://claude.ai/artifact/2KeVfabR7faevvFbJjtRxt
- **Nexus (social graph):** https://nexus.staging.pubky.app (public).

The dev Nexus with Pinakes's reference feed isn't reachable from other workspaces. You don't need
it as a client.

## API facts
- **Auth:** none. The API is public and read-only. Per-IP budget: 120 requests/min; on-demand
  previews (`POST /v1/resolve`) 30/min. CORS is open.
- **Pagination:** keyset `cursor` + `limit` (max 500). No offsets.
- **Updates:** no push. Poll `POST /v1/changes/lookup {"uris":[…],"since":"now-1d"}` for what
  changed on links you hold.
- **Main reads:**
  - `GET /v1/search`: q, type, tags, author, `reach=following&viewer=<pubky>`, time `from`/`to`,
    `facet=type,tags`, bbox/near, `collapse=group`.
  - `GET /v1/resources/by-uri?uri=…` and `POST /v1/resources/lookup {"uris":[…]}`: cards for
    links.
  - `GET /v1/groups/{id}`; `POST /v1/resolve` for a link nobody tagged yet.
- **Card:** `{id, uri, status, type, name, description, image, site, point, start, end, group,
  attribution}`, plus `details` on the full read. Text is plain text: escape it before
  rendering as HTML.
- **Join with Nexus by URL,** never by id. Each side keeps its own ids.

## What Pinakes indexes
- Every link Nexus reports as referenced, today through **tags**
  (`/pub/pubky.app/tags/:id`, `PubkyAppTag` with any URI as target).
- Nexus reserves further reference kinds (reply, embed, collection, event).
- **A `/pub/pubky.music/playlists/:id` file is not indexed by itself.**
  - Its items reach Pinakes only if they are tagged, or once Nexus emits `collection`
    references for a playlist schema.
  - Agree that schema with the Nexus/Pinakes owner before relying on it.

## Canonical music URIs: use real URLs, not a `music:` scheme
Pinakes reads web URLs and a few identifier schemes (`isbn:`, `doi:`, `geo:`, `pubky://`, …). It
would refuse an unknown `music:` scheme. Target tags and playlist items at:
- recording: `https://musicbrainz.org/recording/<mbid>` (preferred);
- release: `https://musicbrainz.org/release/<mbid>` (or `/release-group/<mbid>` for the album as
  a work);
- artist: `https://musicbrainz.org/artist/<mbid>`;
- with no MBID yet: the service's own track URL (Spotify, TIDAL, Qobuz, Apple Music, Deezer,
  YouTube Music…). Pinakes links service URLs to the MBID (see below).

## Music support: status
- **Today:** music links get cards from the services' public pages, but typing is
  inconsistent (Spotify tracks show as `VideoObject`, Apple Music as `MusicComposition`).
- **Step 8, in progress, first PR is music plus Wikipedia, about 3–4 days:**
  - every track is `MusicRecording`, album `MusicAlbum`, artist `MusicGroup`;
  - facts `isrc`, `upc`, `mbid`, `artist`, `album`, `duration`;
  - an `also_on` list on each card with the same recording or release on other services,
    found through exact MusicBrainz/Wikidata ids (never by title), marked `derived` with its
    source;
  - links to one recording on different services name each other as group members (hints,
    never merges).
- **Staging seed data:** 8 seed accounts tagged 60 links covering 5 songs, 4 albums and 2
  artists, across Spotify, Deezer, TIDAL, Apple Music, Qobuz, YouTube (Music), Amazon Music,
  Beatport, SoundCloud, Bandcamp and Audiomack. Labels: `music`, the service name,
  `track|album|artist`, the song.

## Ranking and trends
- **Score:**
  `log1p(taggers) + 0.5·log1p(replies) + 0.3·log1p(embeds + collections)`, 30-day half-life.
  It ranks only and is never shown.
- **Friends:** `reach=following&viewer=<pubky>`.
- **Trends:** tag facets over a time window.
- **To feed them, publish:** tags on canonical URIs. Collections (playlists) and listening
  activity count once Nexus emits those reference kinds; agree the schemas first.

## Deployment
Use the shared instance. A per-node Pinakes is possible (Rust, PostgreSQL 16 + PostGIS +
pg_trgm + h3, fed by a Nexus reference feed), but it would only see what its Nexus sees. Disk is
about 12 KB per distinct link plus 0.7 KB per tag.

## Machine notes
- This workspace shares the host disk with the Pinakes builds, and the disk is about 87% full.
  Build Rust with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only` and clean
  target dirs you no longer need.
- Claude Code here starts with the default account's credentials (from the template). Sign in
  with whichever account you want.
