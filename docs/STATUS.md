# Status against the handoff milestones

Updated 2026-10-20. "Verified" means run against real hardware or a live upstream; "built" means
implemented with automated tests but not yet exercised outside this repository's CI.

| # | Milestone | State | Notes |
| :-- | :-- | :-- | :-- |
| M0 | Workspace, `Output` trait, null/WAV outputs, CI | verified | |
| M1 | Library + local bit-perfect playback | verified | Covers: embedded art, folder images, and the Cover Art Archive by release MBID when the files carry none (fetched once, cached). Artist pages: portrait and biography from Wikidata/Wikipedia by artist MBID, cached.  FiiO KA17: 16/44.1, 24/96, 24/192 bit-perfect, `snd-aloop` byte-identical (PR #1). Native DSD silent on that DAC, cause open. |
| M2 | Endpoint daemon, own protocol, gapless, groups | built | `pm-stream`/`pm-endpoint`/`pm-out-net` (ADR 0007), mDNS, loopback test bit-exact. Groups: fan-out with latency pre-roll, drift reporting and sample-slip drift correction (ADR 0009; simulated-clock test). Needs a Pi run and a two-endpoint sync measurement. |
| M3 | Native API + minimal client | built | `/v1` HTTP + WebSocket; a built-in web UI at `/` (feature parity with the app: zones, groups, DSP editor, radio, favourites, tags, reviews, genres, folders, contact; redesigned 2026-10-20: sidebar, cover-first tiles, hover row actions, player bar with Now Playing overlay, zone picker, responsive; `tools/ui-smoke.js` renders every view headlessly); `pm-app` (egui; shared design tokens with the web UI in `crates/pm-app/src/theme.rs`: dark palette, one accent, rounded covers with placeholders, nav rail, round play button, quality pill, primary/secondary buttons) with a Roon-style Home (recently added/played, most played, history), Now Playing screen, browse, search, queue, zones, volume, signal path, playlists, discover, DSP. Play history is node-local (`plays` table); node zones record automatically, the app's local zone and the browser report theirs. Zone queues and repeat modes are saved on every change and restored (stopped) when the node restarts. Never run on a display by its author: `docs/LAPTOP-TEST-PROMPT.md`. |
| M4 | OpenSubsonic | built | XML and JSON; ID3 browsing, genres (getGenres, byGenre, getSongsByGenre), starred, internet radio stations (shared with the Radio screen), search, stream with ranges, cover art, playlists, scrobble. Needs a client run (Symfonium, Feishin, Amperfy). |
| M5 | Remote access by pubkey | partial | Node publishes its address privately on the homeserver; app finds it from a session secret (ADR 0008). No Pubky live transport or relay exists yet; wire stays HTTPS. |
| M6 | OpenHome / UPnP (Linn DSM) | built | SSDP discovery, AVTransport (SetAVTransportURI/Play/Stop) and RenderingControl volume; renderers fetch the zone's live WAV pull stream (bit-exact, tested against a fake renderer). Needs a real DSM/WiiM/Bluesound run; OpenHome-specific services (Playlist, Product, standby) not yet. |
| M7 | Slimproto, AirPlay 2, Chromecast | partial | Slimproto server (players/squeezelite on 3483, `strm` to a pull WAV stream, `audg`, `STAT`, discovery responder; fake-player test). Chromecast sender (mDNS, Cast v2 over TLS, default media receiver LOAD of the pull stream, volume; ceiling 24/48 honest; fake-receiver test). AirPlay 2 not started (pairing and encryption are out of scope for now). |
| M8 | DSP engine | partial | Sample-rate conversion (`Lossy`, FFT sinc), parametric EQ, convolution from an IR WAV, headphone crossfeed, ReplayGain loudness (all `Enhanced`), headroom, software volume, TPDF dither; per-zone, persisted, editable in the app. Group drift correction is sample slip (`sync` stage), not adaptive resampling. Missing: PCM↔DSD, selectable SRC filters. Bypass is bit-perfect (tested). |
| M9 | Pubky social | partial | Objects are v1 `PubkyAppPost` envelopes in `/priv/pubky.app/posts/` (ADR 0006); playlists, reviews (ratings and notes on album pages), activity; favourites are v1 `PubkyAppBookmark`s in `/priv/pubky.app/bookmarks/` (ADR 0010; app, web UI and Subsonic star/unstar); the user's own tags are public v1 `PubkyAppTag`s in `/pub/pubky.app/tags/` written on explicit action and synced (ADR 0011; chips on album and artist pages, Discover by tag); private sync with the homeserver; private audio files. Sign-in through Pubky Ring (`pm-node login` with a QR, or the web UI / API flow) or an exported secret; public publishing waits for the social primitives. |
| M10 | Pinakes integration | partial | Trends, tags, friends' activity joined to the library in the app. Depends on Pinakes' music step (`key=`, `sort=trending`) landing upstream; client falls back meanwhile. |
| M11 | Roon migration + bridge | partial | File migration built and tested: M3U/M3U8, CSV and Roon's Excel export import (`pm-node import-playlist`, `POST /v1/playlists/import/file`, app and web UI), matching by path, file name, then title + artist. `sidecars/roon-bridge` (official Extension API: scrobble, live playlist import): its logic is unit-tested against Roon-shaped fakes (`npm test`) and the RoonLabs packages install from GitHub (they are not on npm); pairing with a real Core is still pending. |
| M12 | Streaming sources | partial | A `StreamingService` trait with two account-free services behind `/v1/sources/{source}/*`: the Internet Archive (items, best-format files, FLAC first) and Audius (independent artists, MP3 streams via its public API); Sources screens in app and web UI; archive.org and audius.co URLs are canonical service links. Internet radio built: `http(s)://` queue items decode through an HTTP media source (MP3/AAC/Ogg/FLAC, M3U/PLS followed), radio-browser.info directory search, saved stations, ICY stream titles in now-playing, Radio screens in app and web UI; the browser zone streams directly. Subscription services remain blocked on accounts and API terms (handoff §11); the source factory is the plug point. |

## Hard constraints (handoff §2) audit

1. No Roon protocol cloning: the Roon bridge uses only the public Extension API.
2. No public audio: uploads go to `/priv/pubky.music/files/` only (refused elsewhere); `/pub/`
   is never written with audio; track files are served only to authenticated callers.
3. Personal subscription: the node serves one user; no streaming integration exists yet.
4. Canonical IDs: playlists, activity and imports accept only canonical music URLs; library tracks
   without a MusicBrainz id or ISRC are refused from social data with an explanatory error.
5. Bit-perfect by default: every conversion is a named stage with a quality level; defaults add
   none; tests cover bypass, pad, dither, resample and EQ paths. Lossy sources (MP3, AAC, Vorbis)
   decode to 24-bit float output and mark the decode stage `Lossy`, so no bit-perfect claim is made.
6. Reverse engineering: none.

## Deployment shapes

Directly on hardware, Docker, Umbrel (`docs/deploy.md`); endpoints on other machines; the native
app on desktops; any OpenSubsonic client on phones.
