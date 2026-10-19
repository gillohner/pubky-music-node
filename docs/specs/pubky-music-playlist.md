# Draft: `PubkyMusicPlaylist` for pubky-app-specs

Status: draft for the Nexus/Pinakes owner (answers §6). Implemented in `pm-pubky::schemas::playlist`;
the wire form below is what the node writes today. Once stable, Nexus adds a `collection`
reference kind for it and playlist items count toward Pinakes ranking.

## Path

`/pub/pubky.music/playlists/<id>`, `id` a Timestamp ID (microseconds → 8 big-endian bytes →
Crockford base32, 13 characters), the same generator as `PubkyAppPost`. The namespace name
`pubky.music` is a placeholder constant until the app name is decided (handoff §11).

## Body

```json
{
  "name": "Late night",
  "description": "Quiet ones",
  "image": "pubky://<z32>/pub/pubky.app/files/<id>",
  "items": [
    { "uri": "https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862", "added_at": 1760900000000000 },
    { "uri": "https://musicbrainz.org/isrc/USQX91300108", "added_at": 1760900001000000 },
    { "uri": "https://open.spotify.com/track/2Foc5Q5nqNiosCNqttzHof", "added_at": 1760900002000000 }
  ],
  "created_at": 1760900000000000,
  "updated_at": 1760900002000000
}
```

| Field | Rule |
| :-- | :-- |
| `name` | 1–100 Unicode scalars after trimming |
| `description` | optional, ≤ 500 |
| `image` | optional, ≤ 200, `pubky`, `http` or `https` URL |
| `items` | ≤ 500; order is the curator's order |
| `items[].uri` | a canonical music URL (ADR 0001): MusicBrainz recording / release / release-group / artist, the ISRC fallback, or a service's canonical track/album/artist URL. Normalized by the writer; `music:` is rejected |
| `items[].added_at` | microseconds |
| `created_at`, `updated_at` | microseconds; `updated_at` bumps on every edit |

Optional fields are omitted when absent (JS style); readers accept `null` too.

## Reference semantics (for Nexus)

Each `items[].uri` is one reference of kind `collection` from the author to that URI, keyed by
the playlist's `object_path`. A `put` replaces the whole item set; a `del` removes it. Nothing in
the body is private; audio never appears here (handoff constraint 2).

## Open

- Unlisted playlists (§8.3 says "public or unlisted"): the homeserver has no unlisted mode; an
  unlisted playlist is simply one the author does not tag or link. To be decided.
- Whether `image` may be an external URL or must be a Pubky file.
