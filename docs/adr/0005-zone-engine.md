# ADR 0005: Zone engine: one audio thread, gapless by format, pause as flush-and-seek

Status: accepted (2026-10-19).

## Context

Handoff §6.4: a zone is one or more grouped outputs plus a queue and transport state. §6.1: the
endpoint owns the clock; the core pushes into a generous buffer. `AudioSink` methods block, and
the M1 hardware test measured a 614 ms gap between two `pm-node play` runs (process restart,
device reopen, end-of-stream pad). The zone layer has to remove that gap without touching the
sample path, and has to work the same for a DAC, a network endpoint and a file.

## Decision

- **One `std` thread per zone** (`pm_zones::engine`). The DAC paces the loop through
  `AudioSink::write`; between blocks the thread polls its command channel, while stopped or
  paused it sleeps on it. No async runtime is involved in the audio path; `Output::open` is
  driven by a thread-park `block_on`.
- **Codec-free zones.** `pm-zones` pulls audio through three traits, `FrameSource`,
  `Processor` and `SourceFactory`, which `pm-audio` implements (`pm_audio::LocalFiles`). The
  engine is tested with synthetic sources; the pipeline's bit-perfect guarantees are not
  re-tested here, they are inherited.
- **Gapless rule:** at the end of a track the next item is already opened and planned
  (prefetched once per track). If its negotiated output format equals the open sink's format,
  the sink stays open and the next block follows the last with nothing between them. A format
  change drains the sink (the tail plays out) and reopens it. Nothing is ever inserted,
  padded or faded, so a gapless join is bit-identical to the concatenation of the two
  tracks (tested: two FLAC/WAV files into one WAV).
- **Pause = flush + remember, resume = seek.** `AudioSink` has no pause. Pausing discards the
  sink's buffer and records the audible position (frames written minus frames still queued);
  resuming seeks the source there. Seek does the same with a new target. Position accuracy is
  the source's seek accuracy: sample-exact for DSF/DFF, Symphonia accurate mode for PCM.
- **Position** = `base_ms + frames_to_ms(frames_written_since_base − queued_frames)`, in the
  output format's rate. Right after a gapless join this is clamped at 0 until the previous
  track's tail has left the buffer.
- **Unopenable items are skipped**, not fatal: a `Skipped` event, then the next item, at most
  one pass over the queue. Sink errors stop the zone with `ZoneState.error` set.
- **State and events.** `ZoneState` goes out through a `tokio::sync::watch` (position refreshed
  every 250 ms while playing, every transition immediately); `ZoneEvent`s through a
  `broadcast`. Every command carries a sequence number the engine acknowledges in the state,
  so a caller can wait for "everything I sent was handled and the zone is stopped".
- **Queue** (`pm_zones::Queue`): ids assigned on insert, current index follows its item through
  removes and moves, repeat off/all/one. Removing the playing item lets whatever moved into its
  slot play next.

## Consequences

- `pm-node play a.flac b.flac --to <output>` plays the album gapless through one open sink; the
  M1 gap goes away for same-format tracks. A format change still drains and reopens, which is
  what a DAC needs anyway (it re-locks).
- Groups (§6.5) are not here yet: a zone has one output. Grouping will give the engine several
  sinks and a reference clock; the queue, transport and state model do not change.
- An `AudioSink::pause` with hardware support (ALSA `snd_pcm_pause`) can later replace the
  flush-and-seek for sinks that have it; the position contract stays the same.
- The engine never calls `drain` between tracks, so the ALSA end-of-stream silence pad from M1
  is paid once per queue, not once per track.
