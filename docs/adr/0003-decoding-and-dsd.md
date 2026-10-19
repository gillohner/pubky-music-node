# ADR 0003: Symphonia for PCM containers, own readers for DSF/DFF

Status: accepted (2026-10-19).

## Context

Handoff §4 names Symphonia with an ffmpeg fallback for edge formats. Symphonia 0.6 decodes FLAC,
ALAC (in MP4), WAV and AIFF in pure Rust and verifies FLAC's STREAMINFO MD5, which gives a
bit-exactness proof for free. It has no DSD support.

## Decision

- PCM: Symphonia 0.6, with verification on. Samples are re-justified into `pm_model` layout.
- DSD: `pm-audio` has its own DSF and DFF readers. Both formats are a handful of chunks; writing
  them avoids an ffmpeg dependency on the audio path and keeps the bitstream untouched.
- ffmpeg remains the fallback for formats outside M1's list, behind a feature, in a later milestone.

## Consequences

- No native code on the decode path; the node cross-compiles to a Pi with `cargo` alone.
- DSD→PCM conversion is not provided by the readers; it is a pipeline stage (M8).
