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
  into the homeserver's private storage; every node with your session mirrors it into
  `data_dir/homeserver` and indexes it. Private only, never `/pub/`.
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

The node speaks plain HTTPS/WebSocket and expects a bearer token, so any private network layer
works until the Pubky transport exists (ADR 0008):

- **Tailscale / WireGuard**: install on the node and the phone or laptop; use the node's tailnet
  address in the app, the web UI and Subsonic clients. Nothing to expose.
- **Cloudflare Tunnel / a reverse proxy with TLS**: terminate TLS in front of port 7788 and keep the
  token. Set `[api] public_url` to the public origin so pull-stream URLs handed to renderers are
  right. Do not put the node on the open internet without TLS and a strong token.
- **Pubky**: a signed-in node publishes its address privately on your homeserver; the app's
  "Find my node via Pubky" reads it, so a moved or re-addressed node is found again without typing.

## Phones and desktops without the native app

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
| Your Pubky homeserver | `pm-node upload` or `POST /v1/pubky/files` stores originals under `/priv/pubky.music/files/`; `POST /v1/pubky/files/mirror` (and start-up) pulls them into `data_dir/homeserver`, which is scanned like any folder |
| Streams | internet radio and the Internet Archive need no storage at all |

Scans read tags, embedded art and MusicBrainz ids; tag with Picard first and favourites, tags,
reviews and playlists can refer to the music by its canonical identity.

## Adding music after the node is running

Folders can be added from the web UI or the app (Settings → Library folders) as well as in
`[library] folders` of the config. Folders added from the UI are stored in the library database,
watched for changes and scanned straight away; removing one there forgets the tracks that lived
only in it.

## Backups

Everything the node knows that is not in your music files lives in one SQLite file,
`<data_dir>/library.db`: playlists, reviews and listening activity (also on your homeserver once
synced), favourites, tags, play history, saved radio stations, settings, the art and lyrics index.
`pm-node -c node.toml backup <file>` writes a consistent snapshot while the node runs (SQLite
`VACUUM INTO`), the web UI's Settings page offers the same file as *Download backup*, and
`GET /v1/backup` serves it for a cron job. To go back: stop the node, `pm-node -c node.toml
restore <file>` (the previous database is kept as `library.db.bak`), start the node and let it
rescan. Covers extracted from files are in `<data_dir>/art` and come back on a scan; the Pubky
session secret is a separate file named by `session_secret_file`.
