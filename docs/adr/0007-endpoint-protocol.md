# ADR 0007: Own endpoint protocol (`pm-stream`) instead of OAAT

Status: accepted (2026-10-20). Resolves the handoff §11 question "OAAT: adopt, fork, or own".

## Context

M2 needs a dumb endpoint (a Pi next to a DAC) that plays exactly what the node sends, with the
endpoint's DAC as clock master (§6.1). OAAT was evaluated as an option; what the node needs is
small and specific: format negotiation by `OutputCaps`, a push stream with back-pressure from the
DAC buffer, flush/drain/volume, and discovery. Adopting a general transport would add a dependency
and a second notion of formats for little gain, and a fork would be ours to maintain anyway.

## Decision

- `pm-stream`: TCP, frames of `kind:u8, len:u32, payload`. Control frames are JSON
  (`Hello/Welcome/Open/Opened/Ack/Flush/Drain/SetVolume/Close/Error`); `Audio` frames carry the
  `AudioBlock` samples verbatim (`i32` LE for PCM, bytes for DSD). No conversion on the wire.
- **The endpoint owns the clock**: every `Audio` is answered by `Ack` only after the block is in
  the endpoint's sink. The node's `NetSink::write` blocks on that reply, so a full DAC buffer stalls
  the zone thread exactly as a local ALSA buffer does. No timers or rate estimation on the node.
- One TCP session per open sink; an output in use refuses a second `Open`. `Welcome` lists the
  endpoint's outputs with their caps, so negotiation is the pipeline's usual `Plan::negotiate`.
- `pm-endpoint` is a std-thread library with no runtime; `pm-out-net` implements `Output` for the
  node. Both reuse the local outputs (`NullOutput`, ALSA), so an endpoint is the M1 output over TCP.

### 2026-10-28: the app's built-in player only, paired with its own node

The owner's decision after the security review of 2026-10-27 (#6: anyone on the LAN could play to
an endpoint or hold its sessions). `pm-endpoint` is only the Pubky Music app's built-in player;
speakers on a Pi or another computer with a DAC run squeezelite (the node's Slimproto server), and
network speakers keep their own protocols.

- **Key.** The app makes a random key (32 bytes from the OS CSPRNG, hex) each time it starts the
  player, and registers it with its node together with the address (`POST /v1/outputs/endpoints
  {address, key}`, with the node token). The node keeps it per address in a 0600 file in its data
  folder, replaces it when the same address registers again, and never returns or logs it.
- **Hello.** Protocol 2: `Hello` carries the key. The endpoint compares it in constant time within
  the `Hello` deadline and answers a missing or wrong key, or another protocol version, with an
  `Error` naming the reason (both versions for a mismatch) before anything else.
- **Connections.** One without the key waits in a small separate cap (8) and is closed by the
  `Hello` deadline (10 s for all of it, not per read); only a client with the key takes one of the
  16 session slots.
- **No discovery.** Endpoints no longer advertise over mDNS and the node no longer browses or reads
  `[outputs] endpoints` (it warns when that is set): an endpoint is added by registration only.
- **Loopback when local.** For a node on the same computer the player listens on 127.0.0.1 and
  registers that address; otherwise it listens on every interface and registers the address it has
  on the way to the node, listening again elsewhere when the node changes.
- The headless `pm-endpoint` binary stays for tests and development and needs a key too
  (`--key-file`, made 0600 on first start, or `--key`).

## Consequences

- Bit-perfect end to end: the loopback test plays files through an endpoint into a WAV output and
  gets the exact concatenation; the hardware proof is the M1 ALSA test run on the endpoint box.
- Latency is one round trip per block (a few ms on a LAN); buffering stays in the DAC buffer plus
  TCP. Grouping (§6.5) will add a clock report (`Ack` already carries `frames_written`) and
  per-endpoint delay; the protocol gets a `Clock` frame then, not a redesign.
- Audio is unencrypted on the LAN segment; remote access goes through the pubkey layer (M5), never
  by exposing this port.
