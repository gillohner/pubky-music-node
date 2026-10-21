# The connection between controllers and the node

How a controller (the web UI, the app, which wraps the web UI, or `pm-client`) finds a node, stays
connected, and catches up after an outage. Infra brief §2.

## Finding a node

- **Pubky Ring** pairs a controller with the owner's node through the homeserver (ADR 0008).
- **By address**: the app's connect page and the web UI take a URL.
- **On the LAN**: mDNS `_pubky-music._tcp` and SSDP (in progress, see below).

## Staying connected: `/v1/events`

One WebSocket per controller carries every change. The REST API (`/v1/...`) carries commands and reads.

**The node:**
- The first message on a socket is a snapshot:
  `{"type":"snapshot","seq":41,"boot":"9f2c…","library_rev":3,"zones":[…]}`.
  - `seq` is the number of the last event before the snapshot.
  - `boot` is a random id, new on every node start.
  - `library_rev` counts library changes in this run.
- Every event after the snapshot carries the next `seq` (`{"seq":42,"type":"zone",…}`). Numbers count
  per node run and arrive in order on every socket. A zone that is created is announced at once with
  a `zone` event, so every controller sees it.
- When a socket falls behind (more than 1024 events queued), the node skips ahead and sends a fresh
  snapshot.
- The node answers `{"type":"ping","id":n}` with `{"type":"pong","id":n}`. Pongs carry no `seq`.
- On `{"type":"resync"}` the node sends a fresh snapshot.
- The node sends a WebSocket ping every 20 s. A controller that has pinged at least once is dropped
  after 45 s of silence, so the half-open socket of a laptop that went to sleep does not linger.
  Controllers that never ping (`pm-client`, older UIs) are not held to this.

**The web UI** (`crates/pm-api/static/app.js`, "connection + events"):
- **Reconnects** with exponential backoff and jitter: the delay doubles from 0.5 s up to 30 s, and the
  actual wait is drawn at random between half and all of it, so many controllers don't reconnect in
  step after a node restart. It retries at once when:
  - the network comes back (`online`);
  - the page becomes visible again (phones and laptops suspend sockets in the background);
  - the user clicks the status or "Retry now";
  - a change is waiting to be sent.
- **Resyncs without a reload.** Each reconnect re-reads `/v1/info`, takes the zones from the snapshot,
  and refreshes the queue, Now Playing and the zones view.
  - If `boot` changed (the node restarted) or `library_rev` changed (the library changed while the UI
    was away), it also reloads the library views.
  - A gap in `seq` means it missed something, so it asks for `resync`.
- **Latency**: it pings every 10 s and shows the round-trip time next to the node name. If two pings in
  a row go unanswered, it drops the socket and reconnects, without waiting for TCP to give up.
- **Changes made while disconnected** (anything but GET) wait up to 8 s for the connection to come
  back, then go out. Otherwise they are refused with "Not connected to the node, so nothing was
  changed", and nothing is sent later. A stale "play" never fires minutes after it was pressed.
- **State**, shown at the bottom of the sidebar (`#conn`, `data-state`). When not connected for more
  than 2 s, it is also shown as a banner at the top with "Retry now":

  | State | Shown as |
  | :-- | :-- |
  | connected | the node's name, track count and round-trip time |
  | reconnecting | for the first 20 s |
  | offline | "Node offline · retrying in N s", or "Offline: no network" when the device has no network |

## Remote access

Today a node is reached outside the LAN over HTTPS (the address it publishes on the homeserver, ADR
0008). See the ADR on iroh for the plan to dial nodes by key.

## Tests

- `crates/pm-client/tests/resync.rs` covers the wire contract: two controllers get the same numbered
  events, ping/pong works, `resync` returns a fresh snapshot with a new zone, and a restarted node has
  a new `boot`.
- `crates/pm-zones` covers the real-time null output, which `PM_NULL_OUTPUT=realtime` turns on in
  `pm-node` and `pm-endpoint` so playback takes as long as the music.
- `tools/ui-reconnect.js` runs in a real browser: two controllers (1280 and 390) on a playing zone,
  and the script kills the node and starts it again.

  ```sh
  cd <dir with playwright@1.49.1>   # npm install playwright@1.49.1 && npx playwright install chromium
  PM_NULL_OUTPUT=realtime node tools/ui-reconnect.js <token> http://127.0.0.1:7799 <shots dir> \
    -- target/debug/pm-node --config node.toml serve
  ```

  Measured on 2026-10-21, against a local debug build:

  | Check | Result |
  | :-- | :-- |
  | both controllers notice a killed node | 2 ms (the socket closes; a vanished host takes ≤ 20 s, two missed pings) |
  | both resync after the node is back (3.3 s outage) | 0.3–0.6 s for the controller with a held change (it retries at once), 1.5–1.7 s for the other (backoff) |
  | a pause pressed during the outage | goes through when the node is back, and both controllers show it |
  | a change on one controller shows on the other | 0.11 s |
  | after 20 s down | offline, "retrying in N s" |
  | a change during a long outage | refused after 8 s, and not applied when the node comes back |
  | Retry now | reconnects in 31 ms |

  Screenshots from the run are in `docs/ux/shots/connection/`: reconnecting and offline, at 1280 and 390.
