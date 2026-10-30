# pubky-music-specs

Music on Pubky, on top of the v1 social objects ([pubky-social-specs]). Tags, bookmarks, comments
and playlists are the shared social objects on canonical music URLs; this crate adds only what is
musical: its own post kinds under `/{pub,priv}/music/v1/posts/{id}/{editId}.json`, and the
`ext.music` hints on a playlist's items. The path decides who sees a post: `/priv/` or `/pub/`.

Every music post `embed`s the music it is about and has no `parent`. Times (`*_at`) are Unix
microseconds; durations (`*_ms`) are milliseconds. Unknown members are kept on a rewrite.

| Kind | Content |
| --- | --- |
| `review` | `{rating: 1–10, text?}` |
| `listen` | one play: `{played_at, duration_ms?, source?}` |
| `listening` | now playing: `{started_at, expires_at, name?, duration_ms?, source?}` |
| `session` | a listening session others join: `{started_at, expires_at?, host: "pubky://<key>", name?, source?}` |

`listening` and `session` are ordinary kinds: a reader stops showing one once its `expires_at` has
passed, at most 24 hours after `started_at`.

## Names

A `listening` or a `session` may have a `name`: trimmed, 1 to 100 code points (`LIVE_NAME_MAX_LENGTH`).
Absent is the one spelling of no name; a blank one is refused.

- A client that publishes one without a name from the person writes the generic name:
  `generic_session_name(display name)` is `"<display name>'s listening session"`,
  `generic_listening_name(display name)` is `"<display name> is listening"`.
- A reader shows the same generic name when `name` is absent (`name_or_generic`), so a post reads
  the same whichever client wrote it.
- The generic name is always a valid name: a blank display name reads as `Someone`, and a long one
  is shortened to fit.

```rust
use pubky_music_specs::{generic_session_name, session};

let post = session(
    started_at,
    None,
    format!("pubky://{host}"),
    Some(generic_session_name("Alice")), // "Alice's listening session"
    None,
    song_url,
);
```

## Feeds

A music feed is a saved query for music, at `/{priv,pub}/music/v1/feeds/{id}.json`, modelled on
the social v1 feed. It is not a post: it embeds no music.

| Field | |
| --- | --- |
| `name`, `icon?`, `created_at` | as on a social feed (1–100 characters; a Lucide icon name; Unix µs) |
| `feed.tags`, `feed.match` | 1–5 canonical tags (folded, unique, sorted) or `null`; `any` (default) or `all` of them |
| `feed.reach` | whose tags count: the social feed's `me`, `following`, `followers`, `friends`, `wot`, `all` |
| `feed.sort` | `recent`, `popularity`, `random` |
| `feed.content` | `songs`, `albums`, `artists`, `playlists`, `sessions` |
| `feed.layout` | the social feed's layouts |
| `feed.rules` | up to 50 `{field, op, value, to?}`: `genre`, `artist`, `album`, `year`, `format`, `tag`, `plays`, `last_played`, `bookmarked`; `is`, `contains`, `between`, `at_least`, `at_most`, `within_days`, `not_within_days` |
| `feed.leave_out` | `artists`, `genres`, `uris` (canonical), `played_within_days` |
| `feed.limit` | `songs` (1–1000) and/or `minutes` (1–1440) |
| `feed.seeds` | up to 500 canonical URIs to start from |

Unlike the social feed, its id is a timestamp id, not a hash of the config: a music feed is edited
in place (a smart playlist's rules change). It is private by default; the public copy is the same
bytes at the `/pub/` path. A reader keeps a value it doesn't know (a later reach, sort or rule field)
and leaves that part out; `MusicFeed::validate(id, strict: true)` is what a writer checks.

## Test vectors

`tests/vectors/live-v1.json` holds `listening` and `session` contents: the valid ones with what a
reader shows, the ones refused, and the generic names. `tests/vectors/feed-v1.json` holds music feeds: valid ones, ones only a reader keeps, refused ones,
and the canonical tag form. `cargo test -p pubky-music-specs` checks
this crate against them; another implementation can check itself the same way.

[pubky-social-specs]: https://github.com/pubky/pubky-social-specs
