# Pubky Music's own data on a homeserver

Two layers, two namespaces (STEWARD-SPEC-SPLIT.md):

- **The music spec** (`crates/pubky-music-specs`, later its own repository): objects any app can read and
  write, under `/pub/music/v1/` and `/priv/music/v1/`, plus the shared social objects (`/pub|priv/social/v1/`).
  It knows nothing about this app.
- **Pubky Music's app data** (`crates/pm-appdata`, this page): what only this app and its node use, under
  `/priv/pubky.music/`, and `/pub/pubky.music/` where something must be public. Every path is a
  constant in `pm_appdata::paths`. A test fails if one is missing here.

## App paths

| Path | Who writes it | Format | Version |
| :-- | :-- | :-- | :-- |
| `/priv/pubky.music/` | the person's Pubky Music clients | the app's private folder | — |
| `/priv/pubky.music/node.json` | the node | the node record (ADR 0008): `{name, api_url, token, version, updated_at, iroh?}`, so the person's own devices find their node | unversioned; readers ignore unknown members |
| `/priv/pubky.music/settings.json` | the node, after every sync and change | the settings backup: `{version, saved_at, node, contact, folders, settings: {key: value}}`, restored after one Ring sign-in on a new or reinstalled node | `version: 1` |
| `/priv/pubky.music/files/` | the node (uploads, copies, imports) and the person | the person's own music files, any audio format; names are the node's (`<id>-<name>.<ext>`) | — |
| `/priv/pubky.music/vault/` | every Pubky Music client the person authorised | the sources vault: streaming sign-ins encrypted to each device (`crates/pm-appdata/VAULT.md`) | `version: 1` in each file |
| `/priv/pubky.music/vault/index.json` | the same clients | the vault's device list, every entry with a grant proof (VAULT.md) | `version: 1` |
| `/priv/pubky.music/vault/<service>.json` | the same clients | one sign-in's envelope (VAULT.md) | `version: 1` |
| `/priv/pubky.music/vault/<service>.lease.json` | the same clients | a refresh lease, only on homeservers without locks (VAULT.md) | — |
| `/pub/pubky.music/tags/` | the node | music tags: social `PubkyAppTag`s on canonical music URLs (below) | pubky-app-specs tag |

Everything under `/priv/` is readable by the homeserver's operator. The vault is therefore encrypted on the
device. The rest is the person's own data, as ADR 0016 says.

### Why tags sit in the app's public namespace
A tag is a social object (ADR 0014), and its place is `/pub/social/v1/tags/`. Pubky Music keeps it at
`/pub/pubky.music/tags/` for one reason: the Nexus behind Pinakes indexes tags on external links (a
MusicBrainz URL) only as universal tags in an app's own namespace. Under `pubky.app/tags/` they must
target `pubky://` objects and are dropped, and `social/v1/tags` isn't indexed yet. When it is, the tags
move there (ADR 0014, "Tags stay at /pub/pubky.music/tags/ until…"). Nothing else social is kept in app
data.

## What the app writes outside its namespace (the specs' paths)

| Path | Objects | Spec |
| :-- | :-- | :-- |
| `/pub|priv/music/v1/posts/` | music `review` and `listen` (later `listening`, `session`) | pubky-music-specs |
| `/pub|priv/social/v1/posts/` | playlists as `collection` posts, with `ext.music` hints | pubky-social-specs, pubky-music-specs `ext` |
| `/priv/social/v1/bookmarks/` | favourites | pubky-social-specs |
| `/priv/pubky.app/`, `/pub/pubky.app/` | older objects from before ADR 0014, read for the one-time move | pubky-app-specs |

## What the node asks Ring for
The node's sign-in requests `/priv/pubky.music/vault/:rw` (named on its own: VAULT.md trusts no wider
grant), `/priv/pubky.music/:rw` and `/pub/pubky.music/:rw` for the app data above. It also requests the
social and music folders (`/pub|priv/social/v1/:rw`, `/pub|priv/music/v1/:rw`), and `/priv/pubky.app/:rw`
and `/pub/pubky.app/:rw` for the move from older objects (`pm_pubky::homeserver::LOGIN_CAPABILITIES`).
