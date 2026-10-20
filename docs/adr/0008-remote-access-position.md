# ADR 0008: Remote access, step one: find your node by pubkey

Status: accepted (2026-10-20); the transport half of M5 stays open.

## Context

M5 wants a phone on cellular to reach the node by pubkey with no port forwarding, over Pubky
Noise, with a relay or hole punching for NAT. The available `pubky-noise` crate (0.1.0) is a
store-and-forward messaging layer: peers exchange Noise packets as files on their homeservers and
poll. That is right for messages and wrong for an interactive API or an audio stream. No Pubky
live transport or relay exists today (handoff §11, "relay design; who operates relays").

## Decision

- **Discovery by pubkey now.** When the node has a session, `pm-node serve` writes
  `/priv/pubky.music/node.json` on the user's homeserver: `{name, api_url, token, version,
  updated_at}`. The folder is private, so only the user's own devices (with their session) read
  it, which makes carrying the API token there acceptable: it is how the user's phone learns where
  and how to reach their node without typing anything. `api_url` comes from `[api] public_url`
  (a hostname the user already exposes: LAN name, Tailscale, a reverse proxy) and falls back to
  the listen address.
- **Transport unchanged.** Clients still speak HTTPS/WebSocket to that URL. Encryption and
  authentication on the wire are the URL's business (TLS at a proxy, an overlay network); the
  node never exposes the endpoint protocol port this way.
- **Noise transport deferred.** When a live Pubky transport or relay exists, the node publishes
  its reachability there and the controller gains a second connector; the API, the token and the
  JSON shapes do not change. `pubky-noise` as it is may still serve for small control messages
  (wake, pairing) later.

## Consequences

- A user with a Pubky session pastes it into the controller once; the app resolves the node from
  the homeserver and connects. Several nodes per user are possible later by naming the document
  per node.
- Nothing here claims NAT traversal. Cellular access needs the user's own reachable URL until the
  relay question is settled; the README says so.
