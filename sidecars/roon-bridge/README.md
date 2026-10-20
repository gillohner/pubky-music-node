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
npm install          # the Roon API packages come from RoonLabs' GitHub repos (they are not on npm)
npm test             # node:test: scrobble rule, 422 handling, Browse walk, API client, no Core needed
PM_NODE=http://node:7788 PM_TOKEN=... npm start              # scrobble
PM_NODE=http://node:7788 PM_TOKEN=... node index.js --import-playlists
```

Authorize "Pubky Music Bridge" once in Roon → Settings → Extensions.

Status: the logic (`lib.js`) is unit-tested against fake zones, a fake Browse API shaped like Roon's
responses and a fake node; the Roon wiring (`index.js`) loads the real RoonLabs packages but has not
been paired with a Core yet (it starts, brings up SOOD discovery and reaches the node; verified by
running it against a live node without a Core on the network). The Browse walk (which items are tracks, pagination) is the part to
verify first on a real Core.
Nothing here touches RAAT or any undocumented Roon protocol (constraint 1).
