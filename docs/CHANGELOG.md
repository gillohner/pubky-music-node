# Changelog

Newest first. Dates are when the change landed on `main`.

## 2026-10-20

- TIDAL through the official API: catalogue search, albums and tracks in the Sources view, sign-in
  with OAuth 2 + PKCE from either UI, playback of `tidal:track:` URIs through stream manifests and
  HLS segments (FLAC when the subscription allows; DRM refused; previews logged); the signed-in
  user's collection and playlists listed on the Sources page and importable as favourites and
  private Pubky playlists (ISRC where TIDAL has one). Config `[tidal]` or `PM_TIDAL_*`.
- Zones that could play nothing now say the last open error.
- Queue tools in both UIs: shuffle what comes next, save the queue as a playlist, transfer playback
  to another zone, Start radio from a track, an album or an artist page, sleep timer per zone.
- Lyrics from tags or a sidecar `.lrc` (synced lines follow the position); Subsonic `getLyrics` and
  `getLyricsBySongId`.
- Focus: filter albums by hi-res, DSD, CD quality, lossless or lossy, with format badges on tiles.
- New releases from your artists on Home (MusicBrainz release groups for artists tagged with an
  MBID, cached a week per artist).
- Zones page in the web UI and the native app: every zone with transport, volume and selection.
- Homeserver private files: list, upload from track menus and mirror, in both UIs.
- Library backup and restore (`pm-node backup`, `pm-node restore`, `GET /v1/backup`, Settings).
- Volume limit per zone; the Volume request shape now matches the docs (`{db}`, `{percent}`,
  `"up"`, `"down"`, `{mute}`), which fixes volume from both UIs on node zones.
- Web UI installs to a phone home screen (web app manifest) and answers media keys.
- Multi-arch images on GHCR (`release.yml`, cross-compiled arm64).
- Subsonic `jukeboxControl` drives a node zone from any Subsonic client (the zone is picked under
  Settings); `byYear` album lists honour `fromYear`/`toYear`.
- The browser zone's queue survives a reload.
- Libraries beyond 1000 albums or tracks show everything: both UIs page through the API's
  `after` cursor.
- Filter boxes on the Albums, Artists and Tracks pages of both UIs.
- The native app's Albums grid is virtualised (only visible covers are fetched).
- The web UI's Tracks page renders 500 rows at a time with a filter over the whole list.
- Sync no longer fails on a fresh homeserver: a folder nobody has written to yet lists as
  empty instead of a 404 error.
- Queue, repeat, radio, shuffle and transport replies wait for the engine to apply the command.
