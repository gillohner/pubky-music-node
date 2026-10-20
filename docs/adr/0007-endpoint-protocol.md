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
- Discovery: mDNS `_pubky-music._tcp` (TXT `name`, `version`), plus static addresses in the node
  config. The node connects at start; hot-plug of endpoints is a follow-up (re-browse on demand).
- `pm-endpoint` is a std-thread binary with no runtime; `pm-out-net` implements `Output` for the
  node. Both reuse the local outputs (`NullOutput`, ALSA), so an endpoint is the M1 output over TCP.

## Consequences

- Bit-perfect end to end: the loopback test plays files through an endpoint into a WAV output and
  gets the exact concatenation; the hardware proof is the M1 ALSA test run on the endpoint box.
- Latency is one round trip per block (a few ms on a LAN); buffering stays in the DAC buffer plus
  TCP. Grouping (§6.5) will add a clock report (`Ack` already carries `frames_written`) and
  per-endpoint delay; the protocol gets a `Clock` frame then, not a redesign.
- Audio is unencrypted on the LAN segment; remote access goes through the pubkey layer (M5), never
  by exposing this port.
