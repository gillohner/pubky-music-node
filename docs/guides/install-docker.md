# Install with Docker

This runs the node from its Docker image on a NAS or any Linux machine. It takes about five
minutes. You need Docker with Docker Compose, and to know where your music folder is.

The image is `ghcr.io/gillohner/pubky-music-node`, for amd64 (most NAS and PCs) and arm64
(Raspberry Pi 4 and 5 with a 64-bit system). Use the `edge` tag, built from the newest code;
`latest` is the last numbered release.

## 1. Write the compose file

Make a folder for it (for example `~/pubky-music`, or `/volume1/docker/pubky-music` on Synology)
and save this as `docker-compose.yml` there:

```yaml
services:
  pubky-music:
    image: ghcr.io/gillohner/pubky-music-node:edge
    container_name: pubky-music
    restart: unless-stopped
    # Needed to find speakers on your network and for them to reach the node.
    network_mode: host
    environment:
      PM_NAME: "Pubky Music"
    volumes:
      # Your music folder on this machine, on the left. Read-only: the node never changes it.
      - /srv/music:/music:ro
      # The node's own data: library, settings, token.
      - pubky-music-data:/data
    # A USB DAC plugged into this machine: remove the # on the next four lines.
    # devices:
    #   - /dev/snd:/dev/snd
    # group_add:
    #   - audio

volumes:
  pubky-music-data:
```

Change `/srv/music` to your music folder (on Synology, for example `/volume1/music`).

## 2. Start it

```sh
cd ~/pubky-music
docker compose up -d
docker compose logs -f
```

The log has a line containing `clients sign in with the one in /data/api-token`, one with the
address speakers fetch music from (this machine's address on your network), then the speakers it
finds. Press Ctrl+C to stop following the log; the node keeps running.

## 3. Open it

Get the access token:

```sh
docker exec pubky-music cat /data/api-token
```

Open `http://<this machine's address>:7788`, click **Or paste the API token**, paste it and
press **Connect**. The address is the one in the log; on Linux, `hostname -I` shows it, on a NAS
its network settings (Synology: Control Panel › Network), and your router's device list does too.
Give the machine a fixed address in the router, so it doesn't change. The node reads your music
folder (`/music` inside the container) by itself when it starts; **Settings › Library folders**
lists it. Continue with the
[first steps](../../README.md#first-steps-after-installing).

To choose the token yourself instead, add `PM_TOKEN: "<a long random string>"` under
`environment` before the first start (`openssl rand -hex 32` makes one; a password manager can
too). Do this if your NAS has no terminal: Synology's Container Manager, for example, runs the
compose file from its screens, and then `docker exec` isn't at hand.

## NAS notes

- **Permissions.** The node runs as user id 1000 inside the container. Your music folder must be
  readable by that user (on most NAS, read access for "everyone" is enough). If the library stays
  empty, see [Troubleshooting](troubleshooting.md#the-library-stays-empty).
- **Synology (Container Manager), QNAP (Container Station), Unraid, Portainer:** create a
  "project" or "stack" and paste the compose file above. Choose the host network if the form asks.
- **TrueNAS SCALE:** Apps › Discover Apps › ⋮ › Install via YAML, and paste
  [`deploy/truenas/docker-compose.yaml`](../../deploy/truenas/docker-compose.yaml). Change the two
  paths, `PM_CONTACT` and `PM_NAME`. The data dataset must be writable by user
  id 1000 (`chown -R 1000:1000 /mnt/tank/apps/pubky-music`), or the node can't start.
- **Docker Desktop on a Mac or Windows PC** can't find speakers, because host networking isn't the
  same there. Use a Linux machine or NAS for the node.

## Settings you can add

These go under `environment`. They are written into `/data/node.toml` on the first start.

| Variable | What it does |
| :-- | :-- |
| `PM_PUBLIC_URL` | the address speakers fetch music from (`http://<ip>:7788`); by default this machine's address on your network |
| `PM_TOKEN` | your own access token instead of the generated one |
| `PM_NAME` | the name the apps show for this node |
| `PM_CONTACT` | an email or website for MusicBrainz and Discover (also in Settings › Discover) |
| `PM_FOLDERS` | music folders inside the container, separated by `:` (default `/music`) |
| `PM_LISTEN` | address and port of the web page and API (default `0.0.0.0:7788`) |

The file is only written on the first start. To apply changed variables later, add
`PM_REWRITE_CONFIG: "1"`, run `docker compose up -d` once, then remove the line again. This
rewrites the whole `node.toml` from the variables. Anything changed in Settings is kept, because
it lives in the library, not in that file.

## With your own Pubky homeserver

[`deploy/bundle/`](../../deploy/bundle/) runs the node together with a Pubky homeserver, so your
Pubky account, playlists and uploaded files live on your own machine. Steps and options are in
[docs/deploy.md](../deploy.md#with-its-own-pubky-homeserver-bundled). In short:

```sh
git clone https://github.com/gillohner/pubky-music-node.git
cd pubky-music-node/deploy/bundle
./setup.sh                # writes .env with generated passwords
nano .env                 # set MUSIC_DIR to your music folder
docker compose up -d
```

There the access token is `PM_TOKEN` in `.env`.

## Next

- [Speakers](speakers.md): what plays and how to add it.
- [Remote access](remote-access.md), [Backups and updates](backup-and-update.md).
