# Speakers

The node plays to the speakers and streamers you already have, using each device's own protocol.
Most are found by themselves: open **Settings › Zones and outputs** to see them, and pick one with
the zone button in the player bar. Each speaker is a "zone" with its own queue.

For speakers to be found, the node must be on the same network as them (in Docker: host
networking). Speakers fetch the music from the node at its address on your network, which the node
finds by itself and writes in its log at start (`speakers fetch music from this address`). Set
`PM_PUBLIC_URL` (or `public_url`) only when that is the wrong one, for example on a machine with
several networks.

## What works

| Device | Examples | Setup |
| :-- | :-- | :-- |
| UPnP / DLNA renderers | network streamers (WiiM, Linn, Naim, Cambridge, Eversolo), smart TVs, AV receivers | found by themselves |
| Sonos | any Sonos speaker | found by themselves (as UPnP) |
| HEOS | Denon, Marantz | found by themselves (as UPnP) |
| BluOS | Bluesound, NAD | found by themselves |
| Chromecast | Chromecast, Google TV, Nest speakers, speakers with Chromecast built in, Cast groups | found by themselves; plays up to 24-bit/48 kHz (Cast's limit) |
| AirPlay | Apple TV, AirPort Express, AirPlay speakers, shairport-sync | found by themselves; plays the original AirPlay at 16-bit/44.1 kHz, which most AirPlay 2 speakers also accept. Receivers that require encryption are skipped (the log says which). AirPlay 2 multi-room is not supported |
| Squeezebox and squeezelite | old Squeezebox players, a Raspberry Pi with a DAC | point the player at the node (below) |
| Snapcast | Pis or old phones running `snapclient`, kept in tight sync | add your snapserver to the config (below) |
| MPD | a Music Player Daemon you already run | add it to the config (below) |
| A DAC on the node's machine | USB DAC, HDMI, the sound card | found by itself; in Docker, pass `/dev/snd` (see the compose file) |
| The Pubky Music app | your computer or Android phone | the app adds the device while it is connected at home, for its own node only ([guide](desktop-app.md)) |
| A browser | the web page itself | **This browser**, always there |

To play the same music in several rooms in sync, pick speakers of one kind in **Settings › Zones
and outputs › Group outputs** and press **Create group**:

- **Sonos with Sonos, HEOS with HEOS, BluOS with BluOS:** that system groups them itself. For
  HEOS, the speakers' HEOS control (TCP 1255) must answer; otherwise they play loosely synced.
- **Squeezelite players with each other:** the node keeps them in time sample by sample. A player
  that shares no sample rate with the ones picked can't join them.
- **Snapcast:** group the players in Snapcast; each Snapcast group is one zone.
- **Chromecast:** a speaker group made in the Google Home app shows up as one speaker.

## A Raspberry Pi with a DAC (squeezelite)

The simplest dedicated player: a Raspberry Pi, a USB or HAT DAC, and squeezelite. It plays
bit-perfect up to the DAC's limits, and several of them can be grouped in sync. The node runs on
another machine (or on the same Pi).

On the Pi (Raspberry Pi OS):

```sh
sudo apt install squeezelite
squeezelite -l            # lists the outputs; note the DAC's name, e.g. hw:CARD=DAC,DEV=0
sudo nano /etc/default/squeezelite
```

In that file set (remove the `#` in front):

```sh
SL_NAME="Kitchen"                     # the name the node shows (no spaces)
SL_SOUNDCARD="hw:CARD=DAC,DEV=0"       # the DAC from squeezelite -l
SB_SERVER_IP="192.168.1.10"            # the node's address
```

Then `sudo systemctl restart squeezelite`. The Pi shows up as a speaker within a few seconds. On
the same network `SB_SERVER_IP` is optional (the node answers squeezelite's search), but set it
if you also run a Logitech Media Server, or the Pi may connect to that instead.

The node listens for these players on port 3483.

## Snapcast and MPD

These are not found by themselves: add them to the node's config file and restart the node.

```toml
[outputs]
snapcast = "192.168.1.50"                 # your snapserver; every Snapcast group becomes a zone
mpd = ["192.168.1.40", "password@study.local:6600"]
```

- **Linux without Docker:** add the lines to `/etc/pubky-music/node.toml`, then
  `sudo systemctl restart pubky-music-node`.
- **Docker:** add them to `/data/node.toml` in the container, then restart it:

  ```sh
  docker exec -i pubky-music sh -c 'cat >> /data/node.toml' <<'EOF'

  [outputs]
  snapcast = "192.168.1.50"
  EOF
  docker restart pubky-music
  ```

  Do this once; a second `[outputs]` section makes the file invalid. `PM_REWRITE_CONFIG`
  rewrites the file and removes these lines.

## A speaker is missing

1. In **Settings › Zones and outputs**, press **Look for speakers again**.
2. Press **Explain what is on the network**: it lists what answered and why something isn't a
   speaker.
3. A speaker on another network segment (a guest or "IoT" Wi-Fi, a VLAN) can't be found by
   search. For UPnP/DLNA, Sonos and HEOS, use **Add a renderer by address** with its IP address.
   Chromecast, AirPlay and BluOS need your router to pass mDNS between the networks (often called
   "mDNS reflector" or "multicast DNS" in UniFi, OPNsense and others).

More in [Troubleshooting](troubleshooting.md#no-speakers-are-found).

## The other way round: the node as a speaker

Every zone also shows up on the home network as a DLNA renderer, so Home Assistant, Music
Assistant or BubbleUPnP can play to the node's speakers. See [Home Assistant](../home-assistant.md).
