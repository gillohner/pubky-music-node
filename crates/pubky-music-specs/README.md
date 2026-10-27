# pubky-music-specs

Music on Pubky, on top of the v1 social objects ([pubky-social-specs]). Tags, bookmarks, ratings,
comments and playlists are the shared social objects on canonical music URLs (a rating is the
social `rating` with `parent` the music URL); this crate adds only what is musical: its own post
kinds under `/{pub,priv}/music/v1/posts/{id}/{editId}.json`, and the `ext.music` hints on a
playlist's items. The path decides who sees a post: `/priv/` or `/pub/`.

Every music post `embed`s the music it is about and has no `parent`. Times (`*_at`) are Unix
microseconds; durations (`*_ms`) are milliseconds. Unknown members are kept on a rewrite.

| Kind | Content |
| --- | --- |
| `listen` | one play: `{played_at, duration_ms?, source?}` |
| `listening` | now playing: `{started_at, expires_at, name?, duration_ms?, source?}` |
| `session` | a listening session others join: `{started_at, expires_at?, host: "pubky://<key>", name?, source?}` |

The `review` kind of the 1.0 drafts is retired: an old one reads as an unknown kind, and
`retired::rating_from_review` rewrites it as a social `rating`.

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

## Test vectors

`tests/vectors/live-v1.json` holds `listening` and `session` contents: the valid ones with what a
reader shows, the ones refused, and the generic names. `cargo test -p pubky-music-specs` checks
this crate against them; another implementation can check itself the same way.

[pubky-social-specs]: https://github.com/pubky/pubky-social-specs
