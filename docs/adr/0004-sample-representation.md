# ADR 0004: One in-memory sample layout for bit-perfect playback

Status: accepted (2026-10-19).

## Context

Constraint 5: bit-perfect by default, any conversion visible in the signal path. That is only
provable if the data between decoder and output has one unambiguous representation.

## Decision

`pm_model::AudioBlock` carries one of three layouts:

- `PcmI32`: interleaved, sign-extended, right-justified integers with `format.bits` valid bits.
  Passing a block through unchanged is bit-perfect by construction; outputs pack it.
- `PcmF64`: interleaved `f64` in `[-1, 1]`, the DSP working format. Int→f64→int without a stage in
  between is exact for ≤ 32-bit integers (tested in `pm-model`).
- `Dsd`: bytes interleaved one per channel, oldest bit in the MSB (DFF and ALSA `DSD_U8` order).
  DSF (LSB-first, channel-blocked) is re-ordered by its reader.

An output never converts. If it cannot take the block's format as-is it refuses, and the
pipeline plans the conversion and records a `PathStage` for it.

## Consequences

- Outputs are simple and auditable; every conversion lives in `pm-audio::pipeline`.
- 24-bit audio over a 32-bit device is a `pad` stage marked `Lossless`, never silent.
