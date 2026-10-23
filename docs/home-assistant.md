# Home Assistant

Two ways for Home Assistant to use the node:

1. **Built in, nothing to install:** every zone is a UPnP/DLNA MediaRenderer, so Home Assistant's own
   **DLNA Digital Media Renderer** (`dlna_dmr`) integration finds it and gives each zone a `media_player`.
2. **Pubky Music integration** (custom, through HACS): richer, with the library in the media browser.
   See [the integration's README](../integrations/home-assistant/README.md) once it lands.

## 1. DLNA (`dlna_dmr`): verified with a real Home Assistant

`tools/ha-check.py dlna` runs a node on the audio fixtures and a real Home Assistant, onboards it
through its own API, and drives the zone from Home Assistant. Checked with Home Assistant 2026.10.2:

| Step | Result |
|---|---|
| Discovery | Home Assistant's SSDP finds each zone's `MediaRenderer:1` and `dlna_dmr` offers it within seconds (Settings › Devices & services › Discovered); confirming creates the entry, named after the zone |
| Play a URL (`media_player.play_media`) | the zone plays it; Home Assistant shows *playing*, the position moves |
| Pause, play, stop | the zone follows each; Home Assistant's state follows through the renderer's events |
| Started from the node | a zone started in the node's own UI shows *playing* in Home Assistant, with the track's title |
| Next, previous, seek | offered |
| Volume and mute | offered **only when the zone's output has hardware volume**. The renderer never attenuates in software, so a bit-perfect zone stays bit-perfect; zones on outputs without hardware volume have no volume slider in Home Assistant |
| Media browser | `dlna_dmr` shows Home Assistant's own media sources, not the node's library (that's what the integration adds) |

What DLNA can't give Home Assistant: the node's library and playlists in the media browser, shuffle and
repeat, grouping zones, the play-from source, and anything that needs the API token. Those come with the
integration.

Notes:
- The renderer takes no API token, like any renderer on a LAN: whoever can reach the port can play to
  the zones. `[renderer] enabled = false` turns it off.
- Discovery is multicast (SSDP, UDP 1900). Home Assistant in Docker needs `network_mode: host` for it,
  as for any DLNA device; across VLANs, add the renderer in `dlna_dmr` by its description URL,
  `http://<node>:<port>/upnp/zone/<zone>/desc.xml`.

Run it yourself (Home Assistant from a virtualenv, or the official image with `--docker`):

```sh
cargo build -p pm-node
python3 -m venv /tmp/ha && /tmp/ha/bin/pip install homeassistant
python3 -I tools/ha-check.py dlna target/debug/pm-node /tmp/hacheck --hass /tmp/ha/bin/hass
# or: python3 -I tools/ha-check.py dlna target/debug/pm-node /tmp/hacheck --docker ghcr.io/home-assistant/home-assistant:stable
```
