# The connection between controllers and the node

How a controller (the web UI, the app, which wraps the web UI, or `pm-client`) finds a node, stays
connected, and catches up after an outage. Infra brief §2.

## Finding a node

- **Pubky Ring** pairs a controller with the owner's node through the homeserver (ADR 0008).
- **By address**: the app's connect page and the web UI take a URL.
- **On the LAN** (`crates/pm-discovery`): the app's connect page lists the nodes it finds under "On this
  network". A node without a token connects with one click; for a node with a token, clicking fills in
  the address and asks for the token. A node reachable on the network always has one: without `[api]
  token` it makes one on first start (`<data_dir>/api-token`). The web UI in a browser cannot search the LAN.
  - A node announces itself over **mDNS** as `_pubky-music._tcp`. Its TXT records are `role=node`, `id`,
    `name`, `version`, `api=v1` and `auth=token|none`. Endpoints use the same service type with
    `role=endpoint`, and the node's endpoint discovery skips `role=node`.
  - It also announces itself over **SSDP**:
    - it answers `M-SEARCH` for `urn:schemas-pubky-music:device:Node:1`, `upnp:rootdevice`, `ssdp:all`
      and `uuid:<id>`, and sends `NOTIFY` alive messages every 10 minutes and a byebye on shutdown;
    - the `X-PM-ID`, `X-PM-NAME` (percent-encoded), `X-PM-VERSION` and `X-PM-AUTH` headers name it;
    - `LOCATION` is a UPnP description at `/upnp/node.xml`, reachable without the token and naming the
      node only. Its `presentationURL` is the web UI, so the node also shows in Windows' Network folder.
  - The app searches both ways at once for 2.5 s and merges the answers by node id. Two protocols, because
    networks differ: some drop mDNS, others SSDP. On Android, SSDP answers arrive as unicast; mDNS needs a
    Wi-Fi multicast lock, which the app holds only while it searches.
  - The node announces itself whenever `[api] listen` is not a loopback address. Turn it off with
    `[api] announce = false`. In Docker, only host networking puts the announcement on the LAN.
- **Following a node that moved.** `/v1/info` returns the node's stable `id` (a UUID kept in its library),
  and the app remembers it. When the saved address stops answering, for example after the router handed
  out a new IP, the app searches the LAN for that id, switches to the new address and saves it. Checked
  under Xvfb: an address pointed at a dead port was followed to the node's LAN address, and the node's UI
  opened signed in.

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
- **Reconnects** with backoff and jitter:
  - For the first 20 s of an outage, and while a change is held, it retries at most every 1.5 s, so a restarted node is picked up within about a second.
  - After that, the delay doubles from 2 s up to 30 s.
  - The actual wait is drawn at random between half and all of the delay, so many controllers don't reconnect in step.
  - The time of the next retry is on `#conn` as `data-retry-at` (ms since the epoch), for tests.
  - Only the latest reconnect attempt reports its result.
  - It retries at once when:
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
0008), and from anywhere over **iroh** (ADR 0013, `crates/pm-remote`):

- **The node.** While it has a Pubky session and `[remote] enabled` is on (the default), it runs an iroh
  endpoint with its own key (`<data_dir>/remote.key`, mode 600).
  - It accepts the ALPN `pubky-music/http/1` and pipes each stream to its HTTP listener on loopback.
  - It writes the key into the private node record (`iroh`), and `/v1/info` shows it as `remote`.
  - `[remote] relay` is empty for n0's public relays, takes an `https://` URL for a self-hosted
    iroh-relay, or `none` for direct connections only.
- **The app.** After a Ring sign-in or at start, it first tries the saved address, then the LAN by node
  id. Then, when the record carried a key, it opens a tunnel: `http://127.0.0.1:<port>` on this device,
  each connection one iroh stream.
  - The webview shows the node through it. The web UI, the token, the event socket and its resync work
    unchanged, and every request still needs the token.
  - The built-in player is not registered while remote, because the node cannot reach it. At home
    the app registers it with the node together with a key it made at start; the player answers
    only a node that shows that key, and listens on loopback only when the node is on the same
    computer (`docs/deploy.md`).
- **Away and back.** The app picks the way at connect time: a laptop that leaves home with the app
  open shows "Node offline" until the app reconnects. Switching paths while open is a later step.

## Tests

- `crates/pm-client/tests/resync.rs` covers the wire contract: two controllers get the same numbered
  events, ping/pong works, `resync` returns a fresh snapshot with a new zone, and a restarted node has
  a new `boot`.
- `crates/pm-zones` covers the real-time null output, which `PM_NULL_OUTPUT=realtime` turns on in
  `pm-node` and `pm-endpoint` so playback takes as long as the music.
- `tools/ui-reconnect.js` runs in a real browser, and in CI (`connection` job) on every push: two controllers (1280 and 390) on a playing zone,
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
  | both resync after the node is back (4.7 s outage) | 0.2–0.4 s for the controller with a held change (it retries at once), 0.6–0.9 s for the other (fast phase) |
  | a pause pressed during the outage | goes through when the node is back, and both controllers show it |
  | a change on one controller shows on the other | 0.11 s |
  | after 20 s down | offline, "retrying in N s" |
  | a change during a long outage | refused after 8 s, and not applied when the node comes back |
  | Retry now | reconnects in 31 ms |

  Screenshots from the run are in `docs/ux/shots/connection/`: reconnecting and offline, at 1280 and 390.
