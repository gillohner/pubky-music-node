# Changelog

Newest first. Dates are when the change landed on `main`.

## 2026-10-20

- **Pinakes music step** folded in (steward note 2026-10-20, fixtures from pubky/pinakes#21): cards carry `details.facts` (MBID, ISRCs, UPC, artist type, duration), one `also_on` link per service with tagged links first, `maybe_same` on event cards, `key=` search and trending charts by type. Library album and track **Versions** now ask Pinakes by key (`mbid:`, `isrc:`, `upc:`; Pinakes has no card for a MusicBrainz URL itself) and list every service that has the recording (Spotify, Apple Music, YouTube, Bandcamp…), tagged links first; TIDAL and Deezer ones are playable. Discover rows show the artist from the card.
- **Tag my whole library on Pubky** (Settings → Pubky; `POST /v1/tags/bulk`): one v1 PubkyAppTag per identified track, so Pinakes matches your own songs across services.
- Fixed: seeking in the browser zone restarted library tracks from 0:00, because `/v1/library/tracks/{id}/file` ignored byte ranges; it now answers 206 with `Accept-Ranges` (shared with the TIDAL proxy).
- **Add renderer by address** (Settings → Outputs; `POST /v1/outputs/upnp`): a Linn or other renderer on another subnet/VLAN is added by IP (unicast SSDP) or description URL and remembered across restarts.
- TIDAL sign-in behind a reverse proxy without `public_url`: the redirect URI now follows the request's host and forwarded scheme instead of the listen address.
- **Deezer catalogue** (no credentials): artists, albums and tracks in the unified search with ISRC/UPC, album and artist pages, Versions of library albums/tracks by UPC/ISRC, and 30 s previews (`deezer:track:<id>`; browsers via `/v1/sources/deezer/stream/{id}`). Source hits now carry a `uri` so clients queue any service's tracks the same way.
- Settings → Outputs gained **Scan the network** (`GET /v1/outputs/discovery`): every UPnP device that answers `ssdp:all`, whether it is a MediaRenderer, Cast devices, and plain-language findings when nothing answers (Docker bridge: use host networking), when a device is not a renderer (OpenHome-only Linn), or when `public_url` points at 0.0.0.0/localhost.
- Release builds of the native controller, node and endpoint for Linux, Windows and macOS; the
  Windows and macOS controllers play locally through CoreAudio / WASAPI (shared mode, `cpal`).
- TrueNAS SCALE: a ready compose file (`deploy/truenas/`) and deployment notes.
- The TIDAL browser proxy serves whole tracks with a known length and byte ranges, so the
  browser zone shows the real length and seeks, pauses and resumes like a file.
- Streaming is integrated where the music is (Roon-style): the main search answers with TIDAL
  albums and tracks next to the library (flagged when already owned), album pages show Versions
  on TIDAL by UPC, track menus offer Stream from TIDAL by ISRC, and the browser zone plays TIDAL
  through a node proxy; Home shows a strip from the TIDAL collection and TIDAL album pages link
  to the owned album. Search merges artists, albums and tracks from the library and the
  streaming accounts (owned items first, streaming artists open their own page), queue items
  from streaming carry their cover and length, and the browser zone shows a stream's real length.
  The Sources screen is for connecting accounts.
- TIDAL through the official API: catalogue search, albums and tracks in the Sources view, sign-in
  with OAuth 2 + PKCE from either UI, playback of `tidal:track:` URIs through stream manifests and
  HLS segments (FLAC; verified live: the THIRD_PARTY developer tier gets 30 s previews until TIDAL
  raises the app's access tier, which the Sources page now says); the signed-in
  user's collection and playlists listed on the Sources page and importable as favourites and
  private Pubky playlists (ISRC where TIDAL has one). Config `[tidal]` or `PM_TIDAL_*`.
- Zones that could play nothing now say the last open error.
- Queue tools in both UIs: shuffle what comes next, save the queue as a playlist, transfer playback
  to another zone, Start radio from a track, an album or an artist page, sleep timer per zone.
- Lyrics from tags or a sidecar `.lrc` (synced lines follow the position); Subsonic `getLyrics` and
  `getLyricsBySongId`.
- Focus: filter albums by hi-res, DSD, CD quality, lossless or lossy, with format badges on tiles.
- New releases from your artists on Home (MusicBrainz release groups for artists tagged with an
  MBID, cached a week per artist).
- Zones page in the web UI and the native app: every zone with transport, volume and selection.
- Homeserver private files: list, upload from track menus and mirror, in both UIs.
- Library backup and restore (`pm-node backup`, `pm-node restore`, `GET /v1/backup`, Settings).
- Volume limit per zone; the Volume request shape now matches the docs (`{db}`, `{percent}`,
  `"up"`, `"down"`, `{mute}`), which fixes volume from both UIs on node zones.
- Web UI installs to a phone home screen (web app manifest) and answers media keys.
- Multi-arch images on GHCR (`release.yml`, cross-compiled arm64).
- Subsonic `jukeboxControl` drives a node zone from any Subsonic client (the zone is picked under
  Settings); `byYear` album lists honour `fromYear`/`toYear`.
- The browser zone's queue survives a reload.
- Libraries beyond 1000 albums or tracks show everything: both UIs page through the API's
  `after` cursor.
- Filter boxes on the Albums, Artists and Tracks pages of both UIs.
- The native app's Albums grid is virtualised (only visible covers are fetched).
- The web UI's Tracks page renders 500 rows at a time with a filter over the whole list.
- Sync no longer fails on a fresh homeserver: a folder nobody has written to yet lists as
  empty instead of a 404 error.
- Queue, repeat, radio, shuffle and transport replies wait for the engine to apply the command.
