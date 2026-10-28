# Troubleshooting

First look at the node's log; most problems are named there.

| Install | Log |
| :-- | :-- |
| Docker | `docker logs --tail 200 pubky-music` |
| Linux without Docker | `journalctl -u pubky-music-node -n 200` |
| Umbrel | right-click the app's icon › **Troubleshoot** |

## The web page doesn't open

- Use `http://`, not `https://`, and port 7788: `http://192.168.1.10:7788`.
- Check the node is running (the log above) and that you use the machine's current address.
- A firewall on the machine must allow TCP 7788 (`sudo ufw allow 7788/tcp` with ufw).
- A node started by hand without a config listens only on the machine itself
  (`127.0.0.1:7788`). Set `[api] listen = "0.0.0.0:7788"` and a token, as in the
  [Linux guide](install-linux.md#2-a-user-and-a-config-file).

## It asks for a token, or the token is refused

Click **Or paste the API token** and paste the token. Where it is:

| Install | Token |
| :-- | :-- |
| Docker | `docker exec pubky-music cat /data/api-token`, or `PM_TOKEN` if you set one |
| Docker bundle | `PM_TOKEN` in `deploy/bundle/.env` |
| Umbrel | the password umbrelOS shows for the app |
| Linux without Docker | `token` in `/etc/pubky-music/node.toml` |

- **Sign in with Pubky Ring** says the node is not signed in yet: use the token the first time,
  then sign the node in under **Settings › My accounts**.
- You set `PM_TOKEN` after the first start and it has no effect: the config is written only once.
  Add `PM_REWRITE_CONFIG: "1"`, run `docker compose up -d`, then remove the line again.
- **Settings › About and updates › Change access token** lets this browser use another token; it
  doesn't change the node's token.
- To give the node a new token: Docker with a generated token: `docker exec pubky-music rm
  /data/api-token`, then `docker restart pubky-music` makes a new one. With `PM_TOKEN` or on
  Linux, change the token there and restart (Docker also needs `PM_REWRITE_CONFIG`, as above).
  Every app and browser then needs the new one. On Umbrel the token is the app's password.

## The library stays empty

- Add the folder in **Settings › Library folders**: type the path and press **Add folder**. In
  Docker and Umbrel the path is `/music`, not the path on the host.
- After adding files while the node was stopped, press **Scan now** on the same page.
- **Permissions:** the node must be able to read the files. In Docker it runs as user id 1000:
  `docker exec pubky-music ls /music` must list your files. If it says "Permission denied", make
  the folder readable for everyone (`chmod -R a+rX /srv/music`) or owned by user 1000. Without
  Docker, check with `sudo -u pubky-music ls /srv/music`.
- A network share must be mounted before the node starts. In `/etc/fstab`, add `_netdev` to the
  share's options.
- The log names files it could not read.

## The node doesn't start: "Permission denied" on /data

The data folder must be writable by user id 1000. With a folder instead of a Docker volume:
`sudo chown -R 1000:1000 /path/to/data`.

## No speakers are found

- **Docker:** the container must use the host network (`network_mode: host`). On Docker Desktop
  for Mac or Windows that isn't possible; run the node on a Linux machine or NAS.
- In **Settings › Zones and outputs**, press **Explain what is on the network**. "No UPnP device
  answered at all" means multicast doesn't reach the node.
- **Other network segments** (guest Wi-Fi, "IoT" VLANs, mesh systems in router mode): search
  doesn't cross them. Put the speakers and the node on the same network, or:
  - UPnP/DLNA, Sonos, HEOS: **Add a renderer by address** with the speaker's IP. UDP port 1900
    must be allowed between the networks. The address is remembered.
  - Chromecast, AirPlay, BluOS: turn on the router's mDNS reflector (UniFi: "Multicast DNS";
    OPNsense and pfSense: the mDNS repeater or Avahi).
- **Squeezelite:** start it with `-s <node address>` (or `SB_SERVER_IP`), and allow TCP 3483.
- **Snapcast and MPD** are never found by search; add them to the config
  ([Speakers](speakers.md#snapcast-and-mpd)).

## A speaker is listed but stays silent

- Speakers fetch the music from the node at `PM_PUBLIC_URL` (`public_url`). It must be the node's
  network address, `http://192.168.1.10:7788`, never `localhost` or `0.0.0.0`. **Explain what
  is on the network** warns when it is wrong.
- The speaker must reach the node on TCP 7788 (check firewalls between networks).
- An AirPlay receiver that requires encryption is skipped; the log says so.

## The DAC on the node's machine is missing or busy

- Docker: pass the sound devices and the `audio` group (the commented lines in the
  [compose file](install-docker.md#1-write-the-compose-file)).
- Linux desktop systems: PipeWire or PulseAudio may hold the DAC. Use a DAC the desktop doesn't
  use, or stop the desktop's sound server for it.
- `aplay -l` on the machine lists the sound devices the system sees.

## Ports

| Port | What | Open to |
| :-- | :-- | :-- |
| TCP 7788 | web page, API, Subsonic apps, the music stream speakers fetch | your home network |
| TCP and UDP 3483 | Squeezebox and squeezelite players | your home network |
| TCP 7790 on the computer running the app (another port if that one is taken) | the app's own player: the node connects to it (not needed when the node runs on that computer) | your home network |
| UDP 1900 | UPnP search (SSDP) | your home network |
| UDP 5353 | mDNS: Chromecast, AirPlay, BluOS, the apps finding the node | your home network |
| TCP 6287 | Pubky homeserver (Umbrel and the bundle only) | your home network, or forwarded for your own domain |

None of these needs to be open to the internet. Away from home, use
[Remote access](remote-access.md).

## Umbrel: the app won't install or start

Pubky's own "Pubky Homeserver" app and this app both use port 6287. Install only one of them.

## Discover is empty

Discover needs a contact: enter an email or website in **Settings › Discover**. MusicBrainz,
Wikipedia and the Pubky music index ask every app for one.

## macOS or Windows won't open the app

The app isn't signed. macOS: right-click › **Open**, or **System Settings › Privacy &
Security › Open Anyway**. Windows: **More info › Run anyway**.

## Still stuck

Open an issue on [GitHub](https://github.com/gillohner/pubky-music-node/issues) with how you
installed it, the version (**Settings › About and updates**) and the last part of the log. Remove
your token from anything you paste.
