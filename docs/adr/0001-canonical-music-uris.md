# ADR 0001: Canonical music URIs are MusicBrainz and service URLs

Status: accepted (2026-10-19). Supersedes handoff §8.2's `music:` scheme.

## Context

Handoff §8.2 proposed `music:recording:mbid:<uuid>`, `music:recording:isrc:<ISRC>`,
`music:release:mbid:<uuid>` and `music:artist:mbid:<uuid>`. §12 (the Pinakes context) is
authoritative where it conflicts with §8: Pinakes reads web URLs and a few identifier schemes
and **refuses an unknown `music:` scheme**. Tags and playlist items that use it would never be
indexed, never ranked and never found by friends.

## Decision

Every social object (tag target, playlist item, review target, activity item) carries a real URL:

| Entity | URL | Notes |
| :-- | :-- | :-- |
| recording | `https://musicbrainz.org/recording/<mbid>` | preferred |
| release | `https://musicbrainz.org/release/<mbid>` | or `/release-group/<mbid>` for the album as a work |
| artist | `https://musicbrainz.org/artist/<mbid>` | |
| no MBID, ISRC known | `https://musicbrainz.org/isrc/<ISRC>` | **our fallback; still to agree with the Pinakes owner** |
| neither | the service's canonical track/album/artist URL | Pinakes links service URLs to the MBID |

`pm_model::MusicUri` is the only type that produces or parses these. It rejects `music:` with a
dedicated error. Service URLs are normalized (tracking parameters dropped, locale prefixes removed)
so one recording on one service has one spelling; Nexus and Pinakes do not fold spellings.

Resolution order on the node stays as §8.2 says: MBID → ISRC → AcoustID → fuzzy, with a stored
confidence; nothing under `Confidence::PUBLISH_THRESHOLD` (0.85) is published.

## Consequences

- Clients join Pinakes cards and Nexus tags by URL, never by either side's id.
- The ISRC fallback publishes a MusicBrainz *list* page, not one recording. Once the node resolves
  the ISRC to an MBID it should re-tag with the recording URL; until the Pinakes owner confirms,
  treat ISRC URIs as local-only.
- Pinakes step 8 will type music cards (`MusicRecording`, `MusicAlbum`, `MusicGroup`) and add
  `also_on`; the node's client types carry those fields as optional today.
