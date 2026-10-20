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
| GET | `/v1/library/folders` | | `[path, present][]` |
| POST | `/v1/library/scan` | `{folders?}` | `{folders}`; 409 while a scan runs |
| GET / POST | `/v1/playlists` | POST: `{name, description?, image?, uris?, track_ids?}` | `PlaylistSummary[]` / `PlaylistDetail` |
| GET / PUT / DELETE | `/v1/playlists/{id}` | PUT: `{name?, description?, image?, uris?}` | `PlaylistDetail` (items + resolved library tracks) |
| POST | `/v1/playlists/{id}/items` | `{uris?, track_ids?, at?}` | `PlaylistDetail`; 422 for a track without a canonical identity |
| DELETE | `/v1/playlists/{id}/items/{index}` | | `PlaylistDetail` |
| POST | `/v1/playlists/{id}/play` | `{zone, replace?}` | `{queued, missing}` |
| GET | `/v1/pubky` | | `{user, unsynced}` |
| POST | `/v1/pubky/sync` | | `{pushed, pulled, deleted, errors}`; 412 without a session |
| GET | `/v1/events` | WebSocket | first `{"type":"snapshot","zones":[...]}`, then `zone` (full state), `zone_event`, `zone_removed`, `library_changed` |

Playlists are v1 `PubkyAppPost` envelopes (ADR 0006) stored in the node and pushed to the user's private
homeserver folder when `pubky.session_secret_file` is set. Items are canonical music URLs only.

Queue item URIs: `track:<library id>`, a local path, or `file://`. Streaming services add schemes later.
Every `ZoneState` carries `now_playing.signal_path`, so a controller can render the chain for any zone.
Types live in `pm_api::types`; a Rust client reuses them.
