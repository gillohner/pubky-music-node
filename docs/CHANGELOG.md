# Changelog

Newest first. Dates are when the change landed on `main`.

## 2026-10-21

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
