# Music Assistant and the node

[Music Assistant](https://music-assistant.io) (MA) can use this node in two ways, both with MA's own
built-in providers. Nothing has to be installed in MA. Tested with MA 2.10.5.

## The node as a music source (OpenSubsonic)

In MA: Settings › Providers › Add › **OpenSubsonic Media Server Library**:
- **Base URL:** `http://<node>`, with the node's API port (`7788` unless `[api] listen` says
  otherwise);
- **Username:** `[api] subsonic_user`;
- **Password:** the node's API token.

What works:
- importing the library (artists, albums, tracks), search, and cover art;
- playing tracks: MA streams the original files;
- creating playlists on the node;
- similar songs (the same artists) and an artist's top songs;
- lyrics where the node has them.

Gaps:
- **Playlists and favourites:** adding a song to a playlist, or favouriting it, fails for songs with
  no MusicBrainz id or ISRC. The node publishes playlists and favourites to Pubky, and needs a song
  id to do so. MA reports "Failed to add songs, check your permissions". This is under discussion
  (QUESTIONS.md, 2026-10-22).
- **Album and artist info** (biographies, extra images) is empty.
- **Podcasts and bookmarks:** the node has none, and answers with empty lists.

## The node's zones as MA players (UPnP/DLNA)

Every zone is a UPnP/DLNA MediaRenderer on the LAN (`[renderer] enabled`, on by default). MA's DLNA
player provider finds the zones by itself (it searches every 5 minutes; reloading the provider
searches at once). Home Assistant's DLNA Digital Media Renderer integration, BubbleUPnP and other
UPnP controllers find them too.

Like any renderer the zones take no token, so the node answers renderer requests from the LAN only:
a private or loopback source address on its own port, never through remote access (iroh), the
Cloudflare tunnel or a reverse proxy (403). The URIs a controller sets and its event callbacks must
be LAN or public addresses; loopback only for a controller on the node's own machine, and never
link-local (`169.254.x`, e.g. a cloud metadata service).

- **The rule trusts the source address the node sees.** Rootless Docker or Podman (rootlesskit,
  slirp4netns port forwarding) and some NAS app proxies show every client as a private gateway
  address, so with the port published to the internet that way, internet clients would count as LAN.
  Turn the renderer off (`[renderer] enabled = false`) when the port is reachable beyond the LAN
  without a proxy in front.
- **Tailscale and other CGNAT addresses (`100.64.0.0/10`) are not LAN** and get 403. Use remote access
  or the app over Tailscale; a UPnP controller needs to be on the node's local network.
- **A LAN peer can still use DNS rebinding:** the node checks a host name when the URI is set, and the
  zone resolves it again when it fetches. Such a peer is already on the LAN, which the renderer trusts.

- **Two ways MA streams:**
  - *Flow mode*, MA's default for DLNA: one continuous stream; MA does the queue, crossfade and seek.
  - *Flow mode off*: track URLs with `SetNextAVTransportURI`; the zone plays them back to back,
    gapless where the formats match.

  Both were tested: play, pause, resume, seek, next track, end of queue, and titles from MA's
  metadata.
- **Volume and mute** are offered only for zones whose output has hardware volume. Otherwise MA and
  Home Assistant show no slider, rather than one that fails.
- **Formats the zones take:** FLAC, WAV, LPCM, AIFF, MP3, AAC/M4A, Ogg. MA picks its stream format
  from this list.
- **No API token on these routes,** like any renderer: whoever reaches the port on the LAN can play
  to the zones. `[renderer] enabled = false` turns them off.
- **Streams that cannot seek** (MA's flow, internet radio) resume from where they are after a
  pause. A seek they cannot do keeps playing, and the zone reports why.

## Slimproto

MA has its own Slimproto server for squeezelite players, so it does not need the node's. The node's
Slimproto server is for players that should be zones of the node. Run one Slimproto server per
player: both listen on port 3483 by default.
