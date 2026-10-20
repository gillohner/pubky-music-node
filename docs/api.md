# Native API (`/v1`)

The node also serves a built-in browser controller at `/` (one static page over this API: browse,
search, play to any zone or in the browser itself, queue, playlists, discover, settings).

HTTP + WebSocket served by `pm-node serve` on `[api] listen` (default `127.0.0.1:7788`).
Set `[api] token` to require `Authorization: Bearer <token>` (or `?token=` on the WebSocket).
Without a token, keep the listener on localhost. Remote access by pubkey (M5) wraps this API.

| Method | Path | Body / query | Returns |
| :-- | :-- | :-- | :-- |
| GET / PUT | `/v1/node/settings` | PUT: `{contact}` | `{contact, discovery}`; the contact (email or URL, never a code-host link) is saved in the library and turns Pinakes/Nexus discovery on without a restart; 400 when invalid |
| GET | `/v1/info` | | name, version, library stats, zone/output counts, `scanning` |
| GET | `/v1/outputs` | | `OutputInfo[]` (id, name, caps) |
| POST | `/v1/outputs/rescan` | | discover again (local devices, endpoints); new outputs get zones; returns the list |
| POST | `/v1/outputs/pull` | `{name}` | (UPnP/OpenHome renderers found by SSDP and Cast devices found by mDNS get one of these automatically (`upnp-<udn>`, `cast-<id>`) and appear as outputs `upnp:<udn>` / `cast:<id>` with zones; `[outputs] upnp`, `[outputs] cast`, `[api] public_url`) |
| POST | `/v1/outputs/pull` | `{name}` | a pull output and its zone `Stream <name>`; renderers, browsers or scripts fetch `GET /v1/outputs/pull/{name}/stream.wav` (live WAV of the zone's post-DSP output; the listener paces the zone; 408 when nothing plays) |
| GET | `/v1/zones` | | `ZoneState[]` |
| POST | `/v1/zones` | `{name, output}` or `{name, outputs: [leader, …]}` for a group | the new `ZoneState`; `now_playing.members` carries per-output drift and `slipped_frames` (net frames duplicated/dropped to stay in time) for groups |
| GET / DELETE | `/v1/zones/{id}` | | `ZoneState` / 204 |
| GET | `/v1/zones/{id}/queue` | | `QueueItem[]` |
| POST | `/v1/zones/{id}/queue` | `{track_ids?, items?, at: end\|next, replace?, play?}` | `{ids}` |
| DELETE | `/v1/zones/{id}/queue` | | 204 (stops and clears) |
| DELETE / PUT | `/v1/zones/{id}/queue/{item}` | PUT: `{to_index}` | 204 |
| POST | `/v1/zones/{id}/transport` | `{action: play\|pause\|toggle\|stop\|next\|previous\|seek, index?, position_ms?}` | `ZoneState` |
| POST | `/v1/zones/{id}/volume` | `{volume: {db: -20} \| {percent: 50} \| "up" \| "down" \| {mute: true}}` | 204 |
| POST | `/v1/zones/{id}/shuffle` | | 204; shuffles the items after the current one (all items when nothing plays) |
| PUT | `/v1/zones/{id}/radio` | `{on}` | library radio: when the queue ends (repeat off), the node appends up to 10 tracks similar to the last one (same genres, then same artists, then anything) and keeps playing; the flag is in `ZoneState.radio` and survives restarts |
| PUT | `/v1/zones/{id}/repeat` | `{repeat: off\|all\|one}` | 204 |
| GET / PUT / DELETE | `/v1/zones/{id}/dsp` | PUT: `PipelineConfig` `{headroom_db?, volume_db?, dither, dsd_mode, resample, loudness: off\|track\|album, eq: [{kind, freq_hz, gain_db, q, enabled}], convolution?: {path, gain_db, enabled}, crossfeed?: {level_db, cutoff_hz, delay_us, enabled}}` | per-output DSP, persisted; applies at the next track start; DELETE restores the default |
| GET | `/v1/library/stats` | | `LibraryStats` |
| GET | `/v1/library/tracks` | `?after=<id>&limit=` | `{tracks}` |
| GET | `/v1/library/tracks/{id}` | | `Track` |
| GET | `/v1/library/search` | `?q=&limit=` | `{tracks, albums, artists, genres}`: tracks by full text (title, artist, album, genre words), albums by title, artists and genres by name |
| GET | `/v1/library/artists` | | `ArtistSummary[]` (id, name, mbid, album and track counts) |
| GET | `/v1/library/artists/{id}` | | `{artist, albums}` |
| GET | `/v1/library/artists/{id}/tracks` | | `{tracks}` |
| GET | `/v1/library/artists/{id}/info` | | `{bio?, image, wikipedia?, wikidata?, musicbrainz?, homepage?, note?}` from MusicBrainz → Wikidata → Wikipedia by the artist's MBID, fetched once and cached for 30 days (misses 7) |
| GET | `/v1/library/artists/{id}/image` | | the cached portrait (Wikimedia Commons); 404 when none |
| GET | `/v1/library/genres` | | `[{id, name, tracks, albums}]` from the files' genre tags (several per track when the tag lists several) |
| GET | `/v1/library/genres/{id}` | | `{genre, albums, tracks}` |
| GET | `/v1/library/albums` | `?after=&limit=` | `Album[]` |
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
| POST | `/v1/playlists/import` | `{name, description?, tracks: [{title, artist?, album?, uri?, path?}]}` | `{id, name, parsed, imported, unresolved}`; items resolve through the library (canonical URL, exact path, file name, title + artist) to canonical URLs |
| POST | `/v1/playlists/import/file` | `?file=<name>&name=&format=` raw body: M3U/M3U8, CSV (Exportify, Soundiiz, Roon export saved as CSV) or Roon's Excel export (.xlsx) | same report; 422 when nothing matched |
| GET / POST | `/v1/activity` | POST: `{track: {title, artist?, album?, uri?}, source, duration_ms?}` | plays as private activity posts; 422 when nothing canonical resolves |
| GET | `/v1/discover/uri` | `?uri=` / `?track_id=` / `?album_id=` / `?artist_id=` | `{uri, indexed, tags: [[label, people]], taggers}`: what Pubky (Nexus) knows about one object; 412 without a `contact` |
| GET | `/v1/discover/trends` | `?window=7d&limit=` | Pinakes trending music, each item with `in_library` (the matching track); 412 without a valid `contact` |
| GET | `/v1/discover/tag/{label}` | `?limit=` | music tagged `label` |
| GET | `/v1/discover/friends/{viewer}` | `?limit=` | music the people a pubky follows tagged or posted |
| GET | `/v1/sources` | | streaming sources the node can search and play: `[{id, name, description, status}]` (`archive` = Internet Archive, `audius` = Audius; subscription services plug in here) |
| GET | `/v1/sources/{source}/search` | `?q=&limit=` | `{source, items: [{id, title, creator?, year?, downloads?, image?, kind: item\|track, url}]}` |
| GET | `/v1/sources/{source}/items/{id}` | | `{source, id, title, creator?, year?, description?, licence?, image?, url, tracks: [{title, track_no?, duration_ms?, format, url, artist?}]}`; play by enqueueing the track URLs (the archive gives one best-format file per recording, FLAC first; Audius one MP3 stream per track) |
| GET | `/v1/radio/search` | `?q=&tag=&country=&limit=` | `{stations: [{id, name, url, homepage?, favicon?, codec?, bitrate?, country?, tags}]}` from radio-browser.info, most voted first; 400 without a term, 502 when the directory fails |
| GET / PUT | `/v1/radio/stations` | PUT: a `Station` | saved stations (node-local settings); PUT adds or updates by id/url |
| DELETE | `/v1/radio/stations/{id}` | | remaining stations; 404 when unknown |
| POST | `/v1/radio/play` | `{zone, station}` | replaces the zone's queue with the stream and plays it |
| GET | `/v1/pubky` | | `{user, unsynced}` |
| POST / GET | `/v1/pubky/login` | | POST starts a Pubky Ring sign-in and returns `{url}` (`pubkyauth://…`); GET polls `{status: none\|pending\|signed_in, user?, url?}`. On approval the node stores the session secret, publishes its record and syncs |
| POST | `/v1/pubky/sync` | | `{pushed, pulled, deleted, errors}`; 412 without a session |
| GET / POST | `/v1/pubky/files` | POST: `{track_id}` or `{path}` | private files on the homeserver (`/priv/pubky.music/files/`); upload a node file |
| POST | `/v1/pubky/files/mirror` | | download missing files into the node's mirror folder and scan it: `{fetched, present, bytes, errors}` |
| GET | `/v1/events` | WebSocket | first `{"type":"snapshot","zones":[...]}`, then `zone` (full state), `zone_event` (track started/ended, skipped, paused, resumed, seeked, stopped, queue, `stream_title` for radio, error), `zone_removed`, `library_changed` |

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
canonical identity), `scrobble` (private activity), `startScan`, `getScanStatus`, plus empty
answers for stars, genres, lyrics and info calls. Ids: songs are track ids, albums `al-<id>`,
artists `ar-<id>`, playlists `pl-<id>`.
