# Troubleshooting

First look at the node's log; most problems are named there.

| Install | Log |
| :-- | :-- |
| Docker | `docker logs --tail 200 pubky-music` |
| Linux without Docker | `journalctl -u pubky-music-node -n 200` |

## The web page doesn't open

- Use `http://`, not `https://`, and port 7788: `http://192.168.1.10:7788`.
- Check the node is running (the log above) and that you use the machine's current address.
- A firewall on the machine must allow TCP 7788 (`sudo ufw allow 7788/tcp` with ufw).
- A node started by hand without a config listens only on the machine itself
  (`127.0.0.1:7788`). Set `[api] listen = "0.0.0.0:7788"` and a token, as in the
  [Linux guide](install-linux.md#2-a-user-and-a-config-file).

## It asks for a token, or the token is refused

On the Log in screen, open **Use the node’s API token instead** and paste the token. Unless one is set, the node makes one on
its first start and keeps it in the `api-token` file in its data folder. Where it is:

| Install | Token |
| :-- | :-- |
| Docker | `docker exec pubky-music cat /data/api-token`, or `PM_TOKEN` if you set one |
| Docker bundle | `PM_TOKEN` in `deploy/bundle/.env` |
| Linux without Docker | `token` in `/etc/pubky-music/node.toml` |

- The Log in screen says the node isn't linked to a Pubky identity yet: use the token the first
  time, then sign the node in under **Settings › My accounts**. After that, **Open in Pubky Ring**
  (or the QR code) logs you in.
- It says "This node belongs to … You approved as …": Pubky Ring approved with another of your
  identities. Pick the node's owner in Pubky Ring and press **Try again**.
- You set `PM_TOKEN` after the first start and it has no effect: the config is written only once.
  Add `PM_REWRITE_CONFIG: "1"`, run `docker compose up -d`, then remove the line again.
- **Log out** (the account button, top right, or **Settings › About and updates**) ends this
  browser's session and shows the Log in screen; the node's token stays as it is.
- To give the node a new token: with a token the node made, **Settings › About and updates ›
  Make a new token** (shown once; the old one stops working at once). With `PM_TOKEN` or on
  Linux, change the token there and restart (Docker also needs `PM_REWRITE_CONFIG`, as above).
  Every app, and a browser given the token by hand, then needs the new one; browsers logged in
  with Pubky Ring stay logged in.

## The library stays empty

- **Your library › Node library** lists the folders. In Docker, `/music` (your mounted
  music folder) is there by itself, marked **From config**, and read at every start. "Not found
  on this computer" means the folder isn't mounted or the path is wrong.
- Another folder: type its path there and press **Add folder**. In Docker the path is the one
  inside the container, not the one on the host.
- Files added to a folder you added in Settings while the node was stopped: press **Scan now** on
  the same page.
- **Permissions:** the node must be able to read the files. In Docker it runs as user id 1000:
  `docker exec pubky-music ls /music` must list your files. If it says "Permission denied", make
  the folder readable for everyone (`chmod -R a+rX /srv/music`) or owned by user 1000. Without
  Docker, check with `sudo -u pubky-music ls /srv/music`.
- A network share must be mounted before the node starts. In `/etc/fstab`, add `_netdev` to the
  share's options.
- The log names files it could not read.

## The homeserver keeps restarting (own homeserver)

`docker compose logs homeserver` shows "Failed to publish the homeserver's pkarr packet to the
DHT": the homeserver needs the internet when it starts, to publish its address. It starts by
itself once the machine is online again (after a power cut, the router may come up later).

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

- Speakers fetch the music from the node's address on your network. The node finds it by itself
  and writes it in its log at start (`speakers fetch music from this address`). It is the address on
  the way to the internet, so with a VPN running, several networks, or Docker without host
  networking it may be one the speakers can't reach (**Explain what is on the network** says
  when it looks like a VPN or Docker address): set `PM_PUBLIC_URL` (`public_url`)
  to the right one, for example `http://192.168.1.10:7788`. **Explain what is on the network**
  warns when the address can't work (`localhost`, `0.0.0.0`).
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
| TCP 6287 | Pubky homeserver (the bundle only) | your home network, or forwarded for your own domain |

None of these needs to be open to the internet. Away from home, use
[Remote access](remote-access.md).

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
