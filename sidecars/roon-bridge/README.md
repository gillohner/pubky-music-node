# Roon bridge

A Roon Extension (official `node-roon-api`, handoff §7.4) that connects a running Roon Core to a
Pubky Music Node:

- **Scrobbling**: tracks Roon plays past the halfway mark (or 4 minutes) are posted to
  `POST /v1/activity` with `source: roon`. The node resolves title/artist against its own library
  and records a private activity post only when a canonical identity exists; everything else is
  skipped and logged.
- **Playlist import** (`--import-playlists`): walks Roon's Playlists through the Browse API and
  calls `POST /v1/playlists/import`. The node reports the tracks it could not resolve so the
  migration is honest about what was lost.

```sh
cd sidecars/roon-bridge
npm install
PM_NODE=http://node:7788 PM_TOKEN=... npm start              # scrobble
PM_NODE=http://node:7788 PM_TOKEN=... node index.js --import-playlists
```

Authorize "Pubky Music Bridge" once in Roon → Settings → Extensions.

Status: written against the Extension API documentation; it has not been run against a Roon
Core yet. The Browse walk (which items are tracks, pagination) is the part to verify first.
Nothing here touches RAAT or any undocumented Roon protocol (constraint 1).
