# Native API (`/v1`)

The node also serves a built-in browser controller at `/` (one static page over this API: browse,
search, play to any zone or in the browser itself, queue, playlists, discover, settings).

HTTP + WebSocket served by `pm-node serve` on `[api] listen` (default `127.0.0.1:7788`).
Set `[api] token` to require `Authorization: Bearer <token>` (or `?token=` on the WebSocket).
Without a token, keep the listener on localhost. Remote access by pubkey (M5) wraps this API.

| Method | Path | Body / query | Returns |
| :-- | :-- | :-- | :-- |
| GET / PUT | `/v1/node/settings` (`jukebox_zone` picks the zone Subsonic jukebox calls drive; empty clears it) | PUT: `{contact}` | `{contact, discovery}`; the contact (email or URL, never a code-host link) is saved in the library and turns Pinakes/Nexus discovery on without a restart; 400 when invalid |
| GET | `/v1/info` | | name, version, library stats, zone/output counts, `scanning` |
| GET | `/v1/outputs` | | `OutputInfo[]` (id, name, caps) |
| GET | `/v1/outputs/discovery` | | `{upnp: [{from, location, server?, friendly_name?, renderer, output}], cast: [{id, friendly_name, addr}], public_url, local_address?, notes, upnp_enabled, cast_enabled, saved_addresses}`: a 3 s network scan (`?quick=1` skips the scan and only reports configuration) with plain-language findings (no multicast answers, unusable public URL, devices that are not renderers) |
| GET | `/mb/ws/2/{path}?…` | | no token: a MusicBrainz `/ws/2` caching proxy (one upstream request per second, answers cached a week, the node's contact in the User-Agent); for Pinakes's `[also_on.sources.musicbrainz] base_url = "https://<node>/mb/ws/2"`; 412 until the node has a contact |
| POST | `/v1/node/settings/backup` | | write the owner's settings (contact, folders, TIDAL session, renderers, jukebox zone, DSP) to `/priv/pubky.music/settings.json`; also done after every sync |
| POST | `/v1/node/settings/restore` | | bring them back from the homeserver onto this node (missing values only); done by itself on a fresh node after the first sign-in |
| POST | `/v1/auth/pubky/start` | | no token needed: starts a Pubky Ring sign-in for a controller or browser; `{id, url, owner}`; 412 until the node itself has a Pubky session |
| GET | `/v1/auth/pubky/poll` | `?id=` | `{status: pending\|ok\|denied\|expired, token?, user?}`: `ok` carries the API token when the approver is the node's owner |
| POST | `/v1/tags/bulk` | `{label, album_id?, artist_id?}` | one public tag on every track in scope that has a MusicBrainz id or ISRC (whole library when no scope); `{label, tagged, already, skipped_no_identity}`; the sync publishes them and Pinakes joins each URL to its group across services |
| POST | `/v1/outputs/upnp` | `{address}` | add a renderer multicast cannot find: an IP (unicast M-SEARCH to port 1900, then common description ports/paths), `ip:port`, `ip mac serial` (Linn: the UDN is `4c494e4e-<MAC>-<serial>01<type>`; with the serial number every type byte is tried on port 55178), or its description URL; registered as output + zone and remembered for the next start; 400 with the reason when nothing answers |
| DELETE | `/v1/outputs/upnp/{address}` | | forget a saved address (URL-encoded); its zone stays until restart |
| POST | `/v1/outputs/endpoints` | `{address}` | add a `pm-endpoint` at `host:port` (the desktop app registers its player this way; mDNS does not cross subnets): one output and zone per device it offers, remembered for the next start; 400 with the reason when it does not answer |
| DELETE | `/v1/outputs/endpoints/{address}` | | forget a saved endpoint; its outputs and zones go away at once |
| POST | `/v1/outputs/rescan` | | discover again (local devices, endpoints); new outputs get zones; returns the list |
| POST | `/v1/outputs/pull` | `{name}` | (UPnP/OpenHome renderers found by SSDP and Cast devices found by mDNS get one of these automatically (`upnp-<udn>`, `cast-<id>`) and appear as outputs `upnp:<udn>` / `cast:<id>` with zones; `[outputs] upnp`, `[outputs] cast`, `[api] public_url`) |
| POST | `/v1/outputs/pull` | `{name}` | a pull output and its zone `Stream <name>`; renderers, browsers or scripts fetch `GET /v1/outputs/pull/{name}/stream.wav` (live WAV of the zone's post-DSP output; the listener paces the zone; 408 when nothing plays) |
| GET | `/v1/outputs/pull/{name}/stream.wav` or `stream.l16` | | the live stream (`?k=` key or the token). `stream.l16` is DLNA LPCM, raw big-endian 16-bit `audio/L16;rate=…;channels=…`, for TVs that take nothing else. Both answer with `transferMode.dlna.org` and `contentFeatures.dlna.org`; HEAD answers the headers without taking the stream |
| GET | `/v1/outputs/pull/{name}/url` | | `{url}`: the stream URL to give a renderer or player by hand. It carries that stream's key (`?k=`), which opens this one stream and nothing else; the API token also works. Renderers the node finds itself (UPnP, Cast, Slimproto) get keyed URLs; they never get the API token |
| GET | `/v1/zones` | | `ZoneState[]` |
| POST | `/v1/zones` | `{name, output}` or `{name, outputs: [leader, …]}` for a group | the new `ZoneState`; `now_playing.members` carries per-output drift and `slipped_frames` (net frames duplicated/dropped to stay in time) for groups |
| GET / DELETE | `/v1/zones/{id}` | | `ZoneState` / 204 |
| GET | `/v1/zones/{id}/queue` | | `QueueItem[]` |
| POST | `/v1/zones/{id}/queue` | `{track_ids?, items?, at: end\|next, replace?, play?}` | `{ids}` |
| DELETE | `/v1/zones/{id}/queue` | | 204 (stops and clears) |
| DELETE / PUT | `/v1/zones/{id}/queue/{item}` | PUT: `{to_index}` | 204 |
| POST | `/v1/zones/{id}/transport` | `{action: play\|pause\|toggle\|stop\|next\|previous\|seek, index?, position_ms?}` | `ZoneState` |
| POST | `/v1/zones/{id}/volume` (absolute dB requests are capped at the zone's `max_volume_db` from its DSP settings) | `{volume: {db: -20} \| {percent: 50} \| "up" \| "down" \| {mute: true}}` | 204 |
| GET / PUT | `/v1/zones/{id}/sleep` | PUT: `{seconds}` or `{seconds: null}` | `{ends_at_ms?, remaining_ms?}`; sleep timer: the zone pauses when it fires; one timer per zone, a new PUT replaces it |
| POST | `/v1/zones/{id}/transfer` | `{to}` | the target's `ZoneState`; moves the queue, the playing item and its position, repeat and radio to zone `to` (paused stays paused); the source ends up stopped and empty; 400 for an empty queue or the same zone |
| POST | `/v1/zones/{id}/shuffle` | | 204; shuffles the items after the current one (all items when nothing plays) |
| PUT | `/v1/zones/{id}/radio` | `{on}` | library radio: when the queue ends (repeat off), the node appends up to 10 tracks similar to the last one (same genres, then same artists, then anything) and keeps playing; the flag is in `ZoneState.radio` and survives restarts |
| PUT | `/v1/zones/{id}/repeat` | `{repeat: off\|all\|one}` | 204 |
| GET / PUT / DELETE | `/v1/zones/{id}/dsp` | PUT: `PipelineConfig` `{headroom_db?, volume_db?, max_volume_db? (volume limit; caps volume_db and every volume request), dither, dsd_mode, resample, loudness: off\|track\|album, eq: [{kind, freq_hz, gain_db, q, enabled}], convolution?: {path, gain_db, enabled}, crossfeed?: {level_db, cutoff_hz, delay_us, enabled}}` | per-output DSP, persisted; applies at the next track start; DELETE restores the default |
| GET | `/v1/library/stats` | | `LibraryStats` |
| GET | `/v1/library/tracks` | `?after=<id>&limit=` | `{tracks}` |
| GET | `/v1/backup` | | a consistent SQLite snapshot of the library database (`application/vnd.sqlite3`, attachment `pubky-music-<date>.db`); `pm-node restore <file>` brings it back |
| GET | `/v1/library/tracks/{id}/lyrics` | | `{track_id, source: lrc\|tag, synced, text, lines: [{ms?, text}]}` from the file's tags or a sidecar `.lrc`; 404 when none |
| GET | `/v1/library/tracks/{id}` | | `Track` |
| GET | `/v1/library/search` | `?q=&limit=&sources=1` | `{tracks, albums, artists, genres, sources: [{source, items}]}`: tracks by full text (title, artist, album, genre words), albums by title, artists and genres by name; `sources` carries hits from streaming services that take part (TIDAL when configured; `sources=0` skips them), each hit flagged with `library_track_id` / `library_album_id` when the library has the same ISRC or UPC |
| GET | `/v1/library/albums/{id}/versions`, `/v1/library/tracks/{id}/versions` | | `[{source, id, label, url, uri?}]`: the same album (by UPC) or track (by ISRC) on TIDAL, cached a week; empty when unknown |
| GET | `/v1/sources/tidal/stream/{id}` | | the track's HLS segments as one `audio/mp4` body for browsers (`X-Tidal-Presentation: FULL\|PREVIEW`); sign-in required |
| GET | `/v1/sources/spotify/login` | | Spotify sign-in URL (Authorization Code + PKCE); the app must list `<public_url>/spotify/callback` |
| GET | `/v1/sources/spotify/status` | | `{configured, signed_in, user_id?, display_name?, country?, product?, redirect_uri, requested_scopes}` |
| DELETE | `/v1/sources/spotify/session` | | sign out |
| GET | `/v1/sources/spotify/collection` | | saved albums and tracks as hits (ISRC/UPC, library matches) |
| POST | `/v1/sources/spotify/collection/import` | | saved tracks (by ISRC) and albums as favourites |
| GET | `/v1/sources/spotify/playlists` | | the user's playlists |
| POST | `/v1/sources/spotify/playlists/{id}/import` | | a private Pubky playlist by ISRC |
| GET | `/v1/sources/spotify/devices` | | Spotify Connect devices |
| POST | `/v1/sources/spotify/play` | `{uris, device_id?}` | start `track:<id>` / `album:<id>` on a device (Premium); no audio enters the node |
| GET | `/v1/sources/deezer/stream/{id}` | | 307 to the track's 30 s preview (browsers); zones play `deezer:track:<id>` directly |
| GET | `/v1/library/artists` | | `ArtistSummary[]` (id, name, mbid, album and track counts) |
| GET | `/v1/library/artists/{id}` | | `{artist, albums}` |
| GET | `/v1/library/artists/{id}/tracks` | | `{tracks}` |
| GET | `/v1/library/artists/{id}/info` | | `{bio?, image, wikipedia?, wikidata?, musicbrainz?, homepage?, note?}` from MusicBrainz → Wikidata → Wikipedia by the artist's MBID, fetched once and cached for 30 days (misses 7) |
| GET | `/v1/library/artists/{id}/image` | | the cached portrait (Wikimedia Commons); 404 when none |
| GET | `/v1/library/genres` | | `[{id, name, tracks, albums}]` from the files' genre tags (several per track when the tag lists several) |
| GET | `/v1/library/genres/{id}` | | `{genre, albums, tracks}` |
| GET | `/v1/library/albums` | `?after=&limit=&focus=` | `Album[]`; `focus` keeps albums with at least one file of that quality: `hi_res` (PCM above 16/48, lossless), `dsd`, `cd` (16-bit at 44.1/48), `lossless`, `lossy` |
| GET / POST | `/v1/library/new-releases` (POST `…/refresh`) | | `{items: [{artist_id, artist, release_group_mbid, title, first_release_date, kind, url, in_library?}], artists_total, artists_checked, refreshing, refreshed_at?}`: albums, EPs and singles from the last year that MusicBrainz lists for the library's artists (those with an artist MBID), cached a week per artist and refreshed in the background one request per second; needs a contact |
| GET | `/v1/library/albums/formats` | | `[{album_id, label, lossless, hi_res, dsd}]`: the best present file per album, e.g. `24/96 FLAC`, `DSD64`, `MP3` |
| GET | `/v1/library/albums/{id}/tracks` | | `{tracks}` |
| GET | `/v1/library/albums/{id}/cover` | | image bytes: the embedded picture extracted at scan, `cover.jpg` next to the files, or (for albums with a MusicBrainz release/release-group id) the Cover Art Archive front image fetched once and cached in the art dir; misses are remembered for a week; 404 when none |
| GET | `/v1/library/history` | `?limit=` | `{plays: [{id, track, zone, played_at}]}`, newest first; node zones record a play when a library track starts |
| POST | `/v1/library/history` | `{track_id, zone?}` | records a play made by the client itself (the app's local zone, the browser); 404 for an unknown track |
| GET | `/v1/library/home` | `?limit=` | `{recently_added, recently_played, most_played_albums, most_played: [{track, plays, last_played_at}], history, stats}` |
| GET | `/v1/library/folders` | | `[path, present][]` |
| POST | `/v1/library/folders` | `{path}` | adds a folder on the node (must exist), watches it and starts a scan; `{folders, scan_started}` |
| DELETE | `/v1/library/folders` | `?path=` | forgets the folder and the tracks that lived only there; `{folders, removed_files, removed_tracks}`; 409 while a scan runs |
| POST | `/v1/library/scan` | `{folders?}` | `{folders}`; 409 while a scan runs. With `[library] watch = true` (default) the node also rescans by itself after `watch_quiet_secs` without file changes |
| GET / POST | `/v1/playlists` | POST: `{name, description?, image?, uris?, track_ids?}` | `PlaylistSummary[]` / `PlaylistDetail` |
| GET / PUT / DELETE | `/v1/playlists/{id}` | PUT: `{name?, description?, image?, uris?}` | `PlaylistDetail` (items + resolved library tracks) |
| POST | `/v1/playlists/{id}/items` | `{uris?, track_ids?, at?}` | `PlaylistDetail`; 422 for a track without a canonical identity |
| DELETE | `/v1/playlists/{id}/items/{index}` | | `PlaylistDetail` |
| POST | `/v1/playlists/{id}/play` (entries that are Internet Archive file URLs stream directly; other unresolved entries are reported as `missing`) | `{zone, replace?}` | `{queued, missing}` |
| GET | `/v1/favourites` | | `{tracks, albums, artists, other: [url], uris: [url]}`: hearts joined to the library (ADR 0010) |
| PUT | `/v1/favourites` | `{uri}` or `{track_id}` / `{album_id}` / `{artist_id}` | `{uri, favourite: true}`; 422 when the object has no MusicBrainz id or ISRC |
| DELETE | `/v1/favourites` | `?uri=` or `?track_id=` / `?album_id=` / `?artist_id=` | `{uri, favourite: false}` |
| GET | `/v1/tags` | none, or `?uri=` / `?track_id=` / `?album_id=` / `?artist_id=` | `{uri?, tags: [{uri, label, created_at, synced}]}`: my tags (ADR 0011) |
| PUT | `/v1/tags` | `{uri|track_id|album_id|artist_id, label}` | `{uri, label, tagged: true}`; label trimmed and lowercased, 1–20 chars; 422 without a canonical identity |
| DELETE | `/v1/tags` | `?…&label=` | `{uri, label, tagged: false}` |
| GET | `/v1/reviews` | none, or `?uri=` / `?track_id=` / `?album_id=` / `?artist_id=` | `{uri?, reviews: [{id, uri, rating?, text, created_at, updated_at, synced}]}`: my private reviews (v1 `pubky.music/review` posts) |
| PUT | `/v1/reviews` | `{uri|track_id|album_id|artist_id, rating?: 1–5, text?}` | creates or replaces my review of that object (same post id); 400 with neither rating nor text, 422 without a canonical identity |
| DELETE | `/v1/reviews` | `?…` | removes it (tombstoned until the delete is pushed) |
| POST | `/v1/playlists/from-queue` | `{zone?, uris?, name?}` | `{playlist: PlaylistDetail, skipped: [label]}`; a new private playlist from a zone's queue (or the given queue URIs: `track:<id>`, local paths, canonical URLs); entries without a canonical identity are listed in `skipped`; 422 when nothing qualifies |
| POST | `/v1/playlists/import` | `{name, description?, tracks: [{title, artist?, album?, uri?, path?}]}` | `{id, name, parsed, imported, unresolved}`; items resolve through the library (canonical URL, exact path, file name, title + artist) to canonical URLs |
| POST | `/v1/playlists/import/file` | `?file=<name>&name=&format=` raw body: M3U/M3U8, CSV (Exportify, Soundiiz, Roon export saved as CSV) or Roon's Excel export (.xlsx) | same report; 422 when nothing matched |
| GET / POST | `/v1/activity` | POST: `{track: {title, artist?, album?, uri?}, source, duration_ms?}` | plays as private activity posts; 422 when nothing canonical resolves |
| GET | `/v1/discover/uri` | `?uri=` / `?track_id=` / `?album_id=` / `?artist_id=` | `{uri, indexed, tags: [[label, people]], taggers}`: what Pubky (Nexus) knows about one object; 412 without a `contact` |
| GET | `/v1/discover/charts` | `?type=tracks|albums|artists&window=24h|7d|30d&limit=` | Pinakes chart, one song across services one row; items carry `kind`, `site`, `group_size`, `playable`, `also_on` |
| GET | `/v1/discover/search` | `?q=&type=&limit=` | Pinakes text search over music tagged on Pubky, joined to the library |
| GET | `/v1/discover/item` | `?uri=` | any link, tagged or not (unknown links are read by Pinakes on demand; `reading: true` with the provider's data meanwhile): one card: facts (`mbid`, `isrcs`, `upc`, `duration_secs`, albums), every `also_on` link, `on_pubky` tags/taggers, `in_library_album` |
| GET | `/v1/search` | `?q&in&limit` | one search, streamed as NDJSON: one `{source, status: ok\|error\|timeout, ms, error?, hits[{kind, title, subtitle, image, id, page, uri, track_id, artist_id, album, album_id, duration_ms, year, keys}], counts, cached}` line per source as it answers; `in` is `everywhere` (default), `library`, `pubky` or a service id; header `x-pm-sources` lists the sources asked |
| GET | `/v1/home/{section}` | `?window&limit` | one Home section (`trending-songs`, `trending-albums`, `trending-artists`, `following`, `tags`, `concerts`, `podcasts`, `yours`, `new`; `tag?labels=a,b`): `{section, items, tags, note?, needs?: pubky\|location\|contact, error?}`; cached 60 s |
| GET/PUT | `/v1/home/location` | `{city}` or `{lat, lon}`; `{}` clears | the city for concerts (looked up once on OpenStreetMap) |
| GET | `/v1/item/{kind}/{id}` | | one item page (`song`, `album`, `artist`) from any source; `id` is `lib-<n>`, `mb-<mbid>`, `isrc-<ISRC>` or `u-<base64url of the link>`: `{kind, id, uri, target, title, artist{name,id}, album{name,id}, year, image, source, plays_from, connect, reading, songs[{no,title,artist,artist_id,album,album_id,duration_ms,uri,track_id,page,id}], total_ms, albums[{id,title,year,image}], versions, on_pubky, about[{label,value,href}], blurb, open_on, section}`; `target` is what `/v1/tags`, `/v1/reviews` and `/v1/favourites` take |
| GET | `/v1/discover/contents` | `?uri=` | an item's songs (albums) or albums and top songs (artists) from the service it plays from, falling back to the page's own link, Deezer previews, then MusicBrainz; cached six hours |
| GET | `/v1/item/candidates` | `?uri&kind&title&artist` | the same title on the connected services ("possibly the same"), for the owner to confirm |
| POST | `/v1/item/matches` | `{uri, version}` | the owner confirms a match; stored on this node only, used for Versions and playback |
| GET | `/v1/discover/play` | `?uri=` | where the item plays from, resolved through the node's connected services: the library (`track_ids`), else the best service link, tagged first, TIDAL before Spotify Connect (`items` to queue, or `spotify_uris` for `POST /v1/sources/spotify/play`); albums expand to their tracks on that service. With nothing connected: `plays_from` absent, `connect` names the services that have it, `preview` a 30 s Deezer excerpt. Rows and `/discover/item` carry the same `plays_from`/`connect` without expanding |
| POST | `/v1/discover/describe` | `{uris}` | rows for music URLs outside the library (≤ 500): title, artist, image, `site`, `plays_from`, `connect`; Pinakes batch lookup, then the provider's API for that id; unknown titles stay empty, never the URL; cached a day |
| GET | `/v1/library/identify` | | `{progress: {running, total, done, identified, suggested, unknown, errors, last_error?}, counts: {total, identified, suggested, unknown, unchecked}, acoustid}` |
| POST | `/v1/library/identify` | `{retry?}` | start the identify job (fingerprint → AcoustID → folder as release → suggestions); resumes, `retry` looks again at "not found" tracks |
| GET | `/v1/library/suggestions` | | open suggestions with their track: `{id, track_id, recording_mbid, release_mbid?, title?, artist?, release_title?, score, source: fingerprint\|search, track}` |
| POST | `/v1/library/suggestions/{id}/accept` | | the owner confirms: the track's identity becomes that recording (`confirmed_by_user`) |
| DELETE | `/v1/library/suggestions/{id}` | | not this one |
| PUT | `/v1/library/identify/settings` | `{write_ids}` | "Write IDs into my files" on or off (off by default) |
| POST | `/v1/library/identify/write` | | write the IDs found so far into the files now (FLAC, WAV, AIFF; tags backed up first): `{written, skipped: [{track_id, path, reason}], backup_dir}` |
| GET | `/v1/discover/trends` | `?window=7d&limit=` | Pinakes trending music, each item with `in_library` (the matching track); 412 without a valid `contact` |
| GET | `/v1/discover/tag/{label}` | `?limit=` | music tagged `label` |
| GET | `/v1/discover/friends/{viewer}` | `?limit=` | music the people a pubky follows tagged or posted |
| GET | `/v1/sources` | | streaming sources the node can search and play: `[{id, name, description, status}]` (`archive` = Internet Archive, `audius` = Audius, `tidal` = TIDAL, `deezer` = Deezer catalogue with 30 s previews; hits carry `uri` when a track can be queued directly) |
| GET | `/v1/sources/{source}/search` | `?q=&limit=` | `{source, items: [{id, title, creator?, year?, downloads?, image?, kind: item\|track, url}]}` |
| GET | `/v1/sources/tidal/login` | | `{url}`: TIDAL's sign-in page (OAuth 2 + PKCE, scopes incl. `playback`); the browser comes back to `/tidal/callback` (no token needed there), which stores the user's tokens and redirects to `/?tidal=signed_in` |
| GET / DELETE | `/v1/sources/tidal/status` (DELETE `…/session`) | | `{configured, signed_in, user_id?, country?, scope?, expires_at?, redirect_uri, requested_scopes, access_tier?, playback_limit?}` (`playback_limit` such as `FULL_REQUIRES_HIGHER_ACCESS_TIER` means TIDAL serves previews to this app's tier); DELETE forgets the tokens |
| GET | `/v1/sources/tidal/collection` (POST `…/import`) | | `{tracks, albums}` of the signed-in user's TIDAL collection as source hits; POST favourites them on the node (`{imported, skipped}`; ISRC where TIDAL has one, else the TIDAL link) |
| GET | `/v1/sources/tidal/playlists` (POST `…/{id}/import`) | | `[{id, name, description?, items, url}]` the user's own playlists; POST creates a private Pubky playlist of canonical URIs (`{imported, skipped, playlist}`) |
| GET | `/v1/sources/{source}/items/{id}` | | `{source, id, title, creator?, year?, description?, licence?, image?, url, tracks: [{title, track_no?, duration_ms?, format, url, artist?}]}`; play by enqueueing the track URLs (the archive gives one best-format file per recording, FLAC first; Audius one MP3 stream per track; TIDAL `tidal:track:<id>` URIs that the node resolves to a stream manifest for the signed-in user; TIDAL ids are `album:<id>` and `track:<id>`) |
| GET | `/v1/radio/search` | `?q=&tag=&country=&limit=` | `{stations: [{id, name, url, homepage?, favicon?, codec?, bitrate?, country?, tags}]}` from radio-browser.info, most voted first; 400 without a term, 502 when the directory fails |
| GET / PUT | `/v1/radio/stations` | PUT: a `Station` | saved stations (node-local settings); PUT adds or updates by id/url |
| DELETE | `/v1/radio/stations/{id}` | | remaining stations; 404 when unknown |
| POST | `/v1/radio/play` | `{zone, station}` | replaces the zone's queue with the stream and plays it |
| GET | `/v1/pubky` | | `{user, unsynced}` |
| POST / GET | `/v1/pubky/login` | | POST starts a Pubky Ring sign-in and returns `{url}` (`pubkyauth://…`); GET polls `{status: none\|pending\|signed_in, user?, url?}`. On approval the node stores the session secret, publishes its record and syncs |
| POST | `/v1/pubky/sync` | | `{pushed, pulled, deleted, errors}`; 412 without a session |
| GET / POST | `/v1/pubky/files` | POST: `{track_id}` or `{path}` | private files on the homeserver (`/priv/pubky.music/files/`); upload a node file |
| POST | `/v1/pubky/files/mirror` | | download missing files into the node's mirror folder and scan it: `{fetched, present, bytes, errors}` |
| GET | `/upnp/node.xml` | | (no token) the UPnP device description the SSDP announcement points at: name, version, `uuid:<node id>`, `presentationURL` `/`. See `docs/CONNECTION.md` |
| GET | `/v1/events` | WebSocket | first `{"type":"snapshot","seq","boot","library_rev","zones":[...]}`, then numbered (`seq`) `zone` (full state; also sent when a zone is created), `zone_event` (track started/ended, skipped, paused, resumed, seeked, stopped, queue, `stream_title` for radio, error), `zone_removed`, `library_changed`. Send `{"type":"ping","id":n}` for `{"type":"pong","id":n}`, `{"type":"resync"}` for a fresh snapshot. See `docs/CONNECTION.md` |

Playlists are v1 `PubkyAppPost` envelopes (ADR 0006) stored in the node and pushed to the user's private
homeserver folder when `pubky.session_secret_file` is set. Items are canonical music URLs only.

Queues (items and repeat mode) are saved per zone on every change and restored, stopped, when the node restarts. Queue item URIs: `track:<library id>`, a local path, `file://`, or `http(s)://` (internet radio and direct file URLs; M3U/PLS playlist URLs are followed to their first stream; lossy streams mark the decode stage `Lossy`; ICY titles arrive as `now_playing.stream_title`). Subscription streaming services add schemes later.
Every `ZoneState` carries `now_playing.signal_path`, so a controller can render the chain for any zone.
Types live in `pm_api::types`; a Rust client reuses them.

## OpenSubsonic (`/rest/*`)

The same server also speaks the Subsonic API (XML by default, JSON with `f=json`) for existing clients: Symfonium,
Feishin, Amperfy, play:Sub, Supersonic. Log in with `[api] subsonic_user` (default `pubky`) and the
API token as the password (token auth `t`+`s` or plain `p`); without a token anyone can connect.

Implemented: `ping`, `getLicense`, `getOpenSubsonicExtensions`, `getMusicFolders`, `getArtists`,
`getIndexes`, `getArtist`, `getAlbum`, `getMusicDirectory`, `getAlbumList(2)` (newest, random,
alphabetical, byYear), `getSong`, `getRandomSongs`, `search2/3`, `stream`/`download` (original
file, `Range` supported, no transcoding), `getCoverArt`, `getPlaylists`, `getPlaylist`,
`createPlaylist`, `updatePlaylist`, `deletePlaylist` (the same playlists as `/v1`, so tracks need a
canonical identity), `scrobble` (private activity), `startScan`, `getScanStatus`,
`getLyrics` (plain text by artist and title) and `getLyricsBySongId` (OpenSubsonic `songLyrics`, timed lines when the source was),
`jukeboxControl` (drives one of the node's zones from a Subsonic client: `set`/`add`/`clear`/`remove`/`shuffle`/`start`/`stop`/`skip`/`setGain`/`get`/`status`; `?zone=<id>` picks and remembers the zone, otherwise the first by name), plus empty
answers for info calls. Ids: songs are track ids, albums `al-<id>`,
artists `ar-<id>`, playlists `pl-<id>`.
