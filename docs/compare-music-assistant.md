# Music Assistant and the node: what to learn

[Music Assistant](https://github.com/music-assistant/server) (MA, Apache-2.0) is a Python music
server with 130-odd providers. This page compares it with the node and ranks what is worth taking.
Read against MA's `main` on 2026-10-23. For using the two together, see
[MUSIC-ASSISTANT.md](MUSIC-ASSISTANT.md).

Porting MA code to Rust is fine under its licence: name the MA file in the new file's header and
add a line to `NOTICE`. **MA's shared API keys are not ours to use.** MA reads them with
`app_var(...)` from a file that only official MA builds carry, under a licence for those builds
alone. Where a service needs a key (fanart.tv, TheAudioDB, Last.fm, AcoustID), we use our own.

## How MA is built

- **Providers.** Each one is a folder with a `manifest.json`: `type` (music, player, metadata,
  plugin), `domain`, `stage` (stable … deprecated), `multi_instance`, `depends_on`, `requirements`,
  `mdns_discovery`. It declares feature flags (`ProviderFeature`: SEARCH, LIBRARY_TRACKS_EDIT,
  LYRICS, SCROBBLE, SIMILAR_TRACKS, …), and the core only asks a provider for what it flags.
- **Setup flows.** A provider writes `run_setup(session)`: forms, "open this URL and wait for the
  callback", device-code polling, then `finish(values)`. The values are stored encrypted.
- **Sign-ins.** One hosted page, `music-assistant.io/callback`, with the local callback carried in
  `state`. This is the same pattern as our [return page](STREAMING-PROVIDERS.md). MA adds a
  fallback for providers that allow only loopback redirects: paste the redirect URL back.
- **Library.** Library items keep *provider mappings* to every provider item that resolves to them.
  Each provider syncs per media type, with a deletion pass.
- **Players and queues.** One queue per player. The queue is the player's usual source, not its
  only one. Enqueue choices: play, next, add, replace, replace-next. Autoplay modes: similar,
  playlist, library. Each queue plays either one continuous "flow" stream or one stream per track.
- **Groups.**
  - Sync groups play in sync and use one protocol only.
  - Universal groups mix protocols but don't play in sync.
  - The universal player merges one device's AirPlay, Cast and DLNA faces into a single player.
- **Announcements.** These play TTS or a URL over a player: an optional chime, a chosen volume,
  then volume, grouping and queue are restored. The player's own announce feature is used where
  there is one.
- **Streams.** Crossfade (standard, or "smart" using beat detection with neural nets). Volume
  normalisation with EBU R128 measured by ffmpeg.
- **Home Assistant add-on.** A second port on HA's network trusts HA's user headers (checked
  against the socket's address, so the headers can't be spoofed). Users are created on first visit.
- **Sendspin** is MA's own open playback protocol (Open Home Foundation, spec at
  github.com/Sendspin/spec), for synced multi-room over WebSocket and WebRTC.

## What MA has that we lack

- **Players we don't drive natively:** HEOS, BluOS, MusicCast, WiiM/LinkPlay, MPD, Bose
  SoundTouch, Home Assistant media players. Most of these also take DLNA, so the node plays to
  them already, but without their own grouping.
- **Announcements**, **crossfade**, and **measured loudness** for files without ReplayGain tags.
- **Merging one device's protocols** into one player, and groups that mix protocols.
- **Party mode:**
  - guests join by link or QR code for a few hours;
  - their songs go ahead of the queue;
  - they can "boost" a song;
  - each action has its own rate limit.
- **gPodder sync** (subscriptions and episode progress, including Nextcloud's gpoddersync).
- **Last.fm** scrobbling and similar artists, **fanart.tv** and **TheAudioDB** images, and
  **iTunes** high-resolution covers.
- **Library heuristics**, where MA does better:
  - "Various Artists" for albums without an album artist.
  - Compilation and release-type tags.
  - Disc folders (`CD1`, `Disc 2`) merged into one album.
  - Covers in the album folder above disc folders, and artist images from artist folders.
  - Kodi `.nfo` files.
  - Playlists (`.m3u`, `.pls`) scanned from the library and editable.
- **Smart playlists from seeds:** MA's rules also take seed songs or artists (plus similar ones),
  a duration range and exclusion lists. Ours have the rest.
- **The provider manifest:** a stage label (beta, alpha) and declared features. The UI can hide
  what a source can't do.

## What we have that MA doesn't

- **A bit-perfect signal path** with every conversion named, and its quality level shown.
- **An honest sync label** on every zone, and sample-accurate groups across the node's own
  endpoints, Slimproto players, Snapcast, Sonos and Cast groups.
- **We serve, as well as play.** Our OpenSubsonic API, and a DLNA renderer per zone, let MA, Home
  Assistant and Subsonic apps use the node. MA is a Subsonic client only.
- **Pubky:**
  - one identity;
  - remote access by public key with no port forwarding;
  - playlists, favourites and tags as Pubky objects with canonical IDs;
  - other people's shared playlists;
  - Pinakes discovery.
- **A bundled homeserver** with accounts per person (homeserver, music, or both), invites and a
  Cloudflare tunnel.
- **Roon migration**, and fingerprint identification of untagged files with a confirmation step.
- **Native apps** (desktop, Android) that find the node from a Pubky Ring login.

## Port these (ranked)

Cost: S = a day or less, M = a few days, L = a week or more. Value is for the node's users.

| # | Idea | Cost | Value | Notes |
| :-- | :-- | :-- | :-- | :-- |
| 1 | Album grouping fixes: "Various Artists" fallback, compilation and release-type tags, disc folders, parent-folder covers | S–M | high | Bugs today: a various-artists album without an album-artist tag or release MBID splits into one album per artist (`pm-library/src/scan.rs`, album artist falls back to the first track artist). Fixed in #57. |
| 2 | Artist splitting: with N MusicBrainz artist ids, keep at most N names | S | low–medium | We split `&` and `,` on purpose (owner, 2026-10-22: "Sefa & Cyclon"), and MA does too once the ids say there are several. MA never splits `/`; we keep splitting it, because ID3v2.3 writes several artists that way. Our list of about 28 whole names misses acts like "Coheed & Cambria"; ids fix those when files have them. |
| 3 | A real `.pls` parser, and playlist paths relative to the playlist file | S | medium | `.pls` goes to the M3U parser today, and the import passes no base folder. Fixed in #57 (uploaded files have no folder, so their entries match by file name). |
| 4 | Party mode on listener accounts (#46): guest links with an expiry, a guest section in the queue, rate limits **checked on the node** | M | high | MA sends its limits to the guest page and checks them there only. Built in #77: guests need no account; the guest token opens only the party. |
| 5 | Announcements on a zone: URL or TTS, chime, volume, then restore and resume | M | medium | The Home Assistant integration (mn-sources) can use it. |
| 6 | iTunes covers as a fallback after the Cover Art Archive | S | medium | Apple's Search and Lookup API, no key. |
| 7 | Measured loudness (EBU R128) for files without ReplayGain, feeding our ReplayGain stage | M | medium | The `ebur128` crate (MIT). Off unless loudness is on, so bit-perfect stays the default. Built in #79. |
| 8 | HEOS player (Denon/Marantz), with HEOS groups | S–M | medium | Official HEOS CLI protocol, JSON over TCP 1255, mDNS discovery. |
| 9 | BluOS player (Bluesound, NAD), with BluOS groups | S | medium | Official BluOS HTTP API. Built in #78. |
| 10 | gPodder sync of subscriptions and progress | M | medium | gpodder.net API v2, Nextcloud gpoddersync, opodsync. |
| 11 | Crossfade per zone (standard, off by default) | M | medium | Marked as a named stage, like the others. Skip "smart" fades (neural nets). Built in #80. |
| 12 | Last.fm scrobbling and similar artists | M | medium | Needs our own API key, and the secret for signing. Either the owner enters both, or the calls go through Pinakes. A client secret never ships. |
| 13 | Smart playlists from seeds, with duration and exclusions | S | low–medium | Seeds use our "keep playing" similar-songs logic. |
| 14 | A declared feature set per source (search, library, playlists, lyrics, scrobble), with a stage label | M | medium | The app hides what a source can't do. |
| 15 | MPD player | S | low–medium | Official text protocol, no discovery. Built in #81. |
| 16 | fanart.tv and TheAudioDB artist images | S | low–medium | Needs our own project key (public, through Pinakes or built in like the AcoustID key). We have Wikipedia portraits already. |
| 17 | "Paste the redirect URL" fallback for sign-ins | S | low | Only for apps that allow loopback redirects only. |
| 18 | A Sendspin output | M–L | watch | Open spec, but young. Revisit when HA devices ship it. |
| 19 | MusicCast, WiiM/LinkPlay, Bose SoundTouch players | M each | low | DLNA already plays to them; native grouping is the gain. |

**Not taking:**
- **Genius lyrics:** scraped web pages. We have LRCLIB.
- **TuneIn:** an undocumented partner endpoint with a hard-coded partner id.
- **go-librespot:** our librespot mode already covers this, as an unofficial mode.
- **Neural crossfades and audio similarity:** these need large ML models.
- **AirPlay receiver:** it wraps shairport-sync, a separate program.

Radio Paradise's API is public but undocumented. Our Radio Paradise source uses only its
published stream addresses.

## MA's sources, checked against ours

| MA source | Upstream | Ours |
| :-- | :-- | :-- |
| Plex, Audiobookshelf, WebDAV, Google Drive, OneDrive | official APIs | built (Drive and OneDrive in review) |
| Jellyfin, Emby | official REST | built (one client for both) |
| OpenSubsonic (as a client) | Subsonic API | built ("Your server") |
| Internet Archive | official, no key | built |
| RadioBrowser, SomaFM, Radio Paradise | public, no key | built |
| Podcasts: RSS, Podcast Index, iTunes search | official | built. Podcast Index goes through Pinakes, so the secret stays off the node. |
| gPodder | gpodder.net API v2 | not yet (#10 above) |
| Cover Art Archive, LRCLIB, Wikipedia, MusicBrainz, AcoustID | official | built |
| fanart.tv, TheAudioDB, iTunes artwork | official, keys for the first two | not yet (#6, #16) |
| Last.fm scrobble and recommendations, Subsonic scrobble | official | ListenBrainz only (#12) |
| TuneIn, Genius | unofficial | not taking |
