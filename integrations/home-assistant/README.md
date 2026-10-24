# Pubky Music for Home Assistant

A Home Assistant custom integration for a [Pubky Music](https://github.com/gillohner/pubky-music-node) node:
every zone becomes a `media_player`, and the node's library is in Home Assistant's media browser.

It talks to the node's own HTTP API and its live events socket (`/v1/events`) with the node's API token.
Nothing leaves your network.

## What you get

- **One media player per zone.** You get play, pause, stop, next, previous and seek. Repeat works, and so
  does shuffle: it shuffles the rest of the queue once, since the node keeps no shuffle mode. Clearing the
  queue and enqueueing (play now, next, add) work too.
- **Volume and mute** on zones whose output has hardware volume. The node never turns the volume down in
  software, so a bit-perfect zone stays bit-perfect and has no volume slider.
- **Source** sets the node's play-from order. The source you pick plays a song first when several have it.
  This is a node-wide setting, not per zone.
- **Grouping** (`media_player.join` and `media_player.unjoin`) makes a group zone of the zones' outputs and
  moves the queue to it. Unjoining moves the queue back and removes the group zone.
- **Media browser:** Home (favourite albums, recently added, recently played, most played), Albums,
  Artists, Playlists and Genres, with covers, plus Home Assistant's own media sources (local media and
  text-to-speech). Search works from the browser and from voice (`media_player.search_media`).
- **`play_media`:**
  - `pubky-music://track/<id>`, `album/<id>`, `artist/<id>`, `genre/<id>` and `playlist/<id>`;
  - any `http(s)` URL;
  - the node's own source URIs, such as `tidal:track:123`, `qobuz:track:…` and `youtube:video:…`;
  - Music Assistant style URIs for services the node has (`tidal://track/123` and the like);
  - Home Assistant media sources.
- **Updates are pushed:** zones change in Home Assistant as they change on the node, including zones added
  or removed there.

- **Announcements** (`play_media` with `announce: true`, so TTS and doorbells work): the clip plays instead
  of the music, then the zone goes back to the same track at the same position, playing or paused as it was.
  `extra.chime: true` adds a short chime first; `extra.volume` (0 to 1, or 0 to 100) sets the clip's volume on
  outputs with hardware volume and restores it afterwards. The music pauses rather than ducks, since the
  zones have no software gain.

## Install

**HACS:** add this integration's repository as a custom repository (category *Integration*), install
*Pubky Music*, then restart Home Assistant.

**By hand:** copy `custom_components/pubky_music` into your Home Assistant `config/custom_components/`
folder and restart.

## Set up

Home Assistant finds nodes on your network by themselves (zeroconf, `_pubky-music._tcp`). Look under
Settings › Devices & services › Discovered and enter the node's API token. You can also add it by hand:
*Add integration › Pubky Music*, then enter the node's address, its port (7788 unless you changed it) and
its API token. The token is in the node's Settings › Node, or `[api] token` in `node.toml`.

Each node is one device, and each zone is a device under it.

## Without this integration

The node's zones are also DLNA renderers, so Home Assistant's built-in DLNA integration offers each zone
as a basic media player, without the library. See
[docs/home-assistant.md](https://github.com/gillohner/pubky-music-node/blob/main/docs/home-assistant.md).
