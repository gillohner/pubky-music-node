# One item page (song, album, artist, playlist)

Owner report 2026-10-21: features differed between pages and Back was inconsistent. The rule: **the same
thing gets the same page and the same actions, wherever it came from.** The source (your library, TIDAL,
Deezer, Spotify, a Pubky link) is a fact on the page, not a different page.

Wireframe: `12-item-page.html` (`?kind=song|album|artist|playlist`), shots in `shots/item-page-*.png`.

## Sections, always in this order

A section with nothing to show keeps its heading and says so in one line; it never disappears.

1. **Hero:** art, eyebrow (`Album · plays from TIDAL`), title, artist · year (both links).
   Actions: Play, Shuffle (album, artist, playlist), Play on… (zones), ♥, ⋯.
2. **Songs:** an album's or playlist's tracks; an artist's top songs, then albums as tiles. A song page
   shows the album it is on.
3. **Versions:** the library copy and every "Also on" link, one play button per service it streams from.
4. **On Pubky:** everyone's tags with counts and the number of taggers, scoped 🌐 Everywhere / ▣ This version.
5. **Yours:** my tags, my rating, my notes.
6. **About:** facts and links: MusicBrainz, ISRC/UPC, year, label, and for library files the format, path
   and ReplayGain.

## Feature matrix

Every action is on every kind and every source unless the cell says why not.

| Action | Song | Album | Artist | Playlist |
| :-- | :-- | :-- | :-- | :-- |
| Play | ✓ | ✓ | ✓ (top songs) | ✓ |
| Shuffle | — (one song) | ✓ | ✓ | ✓ |
| Play on… (zone) | ✓ | ✓ | ✓ | ✓ |
| ♥ Favourite | ✓ | ✓ | ✓ | — (playlists are yours already) |
| ⋯ Add to queue / Play next | ✓ | ✓ | ✓ | ✓ |
| ⋯ Add to playlist | ✓ | ✓ | ✓ (top songs) | ✓ (copy into another) |
| ⋯ Go to artist | ✓ | ✓ | — | — |
| ⋯ Go to album | ✓ | — | — | — |
| ⋯ Open on {service} | ✓ | ✓ | ✓ | — |
| ⋯ Copy link | ✓ | ✓ | ✓ | ✓ |
| ⋯ Copy to homeserver | library files | library albums | — | — |
| Songs section | the album it is on | tracks | top songs + albums | items |
| Versions | ✓ | ✓ | services the artist is on | — (one line: playlists are yours) |
| On Pubky | ✓ | ✓ | ✓ | — (one line: not shared until the social primitives land) |
| Yours: tags, rating, notes | ✓ | ✓ | ✓ | notes only (the description) |
| About | ✓ | ✓ | ✓ | ✓ (count, length, updated) |

Sources: library, TIDAL, Deezer, Spotify (plays on a Connect device), Pubky-only link (Open on, previews
when Deezer has it). A source that cannot do something says so in place ("Not on your services · Connect
TIDAL"), it does not remove the section.

The rating is 1–5 stars in this change. The social specs' `review` is 1–10; half stars come with step 3,
when reviews move to that kind (the node stores 1–5 today).

## Rows and tiles

One song row everywhere (album, playlist, search, queue, Discover, favourites, history): title → song page,
artist → artist page (search for the name when there is no artist page yet), album → album page, the same
⋯ menu as the hero. One tile for albums and artists: click opens the item page, hover Play plays it.

## Paths

| Page | Path |
| :-- | :-- |
| Song, album, artist | `/song/<id>`, `/album/<id>`, `/artist/<id>`: `lib-<n>` for library items, `mb-<mbid>` / `isrc-<ISRC>` when the identity is known, else `u-<base64url of the link>` |
| Playlist, genre | `/playlist/<id>`, `/genre/<id>` |
| Source | `/source/<service>` |
| Search, Discover, sections | `/search?q=`, `/discover`, `/albums`, `/artists`, … |

Reload, deep link and sharing land on the same page. A library item that is identified uses its canonical
id, so the same song has the same path on every node.

## Back

- The back arrow is `history.back()`, labelled with the previous page's title ("‹ Results for 'queen'",
  "‹ Radiohead", "‹ OK Computer").
- Opened by deep link or reload (no in-app history), it goes to the page's section and says so ("‹ Albums").
- No fixed parents and no breadcrumbs. The sidebar highlights the page's section: songs and albums →
  Albums, artists → Artists, playlists → Playlists, Pubky-only items → Discover.
- Back restores the scroll position and the search query.
