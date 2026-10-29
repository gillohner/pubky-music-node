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
| `listening` | someone listening now: `{started_at, expires_at?, paused_at?, ended_at?, name?, duration_ms?, source?}` |

The `session` kind of the earlier drafts is retired: an old one reads as an unknown kind.

## Listening together

One `listening` post per listening session; each song change, pause or resume is a new version of
the same post, never a post per song. `embed` is the song playing now.

- `started_at` is when this song started: the position is now − `started_at`, or
  `paused_at` − `started_at` while paused (`position_ms(now)`).
- `ended_at` is set when the person stops. `expires_at`, optional, is when a reader stops showing
  it without a newer version, at most 24 hours after `started_at`; a writer keeps it a little past the
  song's end. Without it a reader stops a day after the newest version (`is_live(now)`).
- **Listen along:** play the same song from your own source at `position_ms(now)` and follow each
  new version. Comments are social posts with `parent` = the listening post; reactions are social
  tags on it. Suggesting a song is a comment that embeds it.

## Names

A `listening` may have a `name`: trimmed, 1 to 100 code points (`LIVE_NAME_MAX_LENGTH`). Absent is
the one spelling of no name; a blank one is refused.

- A reader shows `generic_listening_name(display name)`, `"<display name> is listening"`, when
  `name` is absent (`name_or_generic`); a client may write the same.
- The generic name is always a valid name: a blank display name reads as `Someone`, and a long one
  is shortened to fit.

```rust
use pubky_music_specs::{listening, listening_post, MusicListeningContent};

// The song that just started; renewed a little past its end
let post = listening(started_at, Some(started_at + 3_600_000_000), None, Some(duration_ms), None, song_url);
// Paused: a new version of the same post
let paused = MusicListeningContent { paused_at: Some(now), ..content };
let post = listening_post(&paused, song_url);
```

## Test vectors

`tests/vectors/live-v1.json` holds `listening` contents: the valid ones with what a reader shows
and the position at a given time, the ones refused (a retired `session` among them), and the
generic names. `cargo test -p pubky-music-specs` checks
this crate against them; another implementation can check itself the same way.

[pubky-social-specs]: https://github.com/pubky/pubky-social-specs
