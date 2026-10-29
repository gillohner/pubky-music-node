# ADR 0013: Remote access over iroh, dialed by the node's key

Status: accepted and built, 2026-10-21 (infra agent; steps 1 and 2 of the build order below). It
completes the transport half of M5 that ADR 0008 left open.

## Context

ADR 0008 lets a controller find its node by pubkey: the node writes `{api_url, token}` into the
owner's private homeserver folder. The wire stays HTTPS to `api_url`, so remote access still needs
one of these:
- port forwarding;
- a reverse proxy with TLS;
- an overlay network (Tailscale);
- a Cloudflare tunnel.

Most owners have none of them, so in practice the node works only on the LAN. The brief (§2) asks to
evaluate **iroh** for control and streaming: QUIC, dialed by the node's public key, hole punching
with relay fallback, and authorized by the Pubky Ring session.

**What iroh is today** (checked 2026-10-21):
- **Versions and stability.** iroh 1.0 shipped on 2026-06-15. 1.4.0 came out on 2026-10-20; the node pins 1.3.0 (2026-09-28), the newest release at least two weeks old. v1 promises a
  stable wire protocol and API, and any wire change comes with a major release.
- **Dialing.** An `Endpoint` is dialed by its `EndpointId`, an Ed25519 public key. The key also
  authenticates and encrypts the connection, which is QUIC with multipath and NAT traversal.
- **Relays.** When a direct path cannot be punched, traffic goes through a relay. The n0 public
  relays are free but rate-limited. The relay is open source, and n0 also sells hosted ones.
- **Finding an endpoint's address.** `iroh::address_lookup` has DNS and **pkarr** lookup
  (`PkarrPublisher`, `PkarrResolver`), with Mainline DHT lookup in a separate crate. pkarr is Pubky's
  own building block, so iroh speaks the same language as the rest of the stack.
- **Bindings.** iroh compiles to WASM for browsers (relay only), and there are official Kotlin and
  Swift bindings.

### Options

1. **Keep HTTPS only** and document Tailscale or Cloudflare. That costs nothing, but most owners stay
   LAN-only.
2. **A Pubky transport** (pubky-noise). ADR 0008 found it store-and-forward, so it can't carry an API
   or audio. Nothing live exists.
3. **WebRTC** (data channels through a TURN/STUN server). Browsers have it natively, but it needs
   signalling and a TURN server someone runs, and the Rust stacks are heavy.
4. **iroh.** It is a plain Rust library on both ends, keys rather than accounts, and relay fallback
   needs no one to host a server. The browser path is relay-only, through WASM.

## Decision

**Use iroh (option 4) as a second way to reach the node, keyed by a node key and authorized by the
same token the LAN uses.**

- **The node.**
  - It gets an iroh `SecretKey` of its own, kept in its data directory with mode 600. It is not the
    owner's Pubky key: if the node leaks, the owner's identity is safe, and the key can be rotated.
  - It binds an `Endpoint` with the n0 relays by default, or `[remote] relay = "<url>"` for a
    self-hosted relay. `[remote] enabled = true` turns it on.
  - It accepts one ALPN, `pubky-music/http/1`. Each bidirectional stream is piped, byte for byte,
    to the node's own HTTP listener on loopback.
  - The HTTP API, the WebSocket, HLS and the pull streams therefore work unchanged over the tunnel.
    Auth stays the bearer token, inside the tunnel.
- **Finding the node.** The node adds `iroh: "<EndpointId>"` (and its relay URL) to the private node
  record of ADR 0008. Only the owner's devices, signed in with Pubky Ring, can read the record, and it
  already carries the token. A controller that has it dials by key; iroh's pkarr lookup finds the
  current addresses. No port is opened.
- **The app (Tauri).**
  - When the record has an `iroh` id and `api_url` does not answer, or the owner chooses "away from
    home", the app starts a local proxy on `127.0.0.1:<random>`. Each TCP connection to it becomes one
    iroh stream to the node.
  - The webview loads `http://127.0.0.1:<port>`, so the web UI needs no change. The token reaches the
    page the same way as on the LAN.
  - Android is the same Rust code. The connect page shows "Remote · direct" or "Remote · relayed"
    from iroh's path information.
- **Browsers without the app.** They are out of scope for now. iroh's WASM build is relay-only and
  can't serve a webview origin, so browsers keep using HTTPS (`api_url`) where the owner set one up.
- **Who may connect.**
  - Any key may open a connection, but every request still needs the token, exactly like the LAN.
  - A later step can pin the controller keys that signed in through Ring: the node would keep their
    `EndpointId`s and refuse others before reading a byte. This is not needed for the first version,
    because the token already gates everything.
- **Audio.** Remote listening uses the same tunnel: HLS or the pull stream to the browser zone, or a
  zone on the phone later. The app's built-in player over iroh is a later step; the endpoint
  protocol (ADR 0007) is a byte stream and would pipe the same way. Nothing becomes
  public: the relay sees only encrypted QUIC (handoff §2).

## Consequences

- Owners reach their node from anywhere with no router setup, at the cost of a QUIC endpoint on the
  node (one UDP socket and a relay connection).
- **Dependence on n0's public relays.** They are free, rate-limited, and no audio is readable to
  them. If they go away or throttle, `[remote] relay` points at a self-hosted `iroh-relay`; the
  pubky-infra side could run one next to the homeserver.
- **Relayed sessions carry audio through the relay.** At 16/44.1 FLAC that is about 1 Mbit/s, under
  the public relays' limits for one listener. Bit-perfect hi-res over a relay is not promised: when
  the path is relayed, the UI shows it.
- `iroh` 1.x moves into the node and the app (about 2–3 MB added to the app). It is pinned to an
  exact version (`=1.3.0`) like Tauri, and v1's stability promise keeps upgrades cheap.
- ADR 0008's record grows by one optional field. Old apps ignore it, and new apps fall back to
  `api_url`.

## Build order

Steps 1 and 2 are built (`crates/pm-remote`, `pm-api/src/remote.rs`, the app's `reach`). Tested:
- `crates/pm-client/tests/tunnel.rs` runs the API and `/v1/events` through an in-process tunnel with
  relays off, and checks that a wrong token is still refused.
- By hand, on 2026-10-21, through n0's public relays from this sandbox:
  - `cargo run -p pm-remote --example tunnel -- serve` and `-- dial <key>` connected by key alone in
    0.2 s;
  - the app, whose saved address pointed nowhere, reached the node over the tunnel and opened its UI
    signed in, with the event socket live.
- Step 3 (the owner's phone on cellular) is open.


1. Node: the key, the endpoint, the ALPN pipe to loopback, `[remote]` config, and the `iroh` field in
   the node record. An integration test pipes `/v1/info` and `/v1/events` through an in-process
   endpoint pair with relays off.
2. App: the local proxy and the fallback from `api_url` to iroh, with the connect page's remote
   label. A test runs a node and the app proxy as two processes on one machine.
3. Measure on real networks (the owner's phone on cellular): time to connect, direct versus relayed,
   and resync after a network switch (`docs/CONNECTION.md` covers the reconnect).
4. Later: pin controller keys, and `pm-endpoint` over iroh.
