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
