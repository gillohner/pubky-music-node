# Deploying the node

One binary, three shapes. The homeserver is always a separate Pubky service the node talks to by key.

## Directly on hardware (Pi, NUC, NAS)

```sh
sudo apt install libasound2-dev          # build only
cargo build --release -p pm-node --features alsa
pm-node config > node.toml               # edit: data_dir, [api] token (contact and folders can also be set later in Settings)
pm-node -c node.toml scan
pm-node -c node.toml serve               # API on [api] listen, one zone per ALSA hw: device
```

Free the DAC from PipeWire/PulseAudio on desktop systems (see `docs/HARDWARE-TEST-PROMPT.md`).

## Docker

`Dockerfile` builds the node with ALSA and runs it as an unprivileged user in the `audio` group.
The entrypoint writes `/data/node.toml` from `PM_*` variables on first start (see
`deploy/docker/entrypoint.sh`).

```sh
docker build -t pubky-music-node .
docker run -d --name pm -p 7788:7788 --device /dev/snd \
  -v /srv/music:/music:ro -v pm-data:/data \
  -e PM_TOKEN=change-me -e PM_CONTACT=you@example.org pubky-music-node
curl -H 'Authorization: Bearer change-me' http://localhost:7788/v1/info
```

## With its own Pubky homeserver (bundled)

`deploy/bundle/` runs the node together with a Pubky homeserver, so your playlists, favourites and
files (Copy to homeserver, Bring files here) live on your own machine instead of a shared public
homeserver. It follows Pubky's own Umbrel app: PostgreSQL, pubky-homeserver (v0.16.2) with its
config wrapper, and optionally Pubky's homeserver dashboard and a Cloudflare tunnel. Every image is
pinned by version and digest.

```sh
cd deploy/bundle
./setup.sh          # once: writes .env with generated passwords (never printed) and your LAN address
# set MUSIC_DIR in .env to your music folder
docker compose up -d
docker compose --profile dashboard up -d    # optional: Pubky's admin UI on http://127.0.0.1:8812
```

- **What listens where:** the node on 7788, and the homeserver's Pubky TLS port 6287 (Pubky apps,
  Pubky Ring) on the network. The homeserver's admin API (6288) and plain HTTP port (6286) are
  bound to 127.0.0.1: only the node, a tunnel and a local reverse proxy reach them. The node runs
  on the host network to find speakers.
- **Passwords:** `.env` holds the database password, the homeserver admin password (also the
  dashboard's login) and the node's API token. `setup.sh` generates them and never prints them;
  the node never logs or returns the admin password.
- **Storage:** users' files are under the homeserver's data volume. Set `HOMESERVER_DATA` to a
  folder on the big disk (e.g. `/mnt/tank/pubky-homeserver`) before the first start. Quotas are
  per user.
- **Reachable from outside, option A: Cloudflare Tunnel.** No open ports and no static IP.
  1. In Cloudflare Zero Trust → Networks → Tunnels, create a tunnel. Under its public hostnames,
     send one hostname to `http://localhost:6286` (the homeserver) and another to
     `http://localhost:7788` (the node).
  2. In the node's Settings › Remote access (`PUT /v1/remote/tunnel`), paste the tunnel's token and
     the two hostnames. The node saves them in the folder it shares with cloudflared and the
     homeserver's config wrapper.
  3. Start cloudflared once with `docker compose --profile tunnel up -d`. It then picks up the token
     by itself.
  4. Restart the homeserver once (the node shows the command:
     `docker compose up -d --force-recreate homeserver-config homeserver`). The node can't do this
     itself without Docker's socket, which would mean root on the host.

  After that the homeserver publishes its hostname on 443, and Pubky clients use HTTPS through it.
  Settings checks the published record and both hostnames over HTTPS
  (`GET /v1/remote/tunnel?check=1`). Pubky TLS (6287) doesn't cross a tunnel; clients don't need
  it then.
- **Option B: your own domain.** You need a static IP, ports 80, 443 and 6287 forwarded, and Caddy
  on 443 in front of `127.0.0.1:6286` (and the node). Set `HOMESERVER_DOMAIN` in `.env` and
  `PUBLIC_IP` to the static IP. The homeserver publishes the domain on 443, and Pubky TLS on 6287
  answers directly.
- **No domain:** the homeserver publishes the LAN address, so Pubky Ring on the same network can
  sign up.
- **What the operator can see:** whoever runs the homeserver can read every account's files,
  including `/priv` (it is access-controlled, not encrypted from the operator). That is fine when
  the operator is you; tell anyone you invite.
- `GET /v1/homeserver/bundled` shows the homeserver as the node sees it: version, key, users,
  disk used, unused invites.
- **People:** Settings › People (`/v1/people`) invites someone and picks what they get:
  - **Homeserver only:** a Pubky account here with a storage quota.
  - **Homeserver + music:** the same, plus this node's music service.
  - **Music only:** someone whose account is elsewhere, added by their public key.

  A homeserver invite is a Pubky Ring link and QR (`direct_signup`, which needs a recent Ring; older ones can type the code). Per person you set the quota, turn their homeserver account off (that blocks writes; they can still read and delete) and take away music access. The tab also shows how much each person stores and which zones listeners may play on.
- **Adding music** (Library → Add music): upload files from the browser (resumable), copy a
  folder on the machine into the homeserver, or keep using folders in place (mounts and shares as
  before). Uploads and copies go into your homeserver's private storage. The node mounts the
  homeserver's files read-only and plays them from there, so there is no second copy. Saving
  playlists, favourites and tags to the homeserver is automatic; `GET /v1/storage` is the status
  line.
- **Signing in per person:** everyone signs in to the node's web app with Pubky Ring. The owner gets the node's API token, as before; apps and integrations keep using it. Someone in People with music access gets a personal token: the shared library, search, radio and other public directories, and playing on the zones you open to listeners (`/v1/people/zones`; all by default). Settings, People, the homeserver, the library's folders, your streaming accounts, and your favourites, playlists, tags and history stay yours. Taking someone's music access away stops their token at once. Their own playlists and favourites (on their own homeserver) come next.
- **Your account on it:** on first run, "Create your Pubky account on this node's homeserver"
  (`POST /v1/homeserver/bundled/owner-signup`) shows a Pubky Ring link and QR. The node mints the
  owner's signup token (unlimited storage); Ring creates the account on this homeserver and signs
  the node in. Playlists, favourites, Copy to homeserver and Bring files here then use it. "Use an
  existing homeserver" is the usual Pubky sign-in; moving an existing account here comes later.

The Umbrel app (below) bundles the same homeserver. Install it or Pubky's own "Pubky Homeserver"
app, not both: both use port 6287.

## TrueNAS SCALE

`deploy/truenas/docker-compose.yaml` is a ready custom app: Apps → Discover Apps → ⋮ → *Install
via YAML*, paste it, set the token, contact and the two dataset paths. The web UI is the node
itself on port 7788 (`http://<nas>:7788/`, it asks for the token once); the same address and token
go into the native app and into Subsonic clients. The image is published on GHCR; the package
must be public (or the NAS needs registry credentials) for the pull to succeed. The container runs
as uid 1000, so the data dataset must be writable by that user; the music dataset is read-only.
Using the form instead: Repository `ghcr.io/gillohner/pubky-music-node`, Tag `edge` (no tag in
the repository field), the `PM_*` variables, and the two storage mounts. Turn **Host Network** on:
A renderer on another subnet or VLAN (common with UniFi "IoT" networks) never answers multicast: add it in
Settings → Outputs → "Add renderer" with its IP (the node sends a unicast M-SEARCH to UDP 1900, so that port
must be allowed between the networks) or its description URL; the address is remembered across restarts.
UPnP/OpenHome renderers (Linn, Naim, …), Chromecast and endpoints are found by multicast and
fetch audio from the node, neither of which crosses Docker's bridge network; with host networking
no port mapping is needed and the UI is on port 7788, and the apps find the node under "On this network" (mDNS and SSDP). Set `PM_PUBLIC_URL` to
`http://<nas-ip>:7788` so devices get a reachable address. The entrypoint writes `/data/node.toml`
only on the first start; after changing variables, set `PM_REWRITE_CONFIG=1` for one restart.

## Umbrel

`deploy/umbrel/pubky-music-node/` holds the app manifest and compose file for an Umbrel community
app store. Points worth knowing:

- The API is exposed on port 7788 with the bearer token (`APP_SEED`, which Umbrel generates per app)
  instead of behind `app_proxy`, because the native controller and OpenSubsonic clients on other
  machines cannot pass Umbrel's web login. The built-in web UI at `/` uses the same token (entered
  once in the browser), so one port serves every client.
- `/dev/snd` is passed through for a DAC on the Umbrel box; otherwise use a network endpoint (M2).
- Umbrel's shared storage is mounted read-only at `/music`, so a torrent or download app writing
  there is just a scan source. The node watches its folders and rescans 15 s after the last change
  (`[library] watch`, `watch_quiet_secs`), so downloads appear on their own; `POST /v1/library/scan`
  and the controller's Settings page still trigger a scan at any time.
- Your own files on your homeserver: `pm-node upload a.flac` (or `POST /v1/pubky/files`) puts audio
  into the homeserver's private storage. On another node with your session, Settings › Accounts ›
  Bring files here downloads what that node lacks into a library folder (`From homeserver` by
  default) and indexes it. Private only, never `/pub/`.
- Hard constraints hold on every shape: audio stays node-local or in private storage, nothing under
  `/pub/`, and the node never serves one person's library to many users.

Images are published to `ghcr.io/gillohner/pubky-music-node` for `linux/amd64` and `linux/arm64`
(Umbrel boxes are mostly Raspberry Pis) by `.github/workflows/release.yml`: every `v*` tag gives
`<version>`, `<major.minor>` and `latest`; running the workflow by hand publishes `edge`. The
arm64 binary is cross-compiled on the amd64 runner (Debian multiarch ALSA headers, GNU cross
toolchain), so no emulated cargo build. The Umbrel compose file pins the version tag; make the
package public in the repository's package settings before pointing an Umbrel at it. Image builds
are not possible on the development machine (no Docker daemon); CI's `docker` job builds the
native image on every push to catch Dockerfile drift.

## Reaching the node from outside the house

**Without any of the below:** once the node is signed in with Pubky Ring, the Pubky Music app reaches it from anywhere over iroh (ADR 0013): no port forwarding, no domain, and it works behind Docker's bridge too (outbound UDP only). `[remote] relay` picks the relays (empty = n0's public ones, a URL = your own iroh-relay, `none` = off); `[remote] enabled = false` turns it off. Browsers without the app still need one of the options below.

The node speaks plain HTTPS/WebSocket and expects a bearer token, so any private network layer
works until the Pubky transport exists (ADR 0008):

- **Tailscale / WireGuard**: install on the node and the phone or laptop; use the node's tailnet
  address in the app, the web UI and Subsonic clients. Nothing to expose.
- **Cloudflare Tunnel / a reverse proxy with TLS**: terminate TLS in front of port 7788 and keep the
  token. Set `[api] public_url` to the public origin so pull-stream URLs handed to renderers are
  right. Do not put the node on the open internet without TLS and a strong token.
- **Pubky**: a signed-in node publishes its address privately on your homeserver; the app's
  "Find my node via Pubky" reads it, so a moved or re-addressed node is found again without typing.

## Streaming services: what you set up

Nothing for most services. A release build carries the project's own TIDAL, Jamendo and AcoustID apps (public ids only;
no secrets ship), and the TIDAL sign-in goes through the project's static return page, so it works on a LAN address.
Open Sources › TIDAL › Connect TIDAL.

- **Spotify:** Spotify allows no shared app, so you make your own free one once. Sources › Spotify walks you through
  it and takes only the client ID.
- **Spotify on this machine (Soloist):** Spotify's official headless player, run by the node (Linux, macOS). Under
  Sources › Spotify › Play on this machine: save your own Soloist API key (made with a Premium account at
  developer.spotify.com › Dashboard › Spotify Soloist API Key), press Download Soloist (from Spotify's own servers;
  Spotify forbids us to ship it), switch it on, then pick the device in a Spotify app. It plays on the machine's
  PipeWire or PulseAudio output, not on the node's zones. `[spotify] soloist` (or `PM_SOLOIST`) names an install of
  your own. In Docker it needs the host's audio socket, which the image doesn't set up.
- **Spotify through librespot (unofficial):** install librespot yourself (`cargo install librespot` or your
  distribution's package; it has no ready-made builds), then switch it on under Sources › Spotify › librespot. It can
  play on the machine's sound or into one of the node's zones.
- **TIDAL full tracks (unofficial):** off by default; switch it on under Sources › TIDAL › Full tracks and press Sign
  in with a code. Release builds carry a built-in TIDAL device client (python-tidal's); a device client of your own,
  entered there or as `[tidal] unofficial_client_id/secret`, always wins (an app from TIDAL's developer portal is not
  a device client and won't work there).
- **SoundCloud (unofficial):** off by default. The node has no built-in client; you enter a SoundCloud web client ID
  you are allowed to use under Sources › SoundCloud, and, for your likes and playlists, your own sign-in token.
- **Bandcamp (unofficial):** off by default. Search and streams need nothing; to download what you bought, paste the
  value of your bandcamp.com `identity` cookie under Sources › Bandcamp. Purchases land in `<data_dir>/bandcamp`, a
  library folder.
- **Last.fm:** optional. Make a free API account at last.fm/api/account/create and enter its API key and shared
  secret under Settings › Accounts › Last.fm (or `[lastfm] api_key / api_secret`), then Connect. Plays you make
  here are scrobbled; artist pages show similar artists.
- **Your own apps:** `[tidal]`, `[jamendo]`, `[spotify]` and `[acoustid]` in `node.toml` (or the `PM_*` variables)
  always win over the built-in ones. See [STREAMING-PROVIDERS.md](STREAMING-PROVIDERS.md).
- Artist pages: a release build carries the project's fanart.tv and TheAudioDB keys (`PM_PROJECT_FANART_KEY`,
  `PM_PROJECT_AUDIODB_KEY`, public project keys) for portraits and biographies Wikipedia lacks; without them,
  fanart.tv is skipped and TheAudioDB's public test key is used (a few requests a minute).
- A build from source without `PM_PROJECT_*_CLIENT_ID` / `PM_PROJECT_ACOUSTID_KEY` set has no built-in apps, and one
  without `PM_TIDAL_DEVICE_CLIENT_ID` / `PM_TIDAL_DEVICE_CLIENT_SECRET` has no built-in TIDAL device client; add your
  own as above.

## Phones and desktops without the native app

The web UI ships a web app manifest: on a phone, "Add to Home Screen" installs it as a standalone
app with the node's icon, and the lock screen's media keys drive browser playback.

Any OpenSubsonic client (Symfonium, Feishin, Amperfy, play:Sub, Supersonic) connects to the node's
address with user `[api] subsonic_user` and the API token as password, and gets browsing, search,
playlists, cover art and bit-exact file streaming (`docs/api.md`).

## Endpoints (a Pi next to a DAC)

```sh
cargo build --release -p pm-endpoint --features alsa     # on the endpoint machine
pm-endpoint --name "Kitchen Pi"                          # 0.0.0.0:7790, advertised over mDNS
```

The node lists the endpoint's outputs next to its own (`[outputs] discover = true`, or
`endpoints = ["192.168.1.20:7790"]` where multicast does not cross). The endpoint receives the
negotiated format and packs it for its DAC exactly as the node would locally; its DAC buffer paces
the stream (ADR 0007). The traffic is plain TCP on the LAN; never expose the port to the internet.

## Squeezebox players and squeezelite

The node speaks Slimproto on TCP 3483 (`[outputs] slimproto_port`). Point a player at the node's
address (`squeezelite -s <node-ip> -o hw:0,0`); it appears as an output and zone named after its
model and MAC, plays the zone's post-DSP PCM as a WAV stream it fetches from `[api] public_url`, and
takes volume through its own digital gain. Players on the same subnet also find the node through
the discovery responder.

## Where the music can live

The node scans folders; anything the operating system can mount is a folder.

| Source | How |
| :-- | :-- |
| Local disks, USB drives | add the folder in Settings or `[library] folders` |
| NAS / network shares (SMB, NFS) | mount them (`/etc/fstab`, `mount -t cifs …`) and add the mount point; use `_netdev` so the mount exists before the node starts |
| Dropbox, Google Drive, OneDrive, S3, WebDAV, … | `rclone mount remote:Music /mnt/music --vfs-cache-mode full --read-only` (a systemd unit keeps it up), then add `/mnt/music`. The VFS cache makes the first play of a file a download and every later one local; bit-perfect playback is unaffected because the node reads the original bytes |
| Downloads from another app (Umbrel, a seedbox) | point that app at a folder the node watches; the watcher rescans after the last change |
| Your Pubky homeserver | `pm-node upload` or `POST /v1/pubky/files` stores originals under `/priv/pubky.music/files/`; "Bring files here" (`POST /v1/pubky/files/mirror`) pulls what the node lacks into a library folder as a background job and scans it |
| Streams | internet radio and the Internet Archive need no storage at all |

Scans read tags, embedded art and MusicBrainz ids; tag with Picard first and favourites, tags,
reviews and playlists can refer to the music by its canonical identity.

## Adding music after the node is running

Folders can be added from the web UI or the app (Settings → Library folders) as well as in
`[library] folders` of the config. Folders added from the UI are stored in the library database,
watched for changes and scanned straight away; removing one there forgets the tracks that lived
only in it.

## Backups

Settings → **Backup and restore** makes one file (`pubky-music-<date>-<time>.pmbackup`) with
everything needed to rebuild the node, never the audio:

- the library database: matches and corrections, playlists, reviews, favourites, tags, play
  history, queues, saved radio stations, settings, the lyrics index;
- with a passphrase the owner picks: the sign-ins (TIDAL, Spotify, Jamendo, ListenBrainz, your
  Subsonic and Jellyfin servers), the Pubky session, the remote-access key and the node's config
  file, encrypted (Argon2id, AES-256-GCM). Without a passphrase they are left out, and a restore
  lists which services to sign in to again. Nothing opens them without the passphrase.

**Every day or week, to a folder:** the same card writes backups to a folder on the node (another
disk or a NAS) and keeps the newest N. A passphrase saved there stays on this node and is never
put in a backup. `GET /v1/backup` serves a backup without sign-ins for a cron job.

**Restore** on a new node: Home's Get started card links to it, or Settings → Backup and restore →
Choose a backup file. The node checks the file and shows what it holds first, then swaps the
library in while it runs, puts the sign-ins back, reconnects Pubky and rescans. Unchanged files
are skipped, so every match stays. If the music lives somewhere else now, the restore lists the
folders it cannot find. Type each folder's new place and the tracks keep their matches, playlists
and history (`POST /v1/library/folders/move`). Covers taken from the files are read again. The old
config file is kept as `<data_dir>/node.restored.toml`; from the next start its service keys fill
whatever this node's own config leaves empty.

From the command line: `pm-node -c node.toml backup <file>` writes a database snapshot (without
sign-ins) while the node runs. `pm-node -c node.toml restore <file>`, with the node stopped, takes
that snapshot or a `.pmbackup` file (its library only; the previous database is kept as
`library.db.bak`).
