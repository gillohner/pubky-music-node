# Native API (`/v1`)

The node also serves a built-in browser controller at `/` (one static page over this API: browse,
search, play to any zone or in the browser itself, queue, playlists, discover, settings).

HTTP + WebSocket served by `pm-node serve` on `[api] listen` (default `127.0.0.1:7788`).
Set `[api] token` to require `Authorization: Bearer <token>` (or `?token=` on the WebSocket).
Without a token, keep the listener on localhost. Remote access by pubkey (M5) wraps this API.

| Method | Path | Body / query | Returns |
| :-- | :-- | :-- | :-- |
| GET / PUT | `/v1/node/settings` (`jukebox_zone` picks the zone Subsonic jukebox calls drive; empty clears it) | PUT: `{contact?, jukebox_zone?, copy_rate?, dev_outputs?}` | `{contact, discovery, jukebox_zone, copy_rate, dev_outputs}` (`dev_outputs`: show the Null output in the zone lists; also on with `PM_DEV_OUTPUTS=1`); the contact (email or URL, never a code-host link) is saved in the library and turns Pinakes/Nexus discovery on without a restart; 400 when invalid |
| GET | `/v1/info` | | name, version, library stats, zone/output counts, `scanning`, `last_scan` (`{finished_at, added, updated, removed, error_count, errors: [path, why][]}`, the first 20 files it could not read) |
| GET | `/v1/outputs` | | `OutputInfo[]` (id, name, caps) |
| GET | `/v1/outputs/discovery` | | `{upnp: [{from, location, server?, friendly_name?, renderer, output}], cast: [{id, friendly_name, addr}], public_url, local_address?, notes, upnp_enabled, cast_enabled, saved_addresses}`: a 3 s network scan (`?quick=1` skips the scan and only reports configuration) with plain-language findings (no multicast answers, unusable public URL, devices that are not renderers) |
| GET | `/mb/ws/2/{path}?…` | | no token: a MusicBrainz `/ws/2` caching proxy (one upstream request per second, answers cached a week, the node's contact in the User-Agent); for Pinakes's `[also_on.sources.musicbrainz] base_url = "https://<node>/mb/ws/2"`; 412 until the node has a contact |
| POST | `/v1/node/settings/backup` | | write the owner's settings (contact, folders, TIDAL session, renderers, jukebox zone, DSP) to `/priv/pubky.music/settings.json`; also done after every sync |
| POST | `/v1/node/settings/restore` | | bring them back from the homeserver onto this node (missing values only); done by itself on a fresh node after the first sign-in |
| POST | `/v1/auth/pubky/start` | | no token needed: starts a Pubky Ring sign-in for a controller or browser; `{id, url, owner}`; 412 until the node itself has a Pubky session |
| GET | `/v1/auth/pubky/poll` | `?id=` | `{status: pending\|ok\|denied\|expired, token?, user?, role?}`: `ok` carries the API token when the approver is the node's owner (`role: owner`), or a personal token for someone in People with music access (`role: listener`); anyone else is `denied` |
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
| POST | `/v1/zones/{id}/announce` | `{uri?, chime?, volume_percent?, title?}` | Play a clip (an http(s) URL or a file path; TTS from Home Assistant) instead of the music, after an optional chime, at its own volume on outputs with hardware volume; then back to the same track and position, playing or paused as before, the queue and volume as they were: `{resumes, volume_applied, restores_volume?, items}`; 409 while one is playing |
| POST | `/v1/zones/{id}/volume` (absolute dB requests are capped at the zone's `max_volume_db` from its DSP settings) | `{volume: {db: -20} \| {percent: 50} \| "up" \| "down" \| {mute: true}}` | 204 |
| GET / PUT | `/v1/zones/{id}/sleep` | PUT: `{seconds}` or `{seconds: null}` | `{ends_at_ms?, remaining_ms?}`; sleep timer: the zone pauses when it fires; one timer per zone, a new PUT replaces it |
| POST | `/v1/zones/{id}/transfer` | `{to}` | the target's `ZoneState`; moves the queue, the playing item and its position, repeat and radio to zone `to` (paused stays paused); the source ends up stopped and empty; 400 for an empty queue or the same zone |
| POST | `/v1/zones/{id}/shuffle` | | 204; shuffles the items after the current one (all items when nothing plays) |
| PUT | `/v1/zones/{id}/radio` | `{on}` | library radio: when the queue ends (repeat off), the node appends up to 10 tracks similar to the last one (same genres, then same artists, then anything) and keeps playing; the flag is in `ZoneState.radio` and survives restarts |
| PUT | `/v1/zones/{id}/crossfade` | `{seconds}` (0–12; 0 = off) | fade each track into the next on this zone (equal-power). Songs of the same album following each other stay gapless; no fade across a format change or into a track shorter than twice the fade. In `ZoneState.crossfade_ms`, kept across restarts; while it fades the signal path shows a `crossfade` stage. Owner only |
| PUT | `/v1/zones/{id}/repeat` | `{repeat: off\|all\|one}` | 204 |
| GET / PUT / DELETE | `/v1/zones/{id}/dsp` | PUT: `PipelineConfig` `{headroom_db?, volume_db?, max_volume_db? (volume limit; caps volume_db and every volume request), dither, dsd_mode, resample, loudness: off\|track\|album, eq: [{kind, freq_hz, gain_db, q, enabled}], convolution?: {path, gain_db, enabled}, crossfeed?: {level_db, cutoff_hz, delay_us, enabled}}` | per-output DSP, persisted; applies at the next track start; DELETE restores the default |
| GET | `/v1/library/stats` | | `LibraryStats` |
| GET | `/v1/library/tracks` | `?after=<id>&limit=` | `{tracks}` |
| GET | `/v1/backup` | | a backup file without sign-ins (zip: `manifest.json`, `library.db`), attachment `pubky-music-<date>-<time>.pmbackup` |
| POST | `/v1/backup` | header `x-backup-passphrase` | the same with the sign-ins, Pubky session, remote key and config encrypted under the passphrase (`secrets.bin`, Argon2id + AES-256-GCM) |
| POST | `/v1/restore` | the file; `?check=true`; `?skip_secrets=true`; header `x-backup-passphrase` | `check`: `{from_node, made_at, counts, locked_signins, left_out, folders}` without changing anything. Otherwise restores while the node runs and rescans: `{expected, restored, signed_in, sign_in_again, missing_folders, rescan_started, notes}`. 400 for a wrong or missing passphrase, 422 for a file that is not a backup, 409 during a scan |
| GET, PUT | `/v1/backup/schedule` | `{folder, every_hours?, keep?, passphrase?}` | scheduled backups to a folder on the node, the newest `keep` kept; `{folder, every_hours, keep, with_signins, last_at, last_file, last_error, files}`; an empty folder turns it off; `passphrase: ""` forgets it |
| POST | `/v1/backup/schedule/run` | | one scheduled backup now |
| GET | `/v1/library/tracks/{id}/lyrics` | | `{track_id, source: lrc\|tag, synced, text, lines: [{ms?, text}]}` from the file's tags or a sidecar `.lrc`; 404 when none |
| GET | `/v1/library/tracks/{id}` | | `Track` |
| GET | `/v1/library/tracks/{id}/download` | `?format=aac\|mp3\|opus&bitrate=64–320` | a download for a phone or laptop (STEWARD-DOWNLOADS.md). The original by default, or a transcode (AAC in M4A, MP3, Opus in Ogg; default 256 kbit/s, Opus 160). Resumable: `Range` (one range or a suffix), `If-Range` on the `ETag`, `If-None-Match` gives 304, 416 past the end. `Content-Disposition: attachment` with the file name. Listeners may download what they may play. Files not on the node's disk (a WebDAV share, a drive) and every transcode are written once to `<data>/downloads/` (capped by the `downloads.cache_mb` setting, default 2048, oldest out first). Transcodes need ffmpeg (`downloads.ffmpeg` setting, else on the PATH): 501 without it, 502 when it fails |
| GET | `/v1/library/search` | `?q=&limit=&sources=1` | `{tracks, albums, artists, genres, sources: [{source, items}]}`: tracks by full text (title, artist, album, genre words), albums by title, artists and genres by name; `sources` carries hits from streaming services that take part (TIDAL when configured; `sources=0` skips them), each hit flagged with `library_track_id` / `library_album_id` when the library has the same ISRC or UPC |
| GET | `/v1/library/albums/{id}/versions`, `/v1/library/tracks/{id}/versions` | | `[{source, id, label, url, uri?}]`: the same album (by UPC) or track (by ISRC) on TIDAL, cached a week; empty when unknown |
| GET | `/v1/sources/tidal/stream/{id}` | | the track's HLS segments as one `audio/mp4` body for browsers (`X-Tidal-Presentation: FULL\|PREVIEW`); sign-in required |
| GET | `/v1/sources/spotify/login` | | Spotify sign-in URL (Authorization Code + PKCE); the app must list `<public_url>/spotify/callback` |
| GET | `/v1/sources/spotify/status` | | `{configured, signed_in, user_id?, display_name?, country?, product?, redirect_uri, requested_scopes, app_from?, client_id?}`; `redirect_uri` is what the app must list (the return page for an app without its secret), `app_from` is `config` or `settings` |
| PUT | `/v1/sources/spotify/app` | `{client_id}` | The guided setup: your Spotify app's client ID (32 hex digits; empty forgets it). A new app signs out of Spotify. 409 when `[spotify]` is set in the node config. Returns the status |
| DELETE | `/v1/sources/spotify/session` | | sign out |
| GET | `/v1/sources/spotify/collection` | | saved albums and tracks as hits (ISRC/UPC, library matches) |
| POST | `/v1/sources/spotify/collection/import` | | saved tracks (by ISRC) and albums as favourites |
| GET | `/v1/sources/spotify/playlists` | | the user's playlists |
| POST | `/v1/sources/spotify/playlists/{id}/import` | `?keep_source=true` | a private Pubky playlist by ISRC; with `keep_source` each song is pinned to its Spotify track (ADR 0015) |
| POST | `/v1/sources/tidal/playlists/{id}/import` | `?keep_source=true` | the same for TIDAL |
| GET / PUT / DELETE | `/v1/sources/plex/server` | PUT: `{url, token}` | `{connected, url, server_name, version, user}`, never the token; PUT checks the server (`/identity`, then the token on `/`): 401 when Plex refuses the token; DELETE forgets it |
| POST | `/v1/sources/plex/pin` | | "Sign in with Plex": `{id, code, auth_url}`; open `auth_url` (app.plex.tv) and approve |
| GET | `/v1/sources/plex/pin/{id}` | | `{status: waiting\|connected\|expired, server}`; once approved the node picks your server (your own first) and the first connection that answers (local, then remote, then relay), and keeps that server's token only |
| GET | `/v1/sources/plex/search`, `/v1/sources/plex/items/{track:<id>\|album:<id>}` | | Plex as a source; songs queue as `plex:track:<ratingKey>` |
| GET | `/v1/sources/plex/stream/{id}`, `/v1/sources/plex/cover/{id}/{version}` | | the original file and the cover, relayed so the token stays in the node |
| GET / PUT / DELETE | `/v1/sources/audiobookshelf/server` | PUT: `{url, api_key}` or `{url, user, password}` | `{connected, url, user, version}`, never the key. With a password, an admin's sign-in makes the node its own API key ("Pubky Music node") and ends the session; other users get 403 (only admins make keys). 401 when refused. DELETE forgets the server and deletes the key the node made |
| GET | `/v1/sources/audiobookshelf/search`, `/v1/sources/audiobookshelf/items/book:<id>` | | audiobooks from every book library; a book's tracks are its files (`audiobookshelf:file:<item>/<ino>`), titled by chapter |
| POST | `/v1/sources/audiobookshelf/play` | `{zone, item, restart?}` | `{ids, index, position_ms}`: the whole book into the zone from Audiobookshelf's saved position. While a zone plays a book, the node reports the position back every 10 s |
| GET | `/v1/sources/audiobookshelf/continue` | | `{items}`: the books you are in the middle of |
| GET | `/v1/sources/audiobookshelf/stream/{item}/{ino}`, `/v1/sources/audiobookshelf/cover/{item}` | | the original file and the cover, relayed so the key stays in the node |
| GET | `/v1/sources/spotify/devices` | | Spotify Connect devices |
| POST | `/v1/sources/spotify/play` | `{uris, device_id?}` | start `track:<id>` / `album:<id>` on a device (Premium); no audio enters the node |
| GET | `/v1/sources/deezer/stream/{id}` | | 307 to the track's 30 s preview (browsers); zones play `deezer:track:<id>` directly |
| GET | `/v1/library/artists` | | `ArtistSummary[]` (id, name, mbid, album and track counts). Songs without any artist come last as "Unknown artist" with id `0`; its page is `/v1/item/artist/lib-0` (every such song). Subsonic and the other lists leave it out |
| GET | `/v1/library/artists/{id}` | | `{artist, albums}` |
| GET | `/v1/library/artists/{id}/tracks` | | `{tracks}` |
| GET | `/v1/library/artists/{id}/similar` | | `{artists: [{name, mbid?, url, score, library_artist_id?}], note?}` from Last.fm's `artist.getSimilar` (by MusicBrainz id, else by name), each matched to this library; `note` says why there is nothing (`not configured` without a Last.fm app) |
| GET / PUT / DELETE | `/v1/lastfm` | PUT `{api_key?, api_secret?, submit?}` | Last.fm: `{configured, app_from?, api_key?, connected, user?, submit, pending, problem?}`; the owner's app (both values; an empty key forgets it and signs out), and whether plays go out; DELETE signs out |
| GET | `/v1/lastfm/login` | | `{url}`: Last.fm's permission page, which comes back to `/lastfm/callback` (no token there), where the node turns the token into a session key and redirects to `/?lastfm=signed_in` |
| GET | `/v1/library/artists/{id}/info` | | `{bio?, image, wikipedia?, wikidata?, musicbrainz?, homepage?, image_from?, bio_from?, note?}` from MusicBrainz → Wikidata → Wikipedia, then fanart.tv (the project's key, `PM_PROJECT_FANART_KEY` at build time) and TheAudioDB (`PM_PROJECT_AUDIODB_KEY`, else its public test key) for a portrait and a biography the others lack (`image_from`, `bio_from` say so) → Wikipedia by the artist's MBID, fetched once and cached for 30 days (misses 7) |
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
| GET | `/v1/library/folders/suggest` | | where the music probably is, for a new install: `[{path, audio_files, more}]` for the usual places (the home Music folder, `/music`, drives under `/media`, `/mnt`, `/Volumes`) that hold playable files and are not in the library yet; `more`: at least that many |
| POST | `/v1/library/folders/move` | `{from, to}` | the music under `from` now lives under `to` (a restore on another machine, a new disk): files keep their tracks, so matches, playlists, history and tags stay; a rescan follows; `{folders, moved_files, scan_started}` |
| POST | `/v1/library/scan` | `{folders?}` | `{folders}`; 409 while a scan runs. With `[library] watch = true` (default) the node also rescans by itself after `watch_quiet_secs` without file changes |
| GET / POST | `/v1/playlists` | POST: `{name, description?, image?, uris?, track_ids?}` | `PlaylistSummary[]` / `PlaylistDetail` |
| GET / PUT / DELETE | `/v1/playlists/{id}` | PUT: `{name?, description?, image?, uris?}` | `PlaylistDetail` (items + resolved library tracks) |
| POST | `/v1/playlists/{id}/items` | `{uris?, track_ids?, at?}` | `PlaylistDetail`; 422 for a track without a canonical identity |
| DELETE | `/v1/playlists/{id}/items/{index}` | | `PlaylistDetail` |
| DELETE | `/v1/playlists/{id}/local/{index}` | | take a node-only song out (`PlaylistDetail.local_tracks`: library songs without a MusicBrainz id or ISRC, kept after the items on this node only, never synced; they join the items once identified) |
| POST | `/v1/playlists/{id}/play` (entries that are Internet Archive file URLs stream directly; other unresolved entries are reported as `missing`) | `{zone, replace?}` | `{queued, missing}` |
| GET | `/v1/playlists/{id}/play-from` | | `{order: {order, services}, own_order, items: [{index, uri, title, artist, plays_from, pin, pin_unavailable, sources: [{service, name, url, label, mode, available, chosen}], reason, connect, track_id, stream_uri}]}`: where each song plays from (ADR 0015) |
| PUT | `/v1/playlists/{id}/items/{index}/source` | `{service, uri?}`, or `{service: null}` for Auto | the playlist; pins the song to a source |
| POST | `/v1/pubky/files/upload` | `{album_id \| track_ids \| all, dry_run?}` | a copy job to the homeserver's private storage (files already there are skipped); with `dry_run` only the totals |
| GET | `/v1/pubky/files/upload/{id}` | | `{total, done, skipped, failed, bytes_total, bytes_done, current, finished, errors}` |
| GET | `/v1/shared/resolve?link=` | | `{user, id?}` from a pasted `pubky://` link or key |
| GET | `/v1/shared/{user}/playlists` | | someone's shared playlists `[{user, id, link, name, description, items, updated_at}]` (no session needed) |
| GET | `/v1/shared/{user}/playlists/{id}` | | `{summary, play_from}`: the playlist and where each song plays from on this node |
| POST | `/v1/shared/{user}/playlists/{id}/copy` | | a private copy on this node (pins and order kept) |
| PUT | `/v1/playlists/{id}/visibility` | `{public: bool}` | the playlist; shared ones are published under `/pub/social/v1/posts/` on the next sync, and making it private removes the public copy |
| PUT | `/v1/playlists/{id}/order` | `{order: [service…]}` or `{order: null}` | the playlist; its own play order, or the node's |
| GET/PUT | `/v1/play-order` | `{order: [service…]}` | `{order, services: [{id, name, connected, note}]}`: the node's global play order (`library`, `tidal`, `spotify`) |
| GET | `/v1/favourites` | | `{tracks, albums, artists, other: [url], uris: [url]}`: hearts joined to the library (ADR 0010) |
| PUT | `/v1/favourites` | `{uri}` or `{track_id}` / `{album_id}` / `{artist_id}` | `{uri, favourite: true}`. A library song without a MusicBrainz id or ISRC is a favourite on this node only (`uri` is `track:<id>`; never synced) until it's identified. An album without a MusicBrainz release id or an artist without one is kept the same way (`album:<id>`, `artist:<id>`) and published once MusicBrainz knows it |
| DELETE | `/v1/favourites` | `?uri=` or `?track_id=` / `?album_id=` / `?artist_id=` | `{uri, favourite: false}` |
| GET | `/v1/tags` | none, or `?uri=` / `?track_id=` / `?album_id=` / `?artist_id=` | `{uri?, tags: [{uri, label, created_at, synced}]}`: my tags (ADR 0011) |
| PUT | `/v1/tags` | `{uri|track_id|album_id|artist_id, label}` | `{uri, label, tagged: true}`; label trimmed and lowercased, 1–20 chars; 422 without a canonical identity |
| DELETE | `/v1/tags` | `?…&label=` | `{uri, label, tagged: false}` |
| GET | `/v1/reviews` | none, or `?uri=` / `?track_id=` / `?album_id=` / `?artist_id=` | `{uri?, reviews: [{id, uri, rating?, text, created_at, updated_at, synced}]}`: my private reviews (v1 `pubky.music/review` posts) |
| PUT | `/v1/reviews` | `{uri|track_id|album_id|artist_id, rating?: 1–10, text?}` (rating in half stars: 7 is three and a half) | creates or replaces my review of that object (same post id); 400 with neither rating nor text, 422 without a canonical identity |
| DELETE | `/v1/reviews` | `?…` | removes it (tombstoned until the delete is pushed) |
| POST | `/v1/playlists/from-queue` | `{zone?, uris?, name?}` | `{playlist: PlaylistDetail, skipped: [label]}`; a new private playlist from a zone's queue (or the given queue URIs: `track:<id>`, local paths, canonical URLs); entries without a canonical identity are listed in `skipped`; 422 when nothing qualifies |
| POST | `/v1/playlists/import` | `{name, description?, tracks: [{title, artist?, album?, uri?, path?}]}` | `{id, name, parsed, imported, unresolved}`; items resolve through the library (canonical URL, exact path, file name, title + artist) to canonical URLs |
| POST | `/v1/import/roon` | `?file=&dry_run=true&favourites=true&playlist=true&tags=false`, the Roon export (xlsx or csv) as the body | `{kind, name, rows, matched, unmatched, not_identified, favourites, tags_added, tags_skipped, tags_found, misses: [{what, reason}], playlist?, dry_run}` |
| POST | `/v1/playlists/import/file` | `?file=<name>&name=&format=` raw body: M3U/M3U8, CSV (Exportify, Soundiiz, Roon export saved as CSV) or Roon's Excel export (.xlsx) | same report; 422 when nothing matched |
| GET / POST | `/v1/activity` | POST: `{track: {title, artist?, album?, uri?}, source, duration_ms?}` | plays as private activity posts; 422 when nothing canonical resolves |
| GET | `/v1/discover/uri` | `?uri=` / `?track_id=` / `?album_id=` / `?artist_id=` | `{uri, indexed, tags: [[label, people]], taggers}`: what Pubky (Nexus) knows about one object; 412 without a `contact` |
| GET | `/v1/discover/charts` | `?type=tracks|albums|artists&window=24h|7d|30d&limit=` | Pinakes chart, one song across services one row; items carry `kind`, `site`, `group_size`, `playable`, `also_on` |
| GET | `/v1/discover/search` | `?q=&type=&limit=` | Pinakes text search over music tagged on Pubky, joined to the library |
| GET | `/v1/discover/item` | `?uri=` | any link, tagged or not (unknown links are read by Pinakes on demand; `reading: true` with the provider's data meanwhile): one card: facts (`mbid`, `isrcs`, `upc`, `duration_secs`, albums), every `also_on` link, `on_pubky` tags/taggers, `in_library_album` |
| GET | `/v1/search` | `?q&in&limit` | one search, streamed as NDJSON: one `{source, status: ok\|error\|timeout, ms, error?, hits[{kind, title, subtitle, image, id, page, uri, track_id, artist_id, album, album_id, duration_ms, year, keys}], counts, cached}` line per source as it answers; `in` is `everywhere` (default), `library`, `pubky` or a service id; header `x-pm-sources` lists the sources asked |
| GET | `/v1/search/tags` | `?prefix=` | tag labels as you type: `{mine: [{label, count}], others: [label]}` (yours from the node, others from Nexus). A `#tag` term in `/v1/search` searches tags only (the library by your tags, Pubky by everyone's); other terms also ask Pubky by tag, and such hits carry `tagged` |
| GET | `/v1/home/{section}` | `?window&limit` | one Home section (`trending-songs`, `trending-albums`, `trending-artists`, `following`, `tags`, `concerts`, `podcasts`, `yours`, `new`; `tag?labels=a,b`): `{section, items, tags, note?, needs?: pubky\|location\|contact, error?}`; cached 60 s |
| GET / PUT | `/v1/home/layout` | PUT: `{order, hidden}` (row ids) | which Home rows show and in which order; kept on the node and in the homeserver settings copy |
| GET / POST | `/v1/smart` | POST: `{name, rules: [{field, op, value, to?}], matches: all\|any, sort, limit}` | smart playlists; `field` is `genre`, `artist`, `year`, `favourite`, `plays`, `last_played` or `tag` (your Pubky tags); `sort` is `random`, `most_played`, `least_recent`, `newest` or `title` |
| | (smart playlist rules may also carry `seeds: [{kind: track\|artist\|album\|genre, id, label?}]` (the pool is then what the library finds similar to them), `minutes` (stop at that length, after sorting) and `exclude: {artists?, genres?, tracks?, played_within_days?}`) | | |
| POST | `/v1/smart/preview` | the rules | `{matched, tracks}` without saving |
| PUT / DELETE | `/v1/smart/{id}` | PUT: as POST | change or remove one |
| GET | `/v1/smart/{id}/tracks` | | `{playlist, matched, tracks}`: its songs right now |
| GET/PUT | `/v1/home/location` | `{city}` or `{lat, lon}`; `{}` clears | the city for concerts (looked up once on OpenStreetMap) |
| GET | `/v1/item/{kind}/{id}` | | one item page (`song`, `album`, `artist`). A page is one instance: the library file, or a service's track, album or artist. `id` is `lib-<n>`, `tidal-<n>`, `deezer-<n>`, `spotify-<id>`, `mb-<mbid>`, `isrc-<ISRC>` or `u-<base64url of the link>`; a generic `isrc-`/`mb-` song id opens the best instance by the play-from order and says so in `opened_as`. Answer: `{kind, id, uri, target, title, artist{name,id}, album{name,id}, year, image, source, plays_from, connect, reading, songs[{no,title,artist,artist_id,album,album_id,duration_ms,uri,track_id,page,id}], total_ms, albums[{id,title,year,image}], also_on, versions, opened_as, song_uri, link, on_pubky, about[{label,value,href}], blurb, open_on, section}`. `also_on` is the same recording on other sources, one per source, each `{source, id, label, url, uri, page}`. `versions` lists other recordings only. `song_uri` is what tags "on the song" attach to; `link` is this instance's own link, for tags on this source only. `target` is what `/v1/tags`, `/v1/reviews` and `/v1/favourites` take |
| GET | `/v1/item/{kind}/{id}/extras` | | what a page shows after its main part, asked separately so the page never waits on it: `{related, concerts}`; each outside call has 1.5 s |
| GET | `/v1/discover/contents` | `?uri=` | an item's songs (albums) or albums and top songs (artists) from the service it plays from, falling back to the page's own link, Deezer previews, then MusicBrainz; cached six hours |
| GET | `/v1/item/candidates` | `?uri&kind&title&artist&item=lib-<n>` | the same title on the connected services ("possibly the same"), for the owner to confirm: each a `Version` plus `image`, `detail` (fans, year), `evidence` ("has your album …", "same ISRC") and `likely`, best first |
| DELETE | `/v1/item/matches` | `{uri | kind+id, url}` | "Wrong match": unlink a source the owner linked |
| POST | `/v1/item/matches` | `{uri, version, image?}` or `{kind, id: "lib-<n>", version, image?}` | the owner confirms a match; stored on this node only, used for "Also on" and playback; a library artist takes the match's picture when it has none |
| GET | `/v1/discover/play` | `?uri=` | where the item plays from, resolved through the node's connected services: the library (`track_ids`), else the best service link, tagged first, TIDAL before Spotify Connect (`items` to queue, or `spotify_uris` for `POST /v1/sources/spotify/play`); albums expand to their tracks on that service. With nothing connected: `plays_from` absent, `connect` names the services that have it, `preview` a 30 s Deezer excerpt. Rows and `/discover/item` carry the same `plays_from`/`connect` without expanding |
| POST | `/v1/discover/describe` | `{uris}` | rows for music URLs outside the library (≤ 500): title, artist, image, `site`, `plays_from`, `connect`; Pinakes batch lookup, then the provider's API for that id; unknown titles stay empty, never the URL; cached a day |
| GET | `/v1/library/identify` | | `{progress: {running, total, done, identified, suggested, unknown, errors, last_error?}, counts: {total, identified, suggested, unknown, unchecked}, acoustid}` |
| POST | `/v1/library/identify` | `{retry?}` | start the identify job (fingerprint → AcoustID → folder as release → suggestions); resumes, `retry` looks again at "not found" tracks |
| GET | `/v1/library/suggestions` | | open suggestions with their track: `{id, track_id, recording_mbid, release_mbid?, title?, artist?, release_title?, score, source: fingerprint\|search, track}` |
| GET | `/v1/library/tracks/{id}/candidates` | | "Which song is this?": searches MusicBrainz by the track's tags now (with, then without its length) and lists its suggestions with MusicBrainz's title, artist and release, one per release; `{track, suggestions, needs_contact}`; pick one with the accept call below |
| POST | `/v1/library/suggestions/{id}/accept` | | the owner confirms: the track's identity becomes that recording (`confirmed_by_user`) |
| DELETE | `/v1/library/suggestions/{id}` | | not this one |
| PUT | `/v1/library/identify/settings` | `{write_ids?, acoustid_key?}` | "Write IDs into my files" on or off (off by default); your own AcoustID key (letters and digits; empty removes it and goes back to the configured one). The status says `acoustid` (a key is in use) and `acoustid_own` (it was entered here). The key goes into encrypted backups only |
| POST | `/v1/library/identify/write` | | write the IDs found so far into the files now (FLAC, WAV, AIFF; tags backed up first): `{written, skipped: [{track_id, path, reason}], backup_dir}` |
| GET, POST | `/v1/library/tag-fixes` | `?limit=`; POST `{track_ids?}` | GET: what would change per file (`{total, files: [{track_id, title, path, changes: [{key, now, then}]}], cannot}`): separate artists (`ARTISTS`, artist ids) and the IDs of confirmed matches. POST writes them (tags backed up first, audio untouched): `{written, skipped, backup_dir}` |
| GET | `/v1/discover/trends` | `?window=7d&limit=` | Pinakes trending music, each item with `in_library` (the matching track); 412 without a valid `contact` |
| GET | `/v1/discover/tag/{label}` | `?limit=` | music tagged `label` |
| GET | `/v1/discover/friends/{viewer}` | `?limit=` | music the people a pubky follows tagged or posted |
| GET | `/v1/sources` | | streaming sources the node can search and play: `[{id, name, description, status, enabled, features, stage}]`; `features` is what the source declares it can do (`search`, `play`, `connect`, `artists`, `library`, `playlists`, `downloads`, `lyrics`, `scrobble`) and the app shows only that; `stage` is `stable`, `beta` or `unofficial` (`archive` = Internet Archive, `audius` = Audius, `tidal` = TIDAL, `deezer` = Deezer catalogue with 30 s previews; hits carry `uri` when a track can be queued directly) |
| PUT | `/v1/sources/{source}/enabled` | `{enabled}` | switch a source on or off; off hides it from search, Home, item pages and the play order (its own search answers 404), its sign-in and settings are kept |
| GET | `/v1/sources/youtube/key` | | `{set}`: whether YouTube search has the owner's Data API key (the key never comes back) |
| PUT | `/v1/sources/youtube/key` | `{key}` | Keeps the key after one check call to YouTube (400 with YouTube's reason when refused); empty forgets it |
| GET | `/v1/sources/youtube/unofficial` | | YouTube on the zones through yt-dlp: `{enabled, ytdlp?: {path, from: config\|downloaded\|path, version?}, can_download, health?, warning}` |
| PUT | `/v1/sources/youtube/unofficial` | `{enabled}` | Switch it on or off |
| POST | `/v1/sources/youtube/unofficial/ytdlp` | | Download (or update) yt-dlp from its official GitHub releases, checked against SHA2-256SUMS |
| DELETE | `/v1/sources/youtube/unofficial/ytdlp` | | Remove the downloaded yt-dlp |
| GET | `/v1/sources/spotify/soloist` | | Spotify Soloist on this machine: `{supported, enabled, key_set, device_name, pipewire_device, soloist?: {path, from: config\|downloaded\|path, version?}, can_download, downloaded_at?, running, pid?, waiting_for?, paired?, now?: {status, title?, artists, album?, image?, uri?, duration_ms?, position_ms?, volume?, is_active}, health?, note}` (the key never comes back) |
| PUT | `/v1/sources/spotify/soloist` | `{enabled?, api_key?, device_name?, pipewire_device?}` | The switch (on again is the retry), the owner's key (empty forgets it), the Connect device name and PipeWire output; a change while it runs restarts it |
| POST | `/v1/sources/spotify/soloist/binary` | | Download (or update) Soloist for this machine from Spotify's CDN; checked by running `--version` |
| DELETE | `/v1/sources/spotify/soloist/binary` | | Remove the downloaded Soloist (stops it first) |
| GET / PUT | `/v1/sources/spotify/librespot` | PUT `{enabled?, device_name?, output?: local\|zone, zone_id?}` | librespot (unofficial): `{supported, enabled, device_name, output, zone_id, zones: [{id, name}], librespot?: {path, from: config\|data\|path, version?}, running, pid?, waiting_for?, user?, now?: {status, title?, artists, album?, image?, uri?, duration_ms?}, health?, warning}`; a change while it runs restarts it |
| POST | `/v1/sources/spotify/soloist/control` | `{command: play\|pause\|next\|prev}` | Through `soloist ctl`; 409 when Soloist isn't running, 502 with Soloist's words otherwise |
| GET | `/v1/sources/{source}/search` | `?q=&limit=` | `{source, items: [{id, title, creator?, year?, downloads?, image?, kind: item\|track, url}]}` |
| POST | `/v1/sources/tidal/callback` and `/v1/sources/spotify/callback` | `{url}` | The pasted-address fallback for a sign-in whose last step could not reach the node (another network, an app allowing only loopback redirects): the address the sign-in page landed on, with its `code` and `state`; the node finishes the sign-in as if the browser had arrived at its callback and answers the status |
| GET | `/v1/sources/tidal/login` | | `{url}`: TIDAL's sign-in page (OAuth 2 + PKCE, scopes incl. `playback`); the browser comes back to `/tidal/callback` (no token needed there), which stores the user's tokens and redirects to `/?tidal=signed_in` |
| GET / PUT | `/v1/sources/tidal/unofficial` | PUT `{enabled?, quality?: max\|high, client_id?, client_secret?}` | Full tracks (unofficial): `{enabled, signed_in, user?, quality, pending?: {user_code, uri, expires_at}, health?: {ok, detail, at}, client_from?: settings\|config, warning}`; an empty `client_id` forgets the one set here; `enabled: true` after a failure is the retry |
| GET | `/v1/sources/qobuz/unofficial` | | Qobuz (unofficial-only): `{enabled, signed_in, user?, plan?, hires, quality, health?, app_from?, app_id?, warning}` (the secret never comes back) |
| PUT | `/v1/sources/qobuz/unofficial` | `{enabled?, quality?: max\|cd, app_id?, app_secret?}` | Switch it on or off (on again is Try again), the quality, the owner's app (an empty `app_id` forgets it) |
| POST | `/v1/sources/qobuz/unofficial/login` | `{email, password}` | Signs in once; the node keeps only Qobuz's session token. 403 for an account without a streaming plan |
| DELETE | `/v1/sources/qobuz/unofficial/login` | | Forgets the session |
| GET | `/v1/sources/soundcloud/unofficial` | | SoundCloud (unofficial-only): `{enabled, client_from?: settings\|config, client_id?, signed_in, user?, health?, warning}` (the token never comes back) |
| PUT | `/v1/sources/soundcloud/unofficial` | `{enabled?, client_id?, token?}` | Switch it on or off (on again is Try again); the owner's client ID (empty forgets it); their sign-in token, checked once with `me` (400 when SoundCloud doesn't take it; empty signs out) |
| GET | `/v1/sources/soundcloud/likes` (POST `…/import`) | | `{tracks, playlists}` the signed-in user's likes as source hits; POST favourites them here by their SoundCloud link (`{imported, skipped}`) |
| GET | `/v1/sources/soundcloud/playlists` (POST `…/{id}/import[?keep_source=true]`) | | `[{id, name, description?, items, url}]` the user's own sets; POST creates a private Pubky playlist of the tracks' SoundCloud links (`{imported, skipped, playlist}`) |
| GET | `/v1/sources/soundcloud/stream/{id}` | | The track as MP3 (or AAC) for browsers, relayed by the node; HLS is stitched into one stream |
| GET / PUT | `/v1/sources/bandcamp/unofficial` | PUT `{enabled?, identity?, format?}` | Bandcamp (unofficial): `{enabled, signed_in, user?, format, formats, folder, health?, warning}`; `identity` is the owner's cookie value, checked with Bandcamp first (400 when refused; empty signs out; it never comes back) |
| GET | `/v1/sources/bandcamp/collection` | | The owner's purchases: `[{id, title, artist?, kind, url, image?, item?, downloadable, downloaded}]` |
| POST | `/v1/sources/bandcamp/collection/{id}/download` | `{format?}` (optional body) | Downloads one purchase into `<data_dir>/bandcamp` (a library folder, scanned after); 409 when Bandcamp offers no download or it is already running |
| GET | `/v1/sources/bandcamp/downloads` | | This run's downloads: `[{id, title, state: queued\|downloading\|unpacking\|done\|failed, format?, bytes, total?, folder?, error?}]` |
| GET | `/v1/sources/bandcamp/stream/{key}` | | A Bandcamp stream for browsers, relayed with a fresh token |
| GET | `/v1/sources/qobuz/stream/{id}` | | The track as FLAC for browsers, relayed by the node |
| POST / DELETE | `/v1/sources/tidal/unofficial/login` | | POST starts the device sign-in (the code to enter at link.tidal.com, in `pending`; the node waits for the approval); DELETE forgets the session |
| GET / DELETE | `/v1/sources/tidal/status` (DELETE `…/session`) | | `{configured, signed_in, user_id?, country?, scope?, expires_at?, redirect_uri, requested_scopes, access_tier?, playback_limit?}` (`playback_limit` such as `FULL_REQUIRES_HIGHER_ACCESS_TIER` means TIDAL serves previews to this app's tier); DELETE forgets the tokens |
| GET | `/v1/sources/tidal/collection` (POST `…/import`) | | `{tracks, albums}` of the signed-in user's TIDAL collection as source hits; POST favourites them on the node (`{imported, skipped}`; ISRC where TIDAL has one, else the TIDAL link) |
| GET | `/v1/sources/tidal/playlists` (POST `…/{id}/import`) | | `[{id, name, description?, items, url}]` the user's own playlists; POST creates a private Pubky playlist of canonical URIs (`{imported, skipped, playlist}`) |
| GET | `/v1/sources/{source}/items/{id}` | | `{source, id, title, creator?, year?, description?, licence?, image?, url, tracks: [{title, track_no?, duration_ms?, format, url, artist?}]}`; play by enqueueing the track URLs (the archive gives one best-format file per recording, FLAC first; Audius one MP3 stream per track; TIDAL `tidal:track:<id>` URIs that the node resolves to a stream manifest for the signed-in user; TIDAL ids are `album:<id>` and `track:<id>`) |
| GET | `/v1/radio/search` | `?q=&tag=&country=&limit=` | `{stations: [{id, name, url, homepage?, favicon?, codec?, bitrate?, country?, tags}]}` from radio-browser.info, most voted first; 400 without a term, 502 when the directory fails |
| GET / PUT | `/v1/radio/stations` | PUT: a `Station` | saved stations (node-local settings); PUT adds or updates by id/url |
| DELETE | `/v1/radio/stations/{id}` | | remaining stations; 404 when unknown |
| POST | `/v1/radio/play` | `{zone, station}` | replaces the zone's queue with the stream and plays it |
| GET | `/v1/podcasts/search` | `?q=&limit=` | `{shows: [{id, title, author?, description?, image?, feed, links: [{site, uri}], subscribed}]}`: Pinakes first (one per show, with its pages on other apps), then Apple's iTunes Search; `id` is `feed:<base64url of the feed>` |
| GET / POST | `/v1/podcasts/subscriptions` | POST `{feed}`: a feed URL or a search `id` | subscriptions `{subscriptions: [{id, url, title, author?, image?, episodes, unplayed, latest?, refreshed}]}`; POST fetches the feed (422 when it is not an RSS podcast feed) |
| GET / DELETE | `/v1/podcasts/subscriptions/{id}` | | the show and its episodes, newest first (`items: [{id, title, url, mime?, published?, duration_ms?, image?, description?, feed_id, feed_title, position_ms, played}]`); DELETE unsubscribes |
| GET | `/v1/podcasts/new` | `?limit=` | `{episodes}`: the newest unplayed episodes across subscriptions (a Home row) |
| POST | `/v1/podcasts/refresh` | | fetches every feed now (they also refresh every 6 hours) |
| POST | `/v1/podcasts/play` | `{zone, episode, restart?}` | replaces the zone's queue with the episode and plays it from the saved position (unless `restart`) |
| PUT | `/v1/podcasts/progress/{episode}` | `{position_ms?, played?}` | marks an episode played or unplayed, or sets its position |
| GET | `/v1/radio/curated` | | `{stations}`: SomaFM's channels (ids `somafm:<channel>`, cached an hour), then Radio Paradise's (`rp:<n>`; FLAC except Serenity) |
| GET | `/v1/radio/countries` | `?limit=` | `{items: [{name, code, stations}]}` from radio-browser.info, most stations first; use `code` or `name` with `/v1/radio/search?country=` |
| GET | `/v1/radio/tags` | `?limit=` | `{items: [{name, stations}]}`: the most used station tags (genres, moods, languages) |
| GET | `/v1/sources/radio/search`, `/v1/sources/radio/items/{id}` | | radio as a source: curated stations matching the name or a tag first, then the directory's; an item is one station with its stream as the only track |
| GET | `/v1/pubky` | | `{user, unsynced, needs_resign, missing?, notice?}`: `needs_resign` when the session was approved for fewer folders than the node asks for at sign-in (`missing` lists them) or the homeserver refused one; sign in again (same Pubky Ring flow) to replace the session |
| POST / GET | `/v1/pubky/login` | | POST starts a Pubky Ring sign-in and returns `{url}` (`pubkyauth://…`); GET polls `{status: none\|pending\|signed_in, user?, url?}`. On approval the node stores the session secret, publishes its record and syncs |
| GET | `/v1/homeserver/bundled` | | the homeserver bundled with the node (deploy/bundle): `{configured, reachable, public_key, version, users, disabled_users, disk_used_mb, unused_invites, pubky_address, icann_domain, error}`; never the admin password |
| POST | `/v1/homeserver/bundled/owner-signup` | | "Create your Pubky account on this node's homeserver": `{url, qr_svg}` for Pubky Ring (a `signup_grant` link with the owner's token, unlimited storage); poll `GET /v1/pubky/login`. 409 while the node is signed in; 404 without a bundled homeserver |
| GET | `/v1/storage` | | the status line: `{homeserver: {mode: bundled\|external\|none, user, quota_mb, used_bytes}, saving: {state: saved\|saving\|error\|signed_out, pending, last_saved_at, error}, music: {on_homeserver: {path, tracks, bytes}, folders: [{path, tracks, bytes}]}, job, uploads}`. Changes are saved to the homeserver by themselves (about 20 s after the last one) |
| POST | `/v1/storage/save` | | save now (the retry) |
| POST | `/v1/library/uploads` | `{name, size, relative_path?, blake3?}` | starts one file's upload: `{id, offset, chunk_size}`, or `{already: true}` when the homeserver has that content (`blake3` given). Audio files only |
| PUT | `/v1/library/uploads/{id}?offset=N` | the chunk (≤ `chunk_size`, 4 MiB) | `{offset, state}`; a wrong `N` answers 409 `{offset}` (resume from there). The last chunk stores the file on the homeserver (`/priv/pubky.music/files/<hash>-<name>`) and in the library |
| GET / DELETE | `/v1/library/uploads/{id}` | | `{id, name, size, offset, state: receiving\|storing\|stored\|failed, error, stored_as}`; DELETE cancels. Unfinished uploads are dropped after 24 h or a node restart |
| GET | `/v1/library/uploads` | | `{totals: {files, stored, failed, bytes_total, bytes_done}, uploads}` for the job chip |
| POST | `/v1/library/imports` | `{path, dry_run?}` | copies a folder's audio files into the homeserver as a job (kind `import`, progress and cancel as `/v1/pubky/files/upload/{id}` and `…/jobs/{id}/cancel`); files already there are skipped. `dry_run` answers the count and size |
| GET / POST | `/v1/library/webdav` | POST: `{url, user?, password?, name?}` | WebDAV shares as library roots: `[{id, name, url, user, tracks, bytes}]`, never the password. POST checks the folder (`PROPFIND`), keeps the share and scans it; songs are `webdav://<id>/<path>` in the library and play from the share. 502 when the share refuses |
| POST | `/v1/library/webdav/{id}/scan` | | reads the share again (also every 6 hours by itself) |
| DELETE | `/v1/library/webdav/{id}` | | forgets the share and its songs in the library; `{removed, files, tracks}`. The files on the share stay |
| GET | `/v1/library/drives` | | Google Drive and OneDrive: `{providers: [{id: google\|onedrive, name, configured, connected, user}], roots: [{id, provider, name, tracks, bytes}]}`. `configured` = an app (client id) is set up |
| POST | `/v1/library/drives/{provider}/login` | | `{url}`: the provider's sign-in (OAuth 2 with PKCE, no secret, through the return page); it comes back to `GET /drives/{provider}/callback` and then to Settings › Library |
| GET | `/v1/library/drives/{provider}/folders` | `?parent=` (`root` by default) | the folders inside, to pick one: `[{id, name, folder}]` |
| POST | `/v1/library/drives` | `{provider, folder_id, name?}` | the folder becomes a library root and is scanned (only tags are fetched); songs are `gdrive://…`/`onedrive://…` and play from the drive |
| POST / DELETE | `/v1/library/drives/roots/{id}/scan`, `/v1/library/drives/roots/{id}` | | read it again; or remove it and its songs from the library (the files stay) |
| DELETE | `/v1/library/drives/{provider}/account` | | sign out of that drive |
| GET / PUT / DELETE | `/v1/remote/tunnel` | PUT: `{token, homeserver_domain, music_hostname?}`; GET: `?check=1` | Cloudflare Tunnel for the bundled install: `{available, configured, homeserver_domain, music_hostname, published, restart_needed, restart_command, restart_hint, homeserver_reachable, music_reachable}` (`restart_hint`: how to restart in words, on installs that set `PM_TUNNEL_RESTART_HINT`, e.g. Umbrel and bare metal; then `restart_command` is empty), never the token. PUT saves the token and the homeserver's hostname into the folder shared with cloudflared and the config wrapper (`PM_TUNNEL_DIR`); `published` says whether the homeserver's record already points at the hostname on 443; `check=1` tries both hostnames over HTTPS |
| GET | `/v1/me` | | who this token is: `{role: owner\|listener, key, zones}` (`zones`: the ones a listener may play on, `null` = all). A listener may read `/v1/me`, `/v1/events`, `/v1/info`, `/v1/status`, `/v1/library/{albums,artists,tracks,genres,search,stats}` (identification candidates excluded), `/v1/lyrics`, `/v1/search`, `/v1/item/*`, `/v1/catalog/search`, `/v1/discover/*`, `/v1/shared/*`, `/v1/profiles/*`, `/v1/mb/*`, radio browsing, `/v1/podcasts/search` and `/v1/zones` (a zone's own routes only for zones open to listeners), and may play on those zones. Everything else answers 403 "Not for listeners: …" (`accounts.rs`). In a listener's JSON answers a track's `file.path` is the file name only, never the owner's folders. |
| DELETE | `/v1/me/token` | | a listener signs this device out |
| GET | `/v1/party` | | the parties running now: `[{id, name, zone, zone_name, created_at, expires_at, adds_per_hour, waiting_per_guest, guests, added, url, qr_svg}]` (owner only) |
| POST | `/v1/party` | `{zone, name?, hours? (1–72, 6), adds_per_hour? (1–100, 5), waiting_per_guest? (1–50, 3)}` | start a party on a zone: a guest link (`<public_url>/party/<code>`) and its QR |
| DELETE | `/v1/party/{id}` | | end a party; its guests' tokens stop at once |
| POST | `/v1/party/join` | `{code, name?}` | **no token:** a guest's token (`pmg_…`) for the party behind a link; 410 when it has ended or the code is wrong |
| GET | `/v1/party/guest` | | **guest token:** `{party, zone_name, expires_at, you, playing, waiting: [{title, artist, cover, by, mine}], adds_left, waiting_left, next_add_at}` |
| GET | `/v1/party/guest/search` | `?q=` | **guest token:** library songs `[{track_id, title, artist, cover, duration_ms}]` |
| POST | `/v1/party/guest/queue` | `{track_id}` | **guest token:** add a song to the guest section (after the current song and other guests' waiting songs). 429 past the hourly limit or with too many waiting; the limits are checked on the node. A guest token opens only `/v1/party/guest…` and album covers |
| GET | `/v1/library/loudness` | | measured loudness for files without ReplayGain tags: `{on, measured, failed, to_measure, running}`; `on` = some zone has loudness on, so new files are measured in the background |
| POST | `/v1/library/loudness/measure` | | measure what's left now, even with loudness off |
| GET / PUT | `/v1/people/zones` | PUT: `{zones}` | the zones open to listeners (empty = all) |
| GET | `/v1/people` | | `{owner, people: [{key, access, name, music_enabled, homeserver_enabled, added_at, storage_quota_mb, used_bytes, sessions}], invites: [{id, access, name, token, storage_quota_mb, created_at, url, qr_svg}], homeserver}`. `access` is `homeserver`, `homeserver_music` or `music`; used invites turn into people. `homeserver_enabled` is what the node last set (the admin API can't read it); `used_bytes` is the size of their files in the homeserver's files folder, when the node has it (`[homeserver] files_dir`), else `null` |
| POST | `/v1/people/invites` | `{access, name?, storage_quota_mb?, key?}` | a homeserver invite: a signup token with that quota, as a Pubky Ring link (`pubkyauth://direct_signup?hs=…&st=…`) and QR. `music` adds someone with an account elsewhere by `key` |
| DELETE | `/v1/people/invites/{id}` | | forget an unused invite (the homeserver can't revoke a token; it stays usable) |
| PATCH | `/v1/people/{key}` | `{name?, music?, homeserver_enabled?, storage_quota_mb?}` | music access on or off; the homeserver account on or off (off blocks writes, reads stay); quota in MB, `"unlimited"`, or `null` for the default |
| POST | `/v1/pubky/sync` | | `{pushed, pulled, deleted, errors, notes?, pending}` (`notes`: things worth knowing that are not errors, such as playlists kept in the old format; `pending`: changes left for later passes: a pass writes at most 200, and the node carries on in the background, 2 s apart); 412 without a session. The first sync on a node moves what only the old `/priv/pubky.app/` folders hold to v1 and keeps the old files |
| GET / POST | `/v1/pubky/files` | POST: `{track_id}` or `{path}` | private files on the homeserver (`/priv/pubky.music/files/`); upload a node file |
| POST | `/v1/pubky/files/mirror` | `{dest?, dry_run?}` | bring the files this node lacks (matched by content hash, so the library's own files are not downloaded again) into `dest` (default: `From homeserver` in the first library folder) as a background job, paced by the copy speed, then scan it. Returns the job: `{id, kind: "bring", dest, total, bytes_total, done, bytes_done, skipped (already here), broken (empty or doubled-name files left out), failed, current, finished, cancelled, errors}`; a dry run only counts |
| DELETE | `/v1/pubky/files` | `{paths}` | remove private files (only under `/priv/pubky.music/files/`); returns the new list |
| GET | `/v1/pubky/files/jobs` | | the files job running now (copy or bring), or `null` |
| GET | `/v1/pubky/files/jobs/{id}` | | one job's progress (same as `…/upload/{id}`) |
| POST | `/v1/pubky/files/jobs/{id}/cancel` | | stop after the file in hand (a download stops at once, nothing half-written is kept) |
| GET | `/upnp/node.xml` | | (no token) the UPnP device description the SSDP announcement points at: name, version, `uuid:<node id>`, `presentationURL` `/`. See `docs/CONNECTION.md` |
| GET | `/v1/events` | WebSocket | first `{"type":"snapshot","seq","boot","library_rev","zones":[...]}`, then numbered (`seq`) `zone` (full state; also sent when a zone is created), `zone_event` (track started/ended, skipped, paused, resumed, seeked, stopped, queue, `stream_title` for radio, error), `zone_removed`, `library_changed`. Send `{"type":"ping","id":n}` for `{"type":"pong","id":n}`, `{"type":"resync"}` for a fresh snapshot. See `docs/CONNECTION.md` |

Playlists live in the node and sync to the user's homeserver as pubky-social-specs v1 objects (ADR 0014)
when the node has a session: a playlist is a private social `collection` (up to 100 songs; longer ones stay
in the old private envelope for now), a rated review a music `review`, a text-only review a social note
embedding the music URL, a play a private music `listen`, a favourite a v1 bookmark. Items are canonical
music URLs only.

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
