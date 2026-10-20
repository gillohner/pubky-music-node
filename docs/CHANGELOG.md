# Changelog

Newest first. Dates are when the change landed on `main`.

## 2026-10-20

- **Item pages play** (steward plan step 1): every Discover row, tile and item page has a Play that resolves through the node's connected services (`GET /v1/discover/play`): the library when the recording or album is owned (MBID, ISRC, UPC), else the best service link of the Pinakes group, tagged first, TIDAL before Spotify Connect; an album card plays its tracks from that service's album (an Apple Music card for "Dracula" plays from TIDAL 495402659). With nothing connected, Play offers "Connect TIDAL" and a 30 s Deezer preview. Rows and item pages show the answer as a "Plays from" chip.
- **UX wireframes, phase 1** (`docs/ux/`, screenshots in `docs/ux/shots/`, served at `/ux/` on debug builds): one menu with "Open on ›", the item page (Plays from, Available on, Tags on Pubky apart from About, Versions), a mixed playlist with source chips, a pin, an unavailable row and the source popover, the play-from order editors, and the import dialog. Review notes in the owner's `UX-REVIEW.md`.
- Fixed: the desktop shell did not compile (a missing field in the `PM_NODE` settings path).
- **The desktop app is the web UI plus a player** (`pm-desktop`): one connect step (Sign in with Pubky Ring finds the node through your homeserver; an address and token under "Or connect by address"), then the node's own UI in a native window. The app serves this machine's DACs to the node as a `pm-endpoint` (bit-perfect ALSA `hw:` on Linux, the system output elsewhere) and registers itself with `POST /v1/outputs/endpoints`, so "<this machine> · <DAC>" is an ordinary zone in the UI with the node's signal path. An old address from another network is forgotten on the next registration; `DELETE /v1/outputs/endpoints/{address}` removes an endpoint and its zones. Saved endpoint addresses are restored at start and backed up with the owner's settings.
- Web: service badges on Discover and Sources tiles and search rows are glyph icons in a round badge instead of a text overlay; the cover placeholder rule no longer stretches badges over the whole tile.
- Native egui app: Settings → Node is one step (connected: name and address with Scan/Refresh/Sync; otherwise a "Sign in with Pubky Ring" button, address and token under "Connect by address"); the session-secret field is gone (`PM_SESSION_SECRET` still works for scripts). Output enumeration happens once when Settings opens, not every frame (it was probing the DAC that was playing ~16 times a second), with a Rescan button; track titles no longer wrap into a narrow column.
- **Spotify** (`pm-api/src/spotify.rs`): sign-in with PKCE, catalogue in the unified search with ISRC/UPC (merges with TIDAL and Deezer rows), album and artist pages, Versions by ISRC/UPC, saved tracks/albums and playlists imported by ISRC, and "Play on Spotify" on the user's Connect devices (no audio enters the node). Config `[spotify]` / `PM_SPOTIFY_CLIENT_ID|SECRET`.
- **MusicBrainz caching proxy** at `/mb/ws/2/…` (no token): one upstream request per second, a week of cache, the node's contact in the User-Agent; Pinakes can use it as its MusicBrainz `base_url`.
- The upstream contact (User-Agent for MusicBrainz, Wikipedia, Pinakes) is derived from the owner's Pubky identity, `https://pubky.app/profile/<pubkey>`, as soon as the node has a session and no contact was set; a typed contact still wins.
- **Settings follow your Pubky identity**: after every sync the node writes contact, library folders, the TIDAL session, renderers added by hand, the jukebox zone and DSP to `/priv/pubky.music/settings.json`; a fresh node restores them after its first Ring sign-in (`POST /v1/node/settings/backup|restore` for manual runs). A reinstalled container still needs `/data` on a host path to keep its Pubky session itself.
- **Sign in with Pubky Ring, no tokens**: browsers get a "Sign in with Pubky Ring" button in the connect dialog (`POST /v1/auth/pubky/start`, `GET /v1/auth/pubky/poll`); the node accepts only its own owner and hands the API token out itself. The native app has the same button in Settings and additionally finds the node's address through the owner's homeserver, so nothing is typed on a new device.
- Native app: transport and queue glyphs that egui ships (the old ones rendered as boxes on Linux); alsa-lib's stderr chatter is silenced in the node and the app.
- **Discover, Roon-style** (web): Top tracks / Top albums charts with today, week and month windows, Tags, Friends and "Search Pubky" tabs; every row shows the service, how many services carry the song, who owns it, and plays from the library, from TIDAL/Deezer, or via "Play on…"; a details page per card with MusicBrainz, ISRC, UPC and duration facts, every "Also on" link, who tagged it on Pubky, your tags and your note/rating (v1 tags and posts). The main search gained an "On Pubky" section.
- Discovery searches every IPv4 interface (a NAS with a leg on two VLANs finds renderers on both); `GET /v1/discover/charts|search|item` expose Pinakes charts, text search and one card with facts, every link, taggers and library matches.
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
