AcoustID `/v2/lookup` answers in the documented shape (`meta=recordings releasegroups releases tracks
compress`). Constructed, not recorded: the node has no AcoustID application key yet. The MBIDs of the
recordings are real (Bohemian Rhapsody and a duplicate recording); the release ids and track lists are
illustrative. Replace with recorded answers once the key exists (steward identify brief).

`recorded-bohemian-trackid.json` is a real answer, recorded 2026-10-21 with the node's application key
(`/v2/lookup?trackid=84eed5cd-5b1f-4f5c-8c92-2686d2877862`, same `meta`): Bohemian Rhapsody with 133
release groups. A lookup by track id has score 1.0; a fingerprint lookup carries the match score.
