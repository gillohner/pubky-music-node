# Native API (`/v1`)

HTTP + WebSocket served by `pm-node serve` on `[api] listen` (default `127.0.0.1:7788`).
Set `[api] token` to require `Authorization: Bearer <token>` (or `?token=` on the WebSocket).
Without a token, keep the listener on localhost. Remote access by pubkey (M5) wraps this API.

| Method | Path | Body / query | Returns |
| :-- | :-- | :-- | :-- |
| GET | `/v1/info` | | name, version, library stats, zone/output counts |
| GET | `/v1/outputs` | | `OutputInfo[]` (id, name, caps) |
| GET | `/v1/zones` | | `ZoneState[]` |
| POST | `/v1/zones` | `{name, output}` | the new `ZoneState` |
| GET / DELETE | `/v1/zones/{id}` | | `ZoneState` / 204 |
| GET | `/v1/zones/{id}/queue` | | `QueueItem[]` |
| POST | `/v1/zones/{id}/queue` | `{track_ids?, items?, at: end\|next, replace?, play?}` | `{ids}` |
| DELETE | `/v1/zones/{id}/queue` | | 204 (stops and clears) |
| DELETE / PUT | `/v1/zones/{id}/queue/{item}` | PUT: `{to_index}` | 204 |
| POST | `/v1/zones/{id}/transport` | `{action: play\|pause\|toggle\|stop\|next\|previous\|seek, index?, position_ms?}` | `ZoneState` |
| POST | `/v1/zones/{id}/volume` | `{volume: {db: -20} \| {percent: 50} \| "up" \| "down" \| {mute: true}}` | 204 |
| PUT | `/v1/zones/{id}/repeat` | `{repeat: off\|all\|one}` | 204 |
| GET | `/v1/library/stats` | | `LibraryStats` |
| GET | `/v1/library/tracks` | `?after=<id>&limit=` | `{tracks}` |
| GET | `/v1/library/tracks/{id}` | | `Track` |
| GET | `/v1/library/search` | `?q=&limit=` | `{tracks}` (FTS5) |
| GET | `/v1/library/albums` | `?after=&limit=` | `Album[]` |
| GET | `/v1/library/albums/{id}/tracks` | | `{tracks}` |
| GET | `/v1/library/albums/{id}/cover` | | image bytes (embedded picture extracted at scan, or `cover.jpg` next to the files); 404 when none |
| GET | `/v1/library/folders` | | `[path, present][]` |
| POST | `/v1/library/scan` | `{folders?}` | `{folders}`; 409 while a scan runs. With `[library] watch = true` (default) the node also rescans by itself after `watch_quiet_secs` without file changes |
| GET / POST | `/v1/playlists` | POST: `{name, description?, image?, uris?, track_ids?}` | `PlaylistSummary[]` / `PlaylistDetail` |
| GET / PUT / DELETE | `/v1/playlists/{id}` | PUT: `{name?, description?, image?, uris?}` | `PlaylistDetail` (items + resolved library tracks) |
| POST | `/v1/playlists/{id}/items` | `{uris?, track_ids?, at?}` | `PlaylistDetail`; 422 for a track without a canonical identity |
| DELETE | `/v1/playlists/{id}/items/{index}` | | `PlaylistDetail` |
| POST | `/v1/playlists/{id}/play` | `{zone, replace?}` | `{queued, missing}` |
| POST | `/v1/playlists/import` | `{name, description?, tracks: [{title, artist?, album?, uri?}]}` | `{id, imported, unresolved}`; items resolve through the library to canonical URLs |
| GET / POST | `/v1/activity` | POST: `{track: {title, artist?, album?, uri?}, source, duration_ms?}` | plays as private activity posts; 422 when nothing canonical resolves |
| GET | `/v1/pubky` | | `{user, unsynced}` |
| POST | `/v1/pubky/sync` | | `{pushed, pulled, deleted, errors}`; 412 without a session |
| GET / POST | `/v1/pubky/files` | POST: `{track_id}` or `{path}` | private files on the homeserver (`/priv/pubky.music/files/`); upload a node file |
| POST | `/v1/pubky/files/mirror` | | download missing files into the node's mirror folder and scan it: `{fetched, present, bytes, errors}` |
| GET | `/v1/events` | WebSocket | first `{"type":"snapshot","zones":[...]}`, then `zone` (full state), `zone_event`, `zone_removed`, `library_changed` |

Playlists are v1 `PubkyAppPost` envelopes (ADR 0006) stored in the node and pushed to the user's private
homeserver folder when `pubky.session_secret_file` is set. Items are canonical music URLs only.

Queue item URIs: `track:<library id>`, a local path, or `file://`. Streaming services add schemes later.
Every `ZoneState` carries `now_playing.signal_path`, so a controller can render the chain for any zone.
Types live in `pm_api::types`; a Rust client reuses them.

## OpenSubsonic (`/rest/*`)

The same server also speaks the Subsonic API (JSON, `f=json`) for existing clients: Symfonium,
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
