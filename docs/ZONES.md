# Zones: what the API gives the Zones view

For the Zones UX (infra brief §3; the node agent owns the UI). Everything below is in `/v1` today.

## Kinds of zone, and their icons

A zone's `output` id starts with its protocol, which says what kind of device it is:

| `output` starts with | Device | Icon idea |
| :-- | :-- | :-- |
| `alsa:`, `cpal:` | a DAC or the sound card on the node's machine | DAC |
| `net:` | the Pubky Music app's built-in player: "this computer" or the phone | computer |
| `upnp:` | a DLNA renderer or smart TV, or a Sonos player (`GET /v1/outputs`: name and caps) | TV / speaker |
| `sonos-group:` | Sonos players grouped Sonos's own way | speakers |
| `heos-group:` | HEOS players (Denon, Marantz) grouped HEOS's own way | speakers |
| `bluos:` | a BluOS player (Bluesound, NAD), found over mDNS; it fetches the zone's stream | speaker |
| `bluos-group:` | BluOS players grouped BluOS's own way (the first leads) | speakers |
| `cast:` | Chromecast, Google TV, Nest speaker; names ending "(Cast group)" are Cast groups | cast |
| `airplay:` | an AirPlay receiver | AirPlay |
| `mpd:` | an MPD server listed in `[outputs] mpd`; it plays the zone's stream | speaker |
| `snapcast:` | a Snapcast group of players | speakers |
| `slim:` | a Squeezebox or squeezelite player | speaker |
| `pull:` | an HTTP stream anyone can fetch | link |
| `group:` | a node group of several outputs | speakers |
| `null` | the null output (testing) | — |

MPD servers aren't discovered; list them in the node's config, and each one that answers becomes a
zone (it needs MPD's curl input, which standard builds have). The node adds its stream to MPD's
playlist and removes it again when the zone stops, so the rest of that playlist stays:

```toml
[outputs]
mpd = ["192.168.1.40", "password@study.local:6600"]
```

"This device" is the web UI's browser zone (`browser`, local to the page). In the app it is also the
built-in player's `net:` zone, named "<this computer> · <DAC>" on a desktop and "<phone model> ·
Speaker or headphones" on Android (it follows Android's routing: speaker, wired, Bluetooth, USB DAC).

## The honest sync label

Every zone state has `sync`:

| `sync` | Meaning | Label |
| :-- | :-- | :-- |
| `single` | one device | nothing |
| `tight` | sample-accurate: the node's own endpoints on `hw:` DACs (ADR 0009), Slimproto players (started on one instant, kept within a few ms), a Snapcast group, a Sonos group, a HEOS group, a Cast group | "In sync" |
| `loose` | started together, may drift by a fraction of a second: mixed protocols; AirPlay 1, DLNA and single Cast devices together; any endpoint on a system mixer (`…/cpal:…`: a phone, a Mac, a Windows PC), whose latency the node cannot see | "Loosely synced" |

## Actions

- **Group:** `POST /v1/zones {name, outputs: [ids]}`.
  - A zone of Sonos players only becomes a Sonos group (`sonos-group:`, tight).
  - A zone of HEOS players only becomes a HEOS group (`heos-group:`, tight). The node asks the first player's HEOS CLI (TCP 1255) whether it knows them all, and makes a node group otherwise. The leader plays over UPnP, and HEOS plays its source on the others. The HEOS group ends when the zone stops.
  - A zone of BluOS players only becomes a BluOS group (`bluos-group:`, tight).
  - Anything else becomes a node group (`group:`), tight or loose as above. Outputs that share no
    sample rate are refused (400).
  - `GET /v1/outputs` marks Sonos, HEOS and BluOS players with `group_by`. Settings › Group
    outputs offers the outputs that group in sync (those, and `caps.groupable` ones: squeezelite,
    and the app's players on `hw:` DACs, `net:…/alsa:hw:…`), one kind at a time, and not one that shares no sample rate with the ones
    picked. Other mixes (Home Assistant's join, the API) are node groups, loosely synced.
- **Ungroup:** `DELETE /v1/zones/{id}` on the group zone; its members' own zones remain.
- **Volume:**
  - `POST /v1/zones/{id}/volume` (`{"percent"}`, `{"db"}`, `{"mute"}`, `"up"`/`"down"` where the device supports them).
  - A group zone sets every member, so per-member volume is on the members' own zones.
- **Transfer:** `POST /v1/zones/{id}/transfer {to}` moves the queue and position to another zone.
- **Find new devices:** `POST /v1/outputs/rescan` (UPnP, Cast, AirPlay, Snapcast, endpoints).
- **Ask what is on the network:** `GET /v1/outputs/discovery` gives plain-language reasons when
  something is not an output.
