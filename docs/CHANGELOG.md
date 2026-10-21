# Changelog

Newest first. Dates are when the change landed on `main`.

## 2026-10-21

- **Fix: the edge release could vanish.** Publishing deleted the old edge release first. When an upload then timed out (HTTP 408, on the first build with the updater), there was no edge at all until I republished it by hand from that run's files. Now the edge release stays up: its tag moves to the new commit, files are replaced in place with retries, the update feed and checksums go last, and files the new build lacks are removed. Versioned releases retry their uploads too.
- **The owner's Discover findings, fixed on the node** (step 4 list).
  - **The same song shows once.** Home's lists merge listings of one song: same MusicBrainz id or ISRC, else the same title (without "Remastered 2011", "(feat. …)", "| Music Video") and artist. A listing without an artist joins the one that has it. The row says "4 versions" and shows the plain title. Live, the 7-day chart's four Bohemian Rhapsody rows are one, and so are the two "Das Boot" rows.
  - **No placeholder names.** A listing named "ISRC NLS161600475", or not named at all, is named by MusicBrainz, else by Deezer's public catalogue by ISRC, else by the TIDAL track it plays from (live: "Jim" by Sefa, "Mr. Brightside" by The Killers). "Untitled on …" stays only when nothing knows the song.
  - **No raw file names as titles.** An untagged file is titled from its name without the track number and underscores (`14_wasted_bits` → "Wasted bits"). Tracks already in the library are retitled once when the node starts.
- **The desktop app updates itself** (infra item 3). At start it checks the channel it was built for: builds of main follow edge, and tagged releases follow the latest release. When there is a newer version it asks, then installs it and restarts. This works on macOS, on Windows, and on Linux as an AppImage. Updates are signed with the project's own update key, and the app installs nothing else. Edge builds now carry `0.1.0-<run>` versions so each is newer than the last. Each release has a `latest.json` feed. Details in `docs/RELEASING.md`.
- **ListenBrainz** (step 5.5).
  - Settings › ListenBrainz connects with the user token. It's checked with ListenBrainz, kept on the node, and never returned.
  - **Send what I play here:** each zone reports "playing now". A song counts as a listen once it has played half its length or four minutes, or plays to its end, and each play is sent once. Listens ListenBrainz can't take wait in a queue and go with the next one.
  - **Bring my listens here:** imports your listens into the node's history (private listen posts) by MusicBrainz recording, picking up from the last import.
  - **Recommended for you** on Home: ListenBrainz's picks, named and with covers from its metadata. **Made for you** playlists (Weekly Jams, Weekly Exploration, …) import as Pubky playlists.
  - API: `GET|PUT|DELETE /v1/listenbrainz`, `POST /v1/listenbrainz/import`, `GET /v1/listenbrainz/recommendations`, `GET /v1/listenbrainz/playlists`, `POST /v1/listenbrainz/playlists/{id}/import`.
  - Test: a mock ListenBrainz covering a bad token, a zone playing while ListenBrainz is down (the listen waits, then goes), history import (once), recommendations and playlist import. Checked live: a wrong token is refused, and a public user's recommendations and playlists load.
- **MusicBrainz recordings play even when Pinakes hasn't seen them** (ADR 0001 order). MusicBrainz names the recording and gives its ISRC, and TIDAL or Spotify find it by that ISRC. This works for Play, playlists and recommendations. Lookups are cached for 30 days and paced as MusicBrainz asks. Live: a ListenBrainz pick (Angine de Poitrine, "Sherpa") plays from TIDAL.
- **Jellyfin (and Emby) as a source** (step 5.2).
  - Sources › Jellyfin signs in once with a user and password. The node keeps only the access token Jellyfin gives it, never the password, and signing out ends the session on the server too.
  - Search (albums and songs, also in the one search), album and song pages, covers relayed by the node.
  - Zones play the original file (`jellyfin:track:<id>`), and browsers get it relayed.
  - Playlists import by each song's MusicBrainz recording id (Jellyfin's `MusicBrainzTrack` is a release track and isn't used).
  - Test: a mock Jellyfin covering a wrong password refused, the token on every call, search, album, zone playback, stream and cover relay, playlist import and sign-out.
  - The owner's server (jellyfin.riginode.xyz, Jellyfin 10.11.11) answers the sign-in correctly. The live library check waits for a login.
- **Fix:** the Subsonic source no longer puts its signed request query into a playback error message.
- **Slimproto works with real squeezelite** (infra item 2; it was tested only against a fake player before, and no real player had ever played). CI now runs real squeezelite on every push: 16/44.1 and 24/96 come out bit-exact from the first sample, the position follows real time, pause is silent within a second, play resumes, and a player that reconnects keeps its one zone. Fixed:
  - A connecting player crashed its own session (the zone was created outside the async runtime). Its reconnect then got a new output while the zone kept the dead one ("Broken pipe"). Each player now has one output for the node's lifetime, and reconnects attach to it.
  - squeezelite starts at volume 0 and played silence: the node now sends the volume on every connection, and remembers volume changes made while the player is away.
  - `strm` sent autostart and transition fields as raw bytes where squeezelite reads ASCII digits.
  - The WAV header was played as audio (a click at every start): players fetch `stream.pcm`, the raw PCM.
  - The node handed a whole track to the player's buffers at once, declared it played, and cut it off: it now waits until the player reports the end (`STMu`), and stays at most 5 s ahead of what the player has played. The position comes from the player's own reports.
  - Pause, seek and skip sent `strm f`, which squeezelite applies only to the next track: they now stop the player (`strm q`) and start a fresh stream.
  - Groups of Slimproto players are "loosely synced" for now: each starts when its own buffer fills. Starting them together is next.
- **Fix: on Android the status bar covered the top of the node's page** (the menu and search). The app draws edge to edge, and older Android WebViews tell the page nothing about the bars. The app now keeps the page clear of the status bar, navigation bar, camera cutout and keyboard, with bars that follow the system's light or dark mode, as the UI does (a theme picked in Settings › Appearance against the system's still gets the system's bar colour). The emulator test checks that the menu sits below the status bar.
- **Fix: on Android the status bar covered the top of the node's page** (the menu and search). The app draws edge to edge, and older Android WebViews tell the page nothing about the bars. The app now keeps the page clear of the status bar, navigation bar, camera cutout and keyboard, with dark bars to match the UI. The emulator test checks that the menu sits below the status bar.
- **Long playlists are v1 collections too** (owner decision). The 100-song cap is gone: a playlist of any length (up to 10,000 songs on the node) syncs, edits, publishes and opens as a v1 collection. A longer collection is checked in parts that each fit the spec's limits, so every other rule still applies. Playlists held back in the old private format move to v1 on the next sync (the old files stay). The "stay in the old format" sync note is gone. ADR 0014 amendment. Tests: a 2,500-song unit test and a 300-song testnet test (sync, edit, second node, published copy opened without a session).
- **Your server, checked against a real Navidrome (0.64.2).** Connect, search, album pages and zone playback all work. A server that can't transcode (Navidrome without ffmpeg) answered the browser's MP3 request with an error, so the browser stream now falls back to the original file (FLAC plays in browsers). Empty MusicBrainz and ISRC fields are read as "not known".
- **Your own music server as a source** (step 5.1, OpenSubsonic client): Navidrome, Airsonic, Gonic, Ampache, LMS or another Pubky Music node.
  - Sources › "Your server" connects with an address, user and password. It's checked with `ping` first, and the tab takes the server's own name ("Navidrome").
  - The password stays on the node and never comes back from the API. Every request is signed with a fresh token (md5 of password and salt).
  - Search (albums and songs, also in the one search), album and song pages. Covers and browser streams are relayed by the node.
  - Zones play the original file (`subsonic:track:<id>`, signed at play time).
  - The server's playlists import as private Pubky playlists, by each song's MusicBrainz id or ISRC.
  - The node's own Subsonic server now lists ISRCs (OpenSubsonic `isrc`).
  - API: `GET|PUT|DELETE /v1/sources/subsonic/server`, `…/stream/{id}`, `…/cover/{id}`, `…/playlists`, `…/playlists/{id}/import`.
  - Test: two nodes, one serving Subsonic on a real port and the other connected to it (wrong password refused, search, album, zone playback, browser stream, playlist import, forget).
- **Fix:** a search opened directly (a link or a reload) showed no chips for the services. The service list now loads on the search page too.
- **Jamendo sign-in** (step 5.4, part 2).
  - Sources › Jamendo has **Sign in to Jamendo** (OAuth, scope `music`, redirect `/jamendo/callback`). The sign-in is optional: search and playback need no account.
  - **Add my Jamendo likes to Favourites** brings the tracks you liked or favourited on Jamendo.
  - Tokens last two hours and renew with the refresh token.
  - A Jamendo song page now uses Jamendo's own title, artist and cover instead of the web page title Pinakes read.
  - API: `GET /v1/sources/jamendo/status`, `POST …/login`, `POST …/logout`, `POST …/collection/import`.
  - Test: a mock Jamendo covers sign-in, a stale sign-in state, an expired-token refresh, the import (deduplicated) and sign-out. The live sign-in waits for the owner's Jamendo account. Shots: `docs/ux/shots/jamendo/`.
- **Jamendo** (step 5.4): Creative Commons music, full length and lossless, no account needed.
  - Search (albums and tracks) in Sources and in the one search; album and track pages; the licence on each item.
  - Tracks play in zones as FLAC (`jamendo:track:<id>`, falling back to MP3), and in the browser through a node relay.
  - The stream URL carries the app's `client_id`, so it's looked up only when a track plays and is never stored, shown or logged. A test checks responses for it.
  - `jamendo.com/track|album|artist/<id>` links are music links now (ADR 0001 service URLs). They join playlist resolution, the play order (`jamendo` after Spotify) and item pages.
  - Config: `[jamendo] client_id / client_secret` or `PM_JAMENDO_CLIENT_ID` / `PM_JAMENDO_CLIENT_SECRET` (the dev node's start script loads them).
  - Checked live: search, a track page (FLAC, CC BY-NC-ND), and the browser relay (full MP3, no redirect).
  - `docs/STREAMING-PROVIDERS.md` gains Jamendo and the providers not offered, with the reasons.
- **Open playlists other people share** (step 3 leftover).
  - Playlists has "Open" for a pasted `pubky://` link or a Pubky key. A key lists that person's shared playlists.
  - A shared playlist shows where each song plays from on your node: the author's pins count as suggestions, and unavailable songs stay greyed in place.
  - It plays, and **Save a copy** makes a private copy that keeps the pins and the order.
  - Reads need no Pubky session. Their data is validated as hostile input (spec public-root rules with the author in scope, then the node's playlist rules).
  - API: `GET /v1/shared/resolve?link=`, `GET /v1/shared/{user}/playlists[/{id}]`, `POST /v1/shared/{user}/playlists/{id}/copy`. Paths `/shared/<key>[/<id>]`.
  - Tests: link parsing; `tests/testnet_sync.rs::another_node_opens_and_copies_a_shared_playlist` (a second node with no session); a browser check. Shots: `docs/ux/shots/playlist-sources/shared-*.png`.
  - Other people's reviews and listens are left for the separate indexer (owner decision).
- **Share a playlist on Pubky** (step 3 leftover). The playlist page's "On Pubky" section has **Share on Pubky**. The next sync publishes the current version under `/pub/social/v1/posts/{id}/{editId}.json` (pubky-social-specs `plan_publish`; the private versions stay as the draft), and the page shows the `pubky://` link to copy. **Make private** removes every public version on the next sync. Only songs and their order go public, never audio; pins go along as suggestions. API: `PUT /v1/playlists/{id}/visibility {public}`. Test: `tests/testnet_sync.rs::a_shared_playlist_is_published_and_unpublished`, which also checks that the copy reads without a session.
- **Fix: the Android app could not open a node over http** (any `http://192.168.x.y:7788` address). It showed "Webpage not available, ERR_CLEARTEXT_NOT_PERMITTED" since the Pubky Ring sign-in was added (PR #5). The TLS verifier's Android library brings a network security config that allows plain http only to certificate-revocation hosts, and it overrode the app's cleartext setting. The app now has its own config allowing http. Found by the new emulator test, which now also checks that the node's page shows.
- **An Android phone is a zone** (infra brief §3.6). The app's built-in player now runs on Android too: once connected over the LAN, the phone registers as an endpoint, and the node lists "<model> · Speaker or headphones" (the app's player no longer offers a "Null output" zone). It plays through Android's audio path (AAudio), at 44.1 or 48 kHz up to 24-bit; the node converts higher rates, and the signal path shows the conversion. It follows Android's routing: speaker, wired, Bluetooth or USB DAC. A foreground service with a notification keeps it playing while the app is in the background; swiping the app away ends it. The app's log now goes to `adb logcat -s pubky-music`. CI runs the signed release APK on an Android 14 emulator after every release build: it connects, plays in real time, and keeps playing in the background.
  - The system-audio output moved to cpal 0.18 (AAudio instead of oboe), and the ALSA `hw:` output to alsa 0.11, which cpal 0.18 needs. A new CI job plays 16/44.1 and 24/96 noise and a FLAC fixture through the kernel's ALSA loopback and checks that the bytes come out identical.
  - A group with any endpoint on a system mixer (a phone, a Mac or a Windows PC) is "loosely synced": the system adds latency the node cannot see.
- **Light theme.** The web UI follows the device's light or dark setting. Settings › Appearance can pin Light or Dark in this browser; it's applied before the page draws, so there's no flash. Colours come from one set of theme variables. Shots: `docs/ux/shots/theme/` (Home, album, song, playlist, Settings, search; 1280 and 390, light and dark).
- **Navigation** (step 4 nav audit, STEWARD-UX-BRIEF-2):
  - Back closes an open menu, popover or dialog and keeps the page. Opening one adds a temporary history entry, and a page opened from inside a menu takes that entry over.
  - A click outside an open menu only closes it.
  - Item pages show a breadcrumb (Artist › Album › Song).
  - On a deep link, the back arrow goes to the parent page (song → album → artist) and is labelled with its name.
  - The player bar's title opens the song's page, and its artist searches for the artist.
- **Control audit** (`tools/ui-controls.js`, new CI job `ui-controls` with headless Chromium). It clicks each distinct visible button on 16 views and fails when a click changes nothing. Buttons with lasting effects are skipped.
  - The first run clicked 438 controls and found 3 dead ones, now fixed: Radio "Search" and "Add renderer" (disabled until there is text, with the reason as tooltip) and Sources "Search Internet Archive" (now points at the search box with a short ring).
  - The renderer address field had no style; fixed.
- **Importing a TIDAL or Spotify playlist asks once** (step 4, ADR 0015). "Keep TIDAL" pins every song to its exact TIDAL (or Spotify) track. "Auto" lets each song follow your play order. Items stay canonical either way. API: `POST /v1/sources/{tidal,spotify}/playlists/{id}/import?keep_source=true`, and `POST /v1/playlists` takes `sources` (pins by position). Esc or Cancel imports nothing. Shot: `docs/ux/shots/playlist-sources/import-dialog-tidal-1280-dark.png`.
- **Where playlist songs play from** (step 4, STEWARD-UX-BRIEF §3, ADR 0015).
  - Each song row has a source chip (Library, TIDAL, Spotify, …), or "Unavailable" with the reason.
  - A click on the chip lists every source the song has on this node, plus "Auto (follow the playlist)". Picking one pins the song, shown by a pin mark on the chip.
  - A summary bar ("41 songs · 41 TIDAL") filters by source.
  - "Play from" orders the services, per playlist or for the node (Settings). Drag, the arrow buttons or Alt+↑/↓ reorder it.
  - Play queues every song from its resolved source.
  - Songs saved only as an ISRC or MusicBrainz id resolve through Pinakes (`key=isrc:`/`key=mbid:`) and TIDAL's and Spotify's own ISRC lookup. The owner's 41-song Spotify import went from 6 to 41 songs that play, named from the matched track.
  - The page shows songs at once and fills the chips when the lookups finish (about 30 s the first time for 41 songs, 0.5 s after that).
  - Pins and the order sync as the collection's `ext.music` hints.
  - Fixed along the way:
    - A TIDAL or Spotify track link was named after its album ("Deutschrap Party 2024 by STOKED"); now it is named after its song. The describe cache was bumped.
    - Playlist rows' move-up arrow pointed down, and their buttons stacked on phones (styles existed only for the queue).
  - Tests: resolution and order unit tests, `tests/playlists.rs::items_resolve_their_source_with_pins_and_orders`, the v1 round trip of pins and order, and `tools/ui-playlist-sources.js`. Shots: `docs/ux/shots/playlist-sources/`.
- **Fix: controls that did not match the design** (owner report, STEWARD-FIX-design-lint.md). The item page's Everywhere / This version switch was grey browser-default buttons with an emoji globe, because `.scope` had no style and its buttons had no class.
  - The switch is now the app's chip control (the same as the search scope chips), with icons from the icon set, `aria-pressed`, one tab stop and arrow keys.
  - New `tools/design-lint.js` runs in CI (web-ui job). It fails on a `<button>` without a component class, an emoji or symbol used as an icon, or a class with no rule in `app.css`.
  - It found and this change fixes:
    - the album link in track rows (grey default button);
    - the play buttons on Versions chips (`.chiplink` was only styled inside tag chips);
    - the "Tag my whole library" row in Settings (`.row` had no rule; its input had no class);
    - glyph icons (▶ ♪ ✓ ✕) replaced with icon-set icons (a new check icon);
    - eleven classes with no rule (`placeholder`, `lnk`, `blurb`, `filter`, `nav-group`, …).
  - The rating stars keep their own styled glyph.
  - `tools/ui-item-pages.js` checks the switch. Shots: `docs/ux/shots/design-lint/`.
- **Fix: a node with an old Pubky session can be approved again** (owner report, STEWARD-FIX-resign.md). A session approved before the node used `/pub/pubky.music/`, `social/v1` and `music/v1` made Sync fail with a raw "403 Forbidden - Session does not have read access to path", and the Pubky card hid the sign-in button once signed in.
  - `GET /v1/pubky` compares the session's granted capabilities with what the node asks for and reports `needs_resign` and the `missing` folders. A homeserver refusal sets it too.
  - The Pubky card always offers **Sign in again** (the same Pubky Ring flow; the new session replaces the old one). When the session is short, a plain notice explains why, Sign in again is the primary button, and Sync is disabled with the notice as its tooltip.
  - A refused sync ends with that one sentence instead of the raw 403, and unsynced objects stay queued.
  - Home and the item page's "Yours" section (tags, rating) show the same one-line notice with the button.
  - Tests: unit test for the capability check; `tests/testnet_sync.rs::an_old_session_asks_to_sign_in_again` (a real homeserver approving the old folder list, then the full one); `tools/ui-resign.js`. Shots: `docs/ux/shots/resign/`.
- **Ratings in half stars.** Ratings are 1–10, shown as five stars with halves on every item page. Hovering previews the rating; a click on a star's left or right half sets it; the same click again clears it. `PUT /v1/reviews` takes `rating: 1–10`. The node's stored envelope moved to version 2, and older 1–5 ratings (on the node and in old homeserver files) read doubled. A review can be a rating alone, including one pulled from another client. `tools/ui-item-pages.js` checks hover, save and clear. Shots: `docs/ux/shots/ratings/`.
- **Music data moves to pubky-social-specs v1** (steward plan step 3, ADR 0014). The node keeps its working copy and converts at the homeserver:
  - Playlists become private social `collection` posts (`layout: list`, each song's added time in `ext.music.added_at`).
  - Rated reviews become music `review` posts (stars doubled to 1–10). Text-only reviews become social notes that embed the music URL.
  - Plays become private music `listen` posts. Favourites become `/priv/social/v1/bookmarks/`.
  - Edits are new versions. A delete removes every version and the old copy.
  - The first sync on a node reads the old `/priv/pubky.app/` folders, moves what v1 lacks, and keeps the old files. Playlists over 100 songs (the v1 collection cap) stay in the old format for now; the sync report says so (`notes`), and the steward has the question.
  - Checked against a local Pubky testnet: round trip, edit, delete and the one-time migration.
  - The sign-in already asks for the new paths. A node signed in before today needs one Pubky Ring re-approval.
- **`pubky-music-specs`** (new crate, steward plan step 3, ADR 0014): music kinds on the pubky-social-specs v1 post envelope, under `/{pub,priv}/music/v1/posts/`. A `review` is a rating from 1 to 10 with optional text; a `listen` is a private play record (`played_at`, `duration_ms`, `source`). Both embed the music URL. Also readers and writers for the `ext.music` hints on playlists (`source` on an item, `source_order` on the collection) that drop anything malformed. Nothing in the node uses it yet; the migration comes next. ADR 0014 supersedes ADR 0006.
- Zones say how closely their speakers stay together (`sync`: `single`, `tight` or `loose`). Groups of the node's own endpoints or of Slimproto players, Snapcast groups, Sonos groups and Cast groups are tight; mixed protocols are loose. `docs/ZONES.md` maps zone kinds, labels and actions for the Zones view.
- **AirPlay** (`crates/pm-out-airplay`, infra brief §3.4): AirPlay 1 (RAOP) receivers found over mDNS (`_raop._tcp`) become zones, unless `[outputs] airplay = false`.
  - The node sends unencrypted Apple Lossless (verbatim frames), 44.1 kHz 16-bit, over RTP. It answers the receiver's timing requests, sends sync packets, resends lost packets, and sets volume and title/artist/album as text.
  - Receivers that require encryption are skipped, and the log says why. AirPlay 2 is not built, because its pairing is only reverse-engineered.
  - Checked with shairport-sync: discovered, played in real time, and a pattern came out bit-exact.
- **Snapcast** (`crates/pm-out-snapcast`, infra brief §3): with `[outputs] snapcast = "<host>"`, every group on that snapserver becomes a zone. Playing adds a stream to snapserver over its JSON-RPC (snapserver connects to the node and reads raw PCM in the zone's exact format, FLAC on the wire), routes the group to it, and paces the music in real time. Stopping gives the group its previous stream back. Snapcast keeps Pis and old phones in tight sync. Checked with real snapserver 0.34 and snapclient: the zone played and a pattern came out of the client bit-exact at 16 and 24 bits.
- **Sonos** (infra brief §3).
  - A Sonos player that is a member of a Sonos group plays through its group's coordinator. Members refuse transport commands, so the whole Sonos group plays, as the Sonos app shows it.
  - A node zone made only of Sonos players groups them Sonos's own way (`x-rincon:` join, tight sync by Sonos) instead of streaming to each, and they leave the group when the zone stops.
  - Sonos zones are capped at 16-bit 48 kHz, and the signal path shows any conversion.
  - Tested against fake players.
- **Smart TVs over DLNA** (infra brief §3).
  - The node asks each renderer what it accepts (`ConnectionManager.GetProtocolInfo`) and offers WAV under the MIME type it listed. TVs that take only DLNA LPCM get `stream.l16`; the zone converts to 16-bit 44.1/48 kHz, and the signal path says so.
  - A renderer that takes no PCM stream gets no zone.
  - DIDL metadata now carries the title, artist, album and cover (`upnp:albumArtURI`) and full DLNA protocolInfo (`DLNA.ORG_PN=LPCM`, live-stream flags).
  - The stream answers with `transferMode.dlna.org` and `contentFeatures.dlna.org`, and a HEAD no longer takes the live stream from the GET that follows.
  - A Stop goes before SetAVTransportURI, and Play is retried while a TV still loads (error 701).
  - Checked with gmediarender limited to LPCM only and to MP3 only.
- **Cast plays like a Cast sender should** (infra brief §3; `docs/DEVICE-TEST-PROMPT.md` lists what to check on real devices).
  - While a zone plays, a session keeps the link to the device. It answers and sends heartbeats, so STOP and volume still reach the TV after the first seconds.
  - After the connection drops, the session rejoins the same Cast session, or launches and loads again.
  - When another sender takes the TV, the session steps aside and never stops YouTube.
  - When the receiver reports a media error, the session loads the stream again.
  - The TV shows the track's title, artist, album and cover: from the library for library tracks, through a keyed `/v1/art/{album}` URL, and the source's own art for streams.
  - Cast groups are labelled "(Cast group)", and discovery prefers IPv4.
  - Outputs get a `now_playing` hook (`pm_zones::TrackMeta`) for the TV and DLNA work that follows.
- Fixed: **Cast devices and UPnP/DLNA renderers could not play from a node with an API token.** They were handed the stream URL without credentials, and the node answered 401. Each pull stream now has its own key in its URL (`?k=`, derived from a secret kept in the library, so URLs survive restarts). The key opens that stream only. Slimproto players also get keys instead of the API token, and `GET /v1/outputs/pull/{name}/url` gives the URL for manual use. Checked with gmediarender, a real DLNA renderer, on a token-protected node: it fetches and plays the zone.
- **Remote access over iroh** (ADR 0013, `crates/pm-remote`, `docs/CONNECTION.md`).
  - With a Pubky session, the node runs an iroh endpoint with its own key and pipes the `pubky-music/http/1` ALPN to its API. It publishes the key in the private node record. `[remote] enabled` (default on) and `[remote] relay` (empty = n0's public relays, a URL, or `none`) control it.
  - The app reaches the node away from home: when neither the saved address nor the LAN answers, it opens a local tunnel by key, with relay fallback, and shows the node through it. Every request still needs the token.
  - `cargo run -p pm-remote --example tunnel` tries it by hand.
- **Apps find nodes on the LAN** (`crates/pm-discovery`, `docs/CONNECTION.md`).
  - The node announces itself over mDNS (`_pubky-music._tcp`, `role=node`) and SSDP (`urn:schemas-pubky-music:device:Node:1`), with a UPnP description at `/upnp/node.xml`, so it also shows in Windows' Network folder. `[api] announce = false` turns this off; nothing is announced when the API listens on loopback.
  - The app's connect page lists the nodes found under "On this network".
  - `/v1/info` returns a stable node `id`. When the saved address stops answering, the app finds the node by that id and follows it to its new address.
  - Endpoints now announce `role=endpoint`.
- **The egui app (`pm-app`) is retired** (owner decision, ADR 0012): it left the workspace, CI and the release archives. The Pubky Music app (Tauri) and the web UI are the controllers; `pm-endpoint` is the headless player. The Windows and macOS archives now ship `pm-endpoint` with the system's audio output.
- **The connection survives outages** (infra brief §2, `docs/CONNECTION.md`).
  - The node numbers every `/v1/events` message (`seq`). The snapshot carries `boot` (new on every start) and `library_rev`.
  - The node answers `ping` and `resync`, pings sockets itself, and drops silent controllers after 45 s. A new zone is announced at once.
  - The web UI reconnects with backoff and jitter. It resyncs from the snapshot without a reload, and reloads the library views when the node restarted or the library changed.
  - The UI shows the round-trip time, and connected, reconnecting or offline with a "Retry now" banner.
  - A change made while disconnected waits up to 8 s and then goes out, or is refused with a message. It never fires later.
  - `PM_NULL_OUTPUT=realtime` makes the null output play in real time.
  - `tools/ui-reconnect.js` kills and restores the node under two browser controllers. Resync takes 0.3–1.7 s after the node is back.

## 2026-10-20

- The node's Pubky sign-in also asks for the v1 namespaces it moves to next (`/pub|priv/social/v1/`, `/pub|priv/music/v1/`), so the one new approval the tag fix needs covers them too.
- **Home replaces Discover** (owner; STEWARD-UX-HOME.md): Jump back in (local), Trending songs / albums / artists with Today · This week · This month, From people you follow (needs the node's Pubky session), Tags people use for music (chips open `/tag/{labels}`, which combines tags and shows co-tags), Concerts near you (a city in Settings or on the card, looked up once on OpenStreetMap; `near` + 50 km, next 30 days), Podcasts, Yours on Pubky, New on Pubky. Each section is one Pinakes call (`GET /v1/home/{section}`, cached 60 s), renders as it arrives with skeletons, says in one line why it is empty, and fails on its own. "See all" opens `/home/{section}`. `/discover/…` paths redirect. Item pages gain "Upcoming concerts" (events keyed by the artist, once Pinakes keys performers) and "People who tagged this also tagged" (once Pinakes announces `related` in `/v1/info`); both stay hidden until then.
- **One search, scoped and streamed** (`GET /v1/search?q=&in=`, NDJSON, one line per source as it answers, 8 s per source, cached 5 min): the library answers in milliseconds, then Pubky and each connected service; scope chips (Everywhere · Library · Pubky · each service) show a spinner, then a count, or "didn't answer · Retry"; a progress bar under the box; results as Top result, Songs, Albums, Artists, Playlists, Podcasts, Concerts, with the same song from several sources as one row with its marks (Pinakes group, ISRC, UPC). The scope is in the URL; a pasted link opens its item page. Sources no longer has its own search boxes.
- Fixed: Home's "From people you follow" and "Yours on Pubky" sent the owner's key with its `pubky` prefix, which Pinakes refuses.
- **Fixed: the node's tags never reached Nexus or Pinakes** (steward). Tags on external links were written under `/pub/pubky.app/tags/`, which Nexus only indexes for `pubky://` targets. They now go to the node's own namespace, `/pub/pubky.music/tags/<id>` (same `PubkyAppTag`, same id), where Nexus indexes them as universal tags; the sync moves old ones (idempotent, counts logged) and reads both paths meanwhile. The node now asks for `/pub/pubky.music/:rw` at sign-in: an existing session must be approved once more in Pubky Ring (the sync says so). ADR 0011 amended.
- **One item page for songs, albums, artists and playlists, from every source** (owner report; `docs/ux/08-item-page.md`, wireframe `docs/ux/12-item-page.html`). `GET /v1/item/{kind}/{id}` answers the same model for the library, TIDAL, Deezer, Spotify and Pubky-only links: hero (Play, Shuffle, Play on…, ♥, ⋯), Songs (an album's tracks, an artist's top songs and albums), Versions, On Pubky (Everywhere / This version), Yours (tags, rating, notes) and About. Albums that only a service has list their songs (the owner's HYDRA 3D on TIDAL: 15 songs); when the chosen service lists nothing (Spotify refusing the app) the page's own link, then Deezer's previews are used; else MusicBrainz by the card's MBID (owner's scope decision: Pinakes carries no tracklists). One song row and one ⋯ menu everywhere: the title opens the song page, the artist the artist page, the album the album page. Every page has a path (`/song|album|artist/lib-<n>|mb-…|isrc-…|u-…`, `/playlist/…`, `/genre/…`, `/source/…`), served on reload; Back is history, labelled with the previous page ("‹ Results for “radiohead”", "‹ Radiohead"), and on a deep link goes to the page's section. The old library album/artist and Discover item views are gone.
- **Find on other services**: when an item is only on one service, the node searches the connected services by title and artist and shows "possibly the same"; confirmed matches go into the node's own match store (never to Pinakes) and count for Versions and playback (`GET /v1/item/candidates`, `POST /v1/item/matches`).
- Copy: "No one has tagged this on Pubky yet."; the Tags hint moved into a "Public on Pubky" tooltip. Search artist chips use service marks.
- **Write IDs into my files** (identify brief; off by default, Settings › Identify library): the recording MBID, the release MBID found by folder matching and the AcoustID id go into the file's own tags (FLAC Vorbis comments as Picard names them; WAV/AIFF ID3 `TXXX`), after a JSON backup of the tags it touches in `<data>/tag-backups/`; the audio is never rewritten (tests decode before and after). ALAC/MP4 and DSD are left alone and reported. Happens after identification or confirmation when on, or now with "Write now" (`PUT /v1/library/identify/settings`, `POST /v1/library/identify/write`).
- **Confirm matches, album by album:** "Compare with the album" shows the folder's files beside the MusicBrainz release's tracklist, aligned by disc and position, with ✓ for matching suggestions and a note when lengths differ, and "Accept all N matching".
- The MusicBrainz proxy waits and retries once when MusicBrainz answers 503 (its rate limit), instead of passing the error on.
- **Identification verified against the real AcoustID service:** a recorded answer (Bohemian Rhapsody, 133 release groups) is now a fixture and lands on a real 12-track release for a 12-file folder; `tests/live_acoustid.rs` (ignored; key and files from the environment) fingerprints real files and looks them up. On four Creative Commons tracks from Jamendo: one identified by fingerprint, two known to AcoustID without a MusicBrainz recording (left to suggestions), one unknown.
- Item pages: "Also on" rows use service marks instead of text labels, and every row the node can stream plays, albums included (through the resolver: a TIDAL album row queues its tracks).
- **Android app**: every release run builds a signed APK (arm64, and universal for older phones and emulators) and an AAB from the same Tauri project; it connects with Pubky Ring or by address like the desktop app, works on Google TV, and reaches LAN nodes over plain HTTP. **Download** section in the README with direct links to the latest release and edge; `docs/RELEASING.md` describes the workflow, signing and the release key; `docs/APP-TEST-PROMPT.md` is the checklist for real devices.
- **Versions of identified files** come from their MusicBrainz URL: library track Versions now use the canonical identity (tags, fingerprint or confirmation; before, only IDs in tags counted), ask Pinakes for that MusicBrainz URL's own card first (MusicBrainz links are hubs since pubky/pinakes#24; unknown ones are read on demand), then everything tagged with the key.
- **Builds on every push** (`.github/workflows/release.yml`): each push to `main` replaces the `edge` pre-release and image; `v*` tags make a versioned release. Every run builds the app for macOS (universal `.dmg`), Windows (`.msi`, setup `.exe`) and Linux (`.AppImage`, `.deb`, x86_64 and arm64), the node and endpoint for Linux x86_64, arm64 and 32-bit Raspberry Pi OS (armv7), Windows and macOS, and the multi-arch image. Desktop builds are unsigned until signing certificates exist.
- Fixed: the ALSA output did not compile on 32-bit ARM.
- **The app is Tauri 2** (`crates/pm-desktop`, ADR 0012): the same shell (connect with Pubky Ring or by address, then the node's own UI, plus the built-in player on desktops) now bundles for macOS, Windows, Linux and Android. The connect page reconnects to the remembered node at start and offers "Try again" or "Connect to another node" when it does not answer; the token reaches the node's page directly, so the UI no longer asks again after connecting. Only the connect page has IPC; links to other sites open in the system browser. The egui app is frozen (bug fixes only), with its retirement proposed.
- **Drag to reorder the queue** (web UI, node zones and the browser zone): a ⋮⋮ grip on each row works with mouse, touch and pen; the row follows the pointer, the rows in between slide aside, and the list scrolls near its edges. Alt+↑/↓ on the grip moves by keyboard; the playing track keeps playing. `tools/ui-drag.js` checks it in a real browser at 1280 and 390.
- **☰ works at every width** (owner bug): on desktop and tablet it collapses the sidebar to icons (or expands it), remembered per browser; on phones it opens the sidebar as a sheet that closes on back, an outside tap, Esc or a pick. Icon-only nav items have tooltips. `tools/ui-smoke.js` checks both.
- **Item pages never fail on an unknown link** (owner bug "Pinakes has not seen this link"): the node gets cards through by-uri, then asks Pinakes to read the link (`POST /v1/resolve`), paced under 30 a minute, cached, and one resolve per link at a time (`pm-api/src/pinakes_reader.rs`). While Pinakes reads, the page shows the provider's own data with a quiet "Reading this link…" and refreshes. Rows Pinakes did not know are resolved in the background. A card's own link is no longer labelled "tagged on Pubky" unless someone tagged it, and it plays before other editions on the same service.
- **Real paths and back** for the web UI (navigation brief, first part): `/song|album|artist/<id>` (`mb-<mbid>`, `isrc-<ISRC>` or `u-<base64url of the link>`), `/search?q=`, and `/<view>` for the top views; the node serves the UI on them (deep links and reloads land on the same page). Back and forward restore the view, the search query and the scroll; opening a page pushes, changing the query replaces. The item page's back arrow names where it goes. `tools/ui-smoke.js` checks item → back, search → item → back, and a deep link.
- Pinakes fixtures re-recorded from the live #21 build (8092), OpenAPI candidate included. MusicBrainz recording cards that Pinakes now puts in charts without links are filled by `key=mbid:` before resolving, so they play.
- **UX wireframes, phase 1b** (`docs/ux/06…11`): search with the Everywhere / My library scope, artist, album and song pages with Wikipedia and MusicBrainz facts and source labels, the Everywhere / This version tag switch, the review composer, and Copy to my homeserver.
- **Favourites, playlist items and history from other services are real rows** (owner finding): `POST /v1/discover/describe {uris}` names any music URL outside the library with title, artist, cover and where it plays from, through one Pinakes batch lookup, then the provider's own API for that id (TIDAL, Deezer, Spotify); cached a day. A URL is never a name ("Untitled on TIDAL" when nothing is known). `GET /v1/discover/play` now also plays links Pinakes has not seen through the provider (a favourited TIDAL album plays its tracks from TIDAL). Web: Favourites › "From other services" and playlist rows get title, cover, Play, the menu and the Plays-from chip.
- Global search shows library results at once, then streaming services and Pubky as they answer.
- **Identify library** (steward plan step 2): untagged files get a MusicBrainz identity. Chromaprint fingerprints in-process (`pm_audio::fingerprint`, no `fpcalc`), cached by a content key; AcoustID lookups paced at 3 a second with the answer cached; a single recording at score ≥ 0.9 is accepted (`fingerprint`), several are disambiguated by the folder as a release (track count, positions, ±3 s: `fingerprint+release`); everything else becomes suggestions, including a MusicBrainz search by the tags, which the owner confirms (`confirmed_by_user`) on a review screen. Suggestions never count as identity. Identities found this way survive rescans; tag IDs always win. `GET/POST /v1/library/identify`, `GET /v1/library/suggestions`, `POST …/{id}/accept`, `DELETE …/{id}`; config `[acoustid] api_key` / `PM_ACOUSTID_KEY`. Web: a "?" mark on unidentified rows, an Identify library card in Settings with progress, and "Confirm matches". Saving a queue with unidentified tracks now says how many and where to identify them.
- **Item pages play** (steward plan step 1): every Discover row, tile and item page has a Play that resolves through the node's connected services (`GET /v1/discover/play`): the library when the recording or album is owned (MBID, ISRC, UPC), else the best service link of the Pinakes group, tagged first, TIDAL before Spotify Connect; an album card plays its tracks from that service's album (an Apple Music card for "Dracula" plays from TIDAL 495402659). With nothing connected, Play offers "Connect TIDAL" and a 30 s Deezer preview. Rows and item pages show the answer as a "Plays from" chip.
- **UX wireframes, phase 1** (`docs/ux/`, screenshots in `docs/ux/shots/`, served at `/ux/` on debug builds): one menu with "Open on ›", the item page (Plays from, Available on, Tags on Pubky apart from About, Versions), a mixed playlist with source chips, a pin, an unavailable row and the source popover, the play-from order editors, and the import dialog. Review notes in the owner's `UX-REVIEW.md`.
- Fixed: the desktop shell did not compile (a missing field in the `PM_NODE` settings path).
- **The desktop app is the web UI plus a player** (`pm-desktop`): one connect step (Sign in with Pubky Ring finds the node through your homeserver; an address and token under "Or connect by address"), then the node's own UI in a native window. The app serves this machine's DACs to the node as a `pm-endpoint` (bit-perfect ALSA `hw:` on Linux, the system output elsewhere) and registers itself with `POST /v1/outputs/endpoints`, so "<this machine> · <DAC>" is an ordinary zone in the UI with the node's signal path. An old address from another network is forgotten on the next registration; `DELETE /v1/outputs/endpoints/{address}` removes an endpoint and its zones. Saved endpoint addresses are restored at start and backed up with the owner's settings.
- Web: service badges on Discover and Sources tiles and search rows are glyph icons in a round badge instead of a text overlay; the cover placeholder rule no longer stretches badges over the whole tile.
- Native egui app: Settings → Node is one step (connected: name and address with Scan/Refresh/Sync; otherwise a "Sign in with Pubky Ring" button, address and token under "Connect by address"); the session-secret field is gone (`PM_SESSION_SECRET` still works for scripts). Output enumeration happens once when Settings opens, not every frame (it was probing the DAC that was playing ~16 times a second), with a Rescan button; track titles no longer wrap into a narrow column.
- **Spotify** (`pm-api/src/spotify.rs`): sign-in with PKCE, catalogue in the unified search with ISRC/UPC (merges with TIDAL and Deezer rows), album and artist pages, Versions by ISRC/UPC, saved tracks/albums and playlists imported by ISRC, and "Play on Spotify" on the user's Connect devices (no audio enters the node). Config `[spotify]` / `PM_SPOTIFY_CLIENT_ID|SECRET`.
- **MusicBrainz caching proxy** at `/mb/ws/2/…` (no token): one upstream request per second, a week of cache, the node's contact in the User-Agent; for the node itself (not offered to Pinakes as a `base_url`, per the owner).
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
