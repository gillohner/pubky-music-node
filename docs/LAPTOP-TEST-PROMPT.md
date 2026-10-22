# Laptop test prompt: node, native app, endpoint, Subsonic client

Paste into Claude Code on the laptop (the one with the FiiO KA17), in the repo:

```
Pull main of https://github.com/gillohner/pubky-music-node and work in it. Read README.md,
docs/api.md, docs/deploy.md and docs/adr/0005..0007 first. Everything below was built on a machine
without a display, a DAC or a second host, so your job is to run it for real and fix what breaks.
Do not change the audio pipeline or the sample layout to make a test pass; fix the layer that is
wrong and say what it was.

Setup
1. Build with ALSA: `cargo build --release -p pm-node -p pm-endpoint --features alsa`.
   Free the KA17 from PipeWire/PulseAudio as in docs/HARDWARE-TEST-PROMPT.md.
2. `pm-node config > node.toml`; set `contact`, `[library] folders` to a real folder with FLACs
   (tagged with MusicBrainz ids where possible: Picard), `[api] token = "<something>"`.
   `pm-node -c node.toml scan`, then `pm-node -c node.toml serve` in a terminal.

Checks (pass/fail with evidence each)
A. API: `curl -H 'Authorization: Bearer <token>' localhost:7788/v1/info`; `/v1/outputs` must list
   `alsa:hw:N,0` (KA17) and `null`; `/v1/zones` one zone per output; a cover at
   `/v1/library/albums/<id>/cover` for an album with embedded art.
B. Desktop app (web UI in a window + built-in player): install WebKitGTK and ALSA headers
   (Arch: `webkit2gtk-4.1 alsa-lib`; Debian: `libwebkit2gtk-4.1-dev libasound2-dev`), then
   `cargo build --release --manifest-path crates/pm-desktop/Cargo.toml` and run
   `./crates/pm-desktop/target/release/pm-desktop` (or `PM_NODE=… PM_TOKEN=… pm-desktop`).
   Click "Sign in with Pubky Ring"; the UI opens and Zones shows "<hostname> · <DAC>".
   The node must be able to reach this machine on TCP 7790 (same LAN or routed VLANs).
   Albums grid with covers, album page, search, queue. Select the KA17 zone, play an album:
   transport bar shows position moving, the signal path badge opens the chain and matches what
   `/proc/asound/cardN/pcm0p/sub0/hw_params` says. Pause/resume/seek/next/previous. Volume slider
   moves the KA17's UAC2 volume (`amixer -c N`). Report every UI glitch (layout, lag, wrong
   state) with a screenshot; the UI has never been seen by its author.
C. Local playback in the app: select "This computer". It downloads the track from the node into
   ~/.cache/pubky-music and plays through the first ALSA device the app finds. Verify which device
   it picked (it should be the KA17 when PipeWire is stopped) and that the signal path is
   BitPerfect/Lossless with the same stages as on the node.
C2. Queue tools (web UI Queue page and the app's queue view): with an album playing on the KA17
   zone, press Shuffle (the playing track and the ones before it must not move), Save as playlist
   (a new private playlist appears; untagged files are reported as skipped), and Transfer… to
   the Laptop endpoint zone once D is up (playback continues there from the same position, the
   KA17 zone is left empty). From a track's ⋯ menu choose Start radio: the track plays and when
   it ends the queue grows with similar tracks. Media keys on the laptop must drive browser
   playback in the web UI (MediaSession). Drop a `.lrc` next to one track (same file stem), rescan:
   Now Playing in both UIs shows the lyrics and highlights the current line as it plays. Set a
   sleep timer from Now Playing (any length; the API accepts seconds for a quick check with
   `PUT /v1/zones/<id>/sleep {"seconds": 20}`): the zone pauses when it fires.
C3. Web UI extras: the Zones page (sidebar) must show every zone with transport buttons and a
   volume slider for the KA17 zone that moves `amixer`; Albums → Focus chips (Hi-res, DSD, CD
   quality…) must match what the files are, with a format badge on each tile; Home must show
   "New releases from your artists" once a contact is set (MusicBrainz, one artist per second);
   Settings → Download backup must produce a SQLite file that `pm-node restore` accepts; on a
   phone, "Add to Home Screen" installs the UI. In Symfonium (or any Subsonic client with
   jukebox mode), pick the KA17 zone under Settings → Subsonic jukebox and confirm the client's
   jukebox plays through the node.
C4. TIDAL (needs `[tidal]` credentials in node.toml and the app's redirect URI registered): on the
   Sources page pick TIDAL, search, open an album, Connect TIDAL and finish the sign-in in the
   browser. Then play a track to the KA17 zone: report the signal path (FLAC expected; say if it
   is a preview or refused as DRM), and try Import as favourites and one playlist import.
C5. Deezer (no credentials): search any well-known artist in the main search of both UIs. The
   results must show Deezer artists/albums/tracks after your library and TIDAL, with a DEEZER
   badge; an album the library owns (same UPC) must NOT be duplicated. Play a Deezer track to the
   KA17 zone and in the browser zone: a 30-second MP3 preview, the signal path says MP3. Open a
   library album → Versions: a Deezer chip appears when the album has a UPC; the track ⋯ menu
   offers "Stream from Deezer · …" when the track has an ISRC.
C6. Network scan: Settings → Outputs → "Scan the network". Report the list (every UPnP device,
   whether it is a renderer, Cast devices) and any notes. If a renderer you own is missing, say
   what the notes said.
D. Endpoint: in a second terminal `./target/release/pm-endpoint --name Laptop --listen
   127.0.0.1:7790`; restart `pm-node serve` with `[outputs] endpoints = ["127.0.0.1:7790"]`.
   `/v1/outputs` must show `net:127.0.0.1:7790/alsa:hw:N,0`. Play the same 16/44.1 and 24/96
   files to that zone and repeat the hw_params and byte-identical loopback checks from
   docs/HARDWARE-TEST-PROMPT.md check C via `snd-aloop` (the endpoint's ALSA output is the same code,
   only reached over TCP). Also test mDNS: start the endpoint without `--listen` and with
   `discover = true` on the node; say whether it was found.
E. Playlists: create one in the app, add tracks via the ⋯ menu (tracks without MusicBrainz ids
   must be refused with the explanatory message), play it on both zones, delete it.
F. Subsonic: install Symfonium or Feishin on a phone/desktop in the same network; server
   `http://<laptop-ip>:7788`, user `pubky`, password `<token>`. Browse artists/albums with covers,
   search, play a FLAC (should stream the original file), create and edit a playlist, check it
   appears in the app. Report which calls the client made that returned errors (`RUST_LOG=info`
   on the node).
G2. Squeezelite: `apt install squeezelite` (or a Pi with squeezelite), run
   `squeezelite -s <laptop-ip> -o hw:N,0 -n Test` with PipeWire stopped; the node logs a Slimproto
   player and a zone "SqueezeLite (<mac>)" appears. Play 16/44.1 and 24/96 to it, check hw_params on
   the player side and the signal path on the node (the player fetches the zone's WAV stream;
   `squeezelite -d all=debug` shows the strm header it received). Try the volume slider (audg).
G3. UPnP: if a WiiM, Bluesound, Linn or any DLNA renderer is on the network, it should appear as an
   output after start (or Settings → Rescan outputs). Play to it; report whether it starts within a
   couple of seconds, the format it shows, and whether Stop/Next behave. The web UI at
   http://<laptop-ip>:7788/ should show the same zones; try it on a phone too.
G. If you have a Pubky session secret (exported from Pubky Ring): set
   `pubky.session_secret_file`, restart serve, confirm the log says "Pubky session restored", then
   `pm-node upload some.flac` and check `/v1/pubky/files`; Settings › Accounts › Bring files here
   then downloads it into the library folder's `From homeserver` and scans it. Playlists appear under /priv/pubky.app/posts on the homeserver.

H. Newer screens (web UI at http://<laptop-ip>:7788/ and the app): Settings → set the upstream
   contact and add a library folder from the UI (watch it scan); Home (recently added/played,
   history after a few plays); Genres (needs genre tags); an album page: heart, tags (add one,
   it must appear under /pub/pubky.app/tags on the homeserver after Sync), stars and notes,
   "On Pubky" row; an artist page with a portrait and biography (needs MusicBrainz artist ids in
   the tags); Radio: search "fip", play on the KA17 zone and on "This browser", watch the live
   title change; Favourites; search for an album title and an artist name. Say which of these
   produced an error toast and paste the node log line. Internet Archive: search "Grateful Dead
   1977", open an item, play it on the KA17 zone (FLAC items should show a Lossless/BitPerfect
   path, MP3 ones Lossy), heart it, add it to a playlist and play that playlist. In the Subsonic
   client, check that the saved radio stations appear under Internet Radio and play.

Commit on a branch `laptop-test-<date>`, push, open a PR against main with the results table,
hw_params captures, screenshots of the app, the Subsonic client's error log if any, and the fixes.
```

## Visual review (added 2026-10-20)

The author cannot see either UI (no display on the build box). Please take screenshots of the
native app (Home, an album page, Now Playing, the transport bar with something playing, Settings)
and of the web UI at `/` (same screens plus the zone picker and a track list on hover), and judge
them against these principles: clear hierarchy and spacing, consistent components, one accent on a
calm dark palette with real contrast, hover/focus/empty/loading states that explain the next step,
no clipped or overlapping text at 1280×800 and at a phone width for the web UI. Attach the
screenshots to the PR and list everything that looks wrong, however small. `tools/ui-smoke.js`
(jsdom) renders every web UI view headlessly against a running node and must exit 0.
