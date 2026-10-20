# `pubky.music` objects as v1 `PubkyAppPost`s

Status: implemented in `pm-pubky::schemas::post` (pubky-app-specs 0.8.1). This replaces the
earlier draft of an own `PubkyMusicPlaylist` object type: until the music-specific social
primitives are defined, every music object is a **regular v1 post** whose `content` is a JSON
envelope, kept in the owner's **private** posts folder.

## Post

```json
{
  "content": "{\"type\":\"pubky.music/playlist\",\"v\":1,\"name\":\"Late night\", ...}",
  "kind": "long",
  "parent": null,
  "embed": null,
  "attachments": null
}
```

- `kind` is `long` (the envelope is JSON, not prose; the 50 000-character limit applies).
- `parent` and `attachments` are never set by this node.
- Reviews and activity set `embed: {"kind": "link", "uri": "<canonical music URL>"}` so a generic
  client shows what the post is about. Playlists have no single subject.
- The post passes `PubkyAppPost::validate` unchanged; the node additionally validates the envelope.

## Paths and ids

| Visibility | Path | Who can read |
| :-- | :-- | :-- |
| private (default) | `pubky://<z32>/priv/pubky.app/posts/<id>` | the owner's session only |
| public | `pubky://<z32>/pub/pubky.app/posts/<id>` | everyone; indexed by Nexus like any post |

`id` is a Timestamp ID (microseconds → 8 big-endian bytes → Crockford base32, 13 characters),
derived from the object's `created_at`/`played_at`. Moving an object public is a prefix change.
Tags on music URLs stay plain public `PubkyAppTag`s.

## Envelope

Common fields: `type` (one of the three below) and `v` (currently `1`; readers reject newer).

### `pubky.music/playlist`

```json
{
  "type": "pubky.music/playlist", "v": 1,
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
| `items[].added_at`, `created_at`, `updated_at` | microseconds; `updated_at` bumps on every edit |

### `pubky.music/review`

`target_uri` (canonical music URL), optional `rating` 1–5, `text` ≤ 2000 characters (may be empty
when a `rating` is present; required otherwise), `created_at`. Posts have timestamp ids, so "one
review per target" is a reader rule: the newest wins. The node edits a review in place (same id,
`created_at` kept) and tombstones the post on removal.

### `pubky.music/activity`

`uri`, `played_at` (microseconds, also the id), `source_kind` (`{"kind":"local"}`,
`{"kind":"service","name":"tidal"}`, `{"kind":"roon"}`), optional `duration_ms`.

Optional fields are omitted when absent (JS style); readers accept `null` too.

## Favourites and tags (not posts)

| Object | Type | Path | Visibility |
| :-- | :-- | :-- | :-- |
| favourite ("heart") | `PubkyAppBookmark { uri, created_at }`, id = the spec's hash of `uri` | `/priv/pubky.app/bookmarks/<id>` | private by default (ADR 0010); the public path is the spec's own |
| tag | `PubkyAppTag { uri, label, created_at }`, id = the spec's hash of `uri:label` | `/pub/pubky.app/tags/<id>` | public, written only on explicit user action (ADR 0011) |

Both use the same canonical music URLs as the envelopes above; an object without a MusicBrainz id
(or ISRC fallback) cannot be hearted or tagged.

## For Nexus / Pinakes

Tags are the indexable part today (public, standard `PubkyAppTag`s on canonical URLs). Private posts and bookmarks are invisible to indexers by design. When an object goes
public it is an ordinary post; a `content` starting with `{"type":"pubky.music/` marks it as one
of these envelopes. Playlist items then count as references to their canonical URLs.
