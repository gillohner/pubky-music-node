# ADR 0009: Grouped outputs stay in time by sample slip

Status: accepted, 2026-10-20

## Context

A group zone (ADR 0005, `GroupOutput`) writes the same blocks to several outputs. Consumer
devices run on their own crystals, a few hundred ppm apart, so after minutes the members drift
audibly. Roon's RAAT solves this with a clock-synchronised transport we deliberately do not clone
(handoff §2); our own endpoint protocol (ADR 0007) is Ack-paced and carries no clock yet.

## Decision

The group sink measures each member's audible position (frames written minus frames its buffer
still holds, minus pre-roll and slips) against the leader's and, for PCM, corrects any drift beyond
2 ms by duplicating the last frame of a block (member ahead) or dropping it (member behind), at most
one frame per 1000 written. The slips are counted per member (`MemberStatus.slipped_frames`) and
the zone's signal path carries a `sync` stage of quality `Enhanced` for grouped outputs, so a
grouped zone never claims bit-perfect.

## Consequences

* Groups hold within the deadband for clock errors up to 1000 ppm; this is tested with a simulated
  clock and awaits a two-endpoint measurement on hardware.
* Single-sample slips are the crudest correction; adaptive resampling (a `src` stage with a
  continuously adjusted ratio) would be audibly cleaner and is the upgrade path, behind the same
  measurement.
* The measurement trusts each output's `buffer_level`; an output whose queue report does not track
  its hardware (the null output, a renderer that hides its buffer) appears in time and is not
  corrected.
* DSD groups are not corrected.
