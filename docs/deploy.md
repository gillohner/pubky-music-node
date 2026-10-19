# Deploying the node

One binary, three shapes. The homeserver is always a separate Pubky service the node talks to by key.

## Directly on hardware (Pi, NUC, NAS)

```sh
sudo apt install libasound2-dev          # build only
cargo build --release -p pm-node --features alsa
pm-node config > node.toml               # edit: data_dir, contact, [library] folders, [api] token
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
  instead of behind `app_proxy`, because the native controller on another machine cannot pass
  Umbrel's web login. A browser UI would go behind `app_proxy` later.
- `/dev/snd` is passed through for a DAC on the Umbrel box; otherwise use a network endpoint (M2).
- Umbrel's shared storage is mounted read-only at `/music`, so a torrent or download app writing
  there is just a scan source. Trigger scans with `POST /v1/library/scan` or from the controller's
  Settings page; automatic pickup (a filesystem watcher with debounce) is on the list.
- Hard constraints hold on every shape: audio stays node-local or in private storage, nothing under
  `/pub/`, and the node never serves one person's library to many users.

Image builds are not possible on the development machine (no Docker daemon); CI's `docker` job
builds the image to catch Dockerfile drift.
