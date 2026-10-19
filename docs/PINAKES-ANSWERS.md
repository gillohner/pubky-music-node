# Answers from the Pinakes side (2026-10-19)

1. **ISRC fallback, agreed.** Use `https://musicbrainz.org/isrc/<ISRC>` when no MBID is known.
   Pinakes's music step will read it as the exact key `isrc:<ISRC>`. An ISRC that names several
   recordings joins none of them. Preference order stays: MusicBrainz recording URL, then the
   ISRC URL, then the service's own track URL.
2. **The routes you found missing exist in the next release.** The public instance (8092) still
   runs the previous version. The core release now in final review adds:
   - `GET /v1/resources/by-uri?uri=…` (any spelling; 404 if unknown, never fetches);
   - `POST /v1/changes/lookup` and `POST /v1/occurrences/lookup` taking `{"uris":[…]}`, with
     `since` optional;
   - `facet=type`; group hints on cards (`group`).

   It reaches 8092 about a day from now. Code against these routes now, and keep
   `POST /v1/resources/lookup` (which works today) as the batch path.
3. **`from`/`to` filter event start times, so they don't fit music.** Two music routes come with
   the music step (the first "also on" PR, about 3–4 days):
   - `GET /v1/search?key=mbid:<uuid>` (also `isrc:`, `upc:`): every tagged link carrying that
     exact id, on any service;
   - `GET /v1/search?sort=trending&window=7d` (1h–90d): ranked by tags made inside the window;
     with `collapse=group`, one song counts once across services.
4. **Typing and music facts.**
   - Today a Spotify track can come back as `VideoObject` or `MusicRecording`, depending on the
     version and what the page offers. Don't rely on the type yet. Detect music from your own
     `MusicUri` parse of the card's `uri`.
   - The music step makes types consistent (`MusicRecording`, `MusicAlbum`, `MusicGroup`). It
     adds facts `isrc`, `upc`, `mbid`, `artist`, `album` and `duration`, and an `also_on` list
     (`derived` + `via`).
   - Read these fields if present and ignore them if absent. That way your client works on both
     versions.
5. **Joining with Nexus is by URL, never by id.** Send the card's `uri` (and
   `details.aliases`) to Nexus. Pinakes ids are Pinakes's own. For a calendar occurrence, use
   `occurrence.feed_uri`.
6. **Playlists and listening activity aren't indexed yet.** Nexus reports only tags today. For
   v1:
   - keep playlists in your `pubky.music` namespace;
   - make discovery work through plain tags on the canonical URIs;
   - write the playlist schema as a draft for pubky-app-specs: a list of canonical URIs plus
     name and description.

   When it's stable, the Nexus/Pinakes side adds a "collection" reference kind for it, and
   playlist items then count toward ranking. Activity follows the same path later (opt-in).
7. **Clients:**
   - User-Agent with a contact and no "github": sec.gov-style blocks exist, and MusicBrainz
     requires a contact.
   - MusicBrainz at ≤1 request/s. For serious catalogue use, run a local MusicBrainz replica on
     the node instead of the public API.
   - Pinakes: 120 requests/min per IP. Batch with `resources/lookup` (≤500 links).
8. **Hardware:** this machine has no DAC. Do the M1 byte-identical loopback on the real node
   hardware, or with an ALSA `snd-aloop` loopback device on a machine where you can load
   kernel modules.
