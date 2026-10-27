# Pubky Music's own data on a homeserver

Two layers, two namespaces (STEWARD-SPEC-SPLIT.md):

- **The music spec** (`crates/pubky-music-specs`, later its own repository): objects any app can read and
  write, under `/pub/music/v1/` and `/priv/music/v1/`, plus the shared social objects (`/pub|priv/social/v1/`).
  It knows nothing about this app.
- **Pubky Music's app data** (`crates/pm-appdata`, this page): what only this app and its node use, under
  `/priv/pubky.music/`. Every path is a constant in `pm_appdata::paths`. A test fails if one is missing
  here. App data is private: nothing of it is under `/pub/`. (`/pub/pubky.music/tags/` held tags from
  2026-10-21 to 2026-10-26, before they moved to social v1; it is not app data, and the node only empties
  it, below.)

## App paths

| Path | Who writes it | Format | Version |
| :-- | :-- | :-- | :-- |
| `/priv/pubky.music/` | the person's Pubky Music clients | the app's private folder | — |
| `/priv/pubky.music/node.json` | the node | the node record (ADR 0008): `{name, api_url, token, version, updated_at, iroh?}`, so the person's own devices find their node | unversioned; readers ignore unknown members |
| `/priv/pubky.music/settings.json` | the node, after every sync and change | the settings backup: `{version, saved_at, node, contact, folders, settings: {key: value}}`, restored after one Ring sign-in on a new or reinstalled node. Never a sign-in or a token: it is plain JSON, and sign-ins travel in the vault or the encrypted backup file | `version: 1` |
| `/priv/pubky.music/prefs.json` | every node the person uses (`PUT /v1/me/prefs`) | "You" in Settings: `{version, updated_at, home_layout?, quality?, appearance?}`; the newer copy (`updated_at`) wins between nodes | `version: 1` |
| `/priv/pubky.music/files/` | the node (uploads, copies, imports) and the person | the person's own music files, any audio format; the node names them `<16 hex digits of the BLAKE3 hash>-<name>` | — |
| `/priv/pubky.music/files/.pubky-music-layout-check` | the node, once | an empty probe that shows how the homeserver lays out its files | — |
| `/priv/pubky.music/vault/` | every Pubky Music client the person authorised | the sources vault: streaming sign-ins encrypted to each device (`crates/pm-appdata/VAULT.md`) | the index and the sign-ins carry `version: 1` |
| `/priv/pubky.music/vault/index.json` | the same clients | the vault's device list, every entry with a grant proof (VAULT.md) | `version: 1` |
| `/priv/pubky.music/vault/<service>.json` | the same clients | one sign-in's envelope (VAULT.md) | `version: 1` |
| `/priv/pubky.music/vault/<service>.lease.json` | the same clients | a refresh lease, only on homeservers without locks (VAULT.md) | — |
| `/priv/pubky.music/shares.json` | the node, only on its own bundled homeserver (ADR 0016) | the libraries the person shares on the node, so a reinstalled node gets them back. Planned: not written yet | — |

Everything under `/priv/` is readable by the homeserver's operator. The vault is therefore encrypted on the
device. The rest is the person's own data, as [ADR 0016](adr/0016-many-people-one-node.md) says.

### Where tags went
A tag is a social object (ADR 0014), and since 2026-10-26 the node writes it where social v1 puts it:
`/pub/social/v1/tags/<id>.json`. Upstream Nexus #1165 indexes a tag there on an external URL (a
MusicBrainz URL) as a resource tag, and stops indexing `/pub/<app>/tags/`. From 2026-10-21 the node kept
tags at `/pub/pubky.music/tags/`, because the Nexus of the time indexed tags on external links only in an
app's own namespace. The sync moves them once (the `pubky.tags_social_v1` setting) and reads both
folders until then. Nothing else social is kept in app data.

## What the app writes outside its namespace (the specs' paths)

| Path | Objects | Spec |
| :-- | :-- | :-- |
| `/pub|priv/music/v1/posts/` | music `listen` (under the root the listen history setting picks), `listening` and `session`. Older rated reviews (music `review`) were here under `/priv/`; the sync rewrites each as a social `rating` and deletes it | pubky-music-specs |
| `/pub|priv/social/v1/posts/` | collections as `collection` posts with `ext.music` hints; ratings as social `rating`s and comments as `note`s, both with `parent` = the music URL. Always the private draft, and the published copy under `/pub/` while public | pubky-social-specs (ratings: 1.1), pubky-music-specs `ext` |
| `/priv/social/v1/bookmarks/` | bookmarks of music URLs and of public posts (someone's collection) | pubky-social-specs |
| `/pub/social/v1/tags/` | tags on music URLs | pubky-social-specs |
| `/pub/social/v1/follows/` | follows | pubky-social-specs |
| `/pub/pubky.app/follows/` | follows again, the older form, until Nexus indexes v1 follows | pubky-app-specs |
| `/priv/pubky.app/`, `/pub/pubky.app/` | older objects from before ADR 0014, read for the one-time move | pubky-app-specs |
| `/pub/pubky.music/tags/` | tags the node wrote from 2026-10-21 to 2026-10-26, nothing since; the sync moves them to `/pub/social/v1/tags/` once and reads them until then (above) | pubky-app-specs tag |

## What the node asks Ring for
The node's sign-in requests `/priv/pubky.music/vault/:rw` (named on its own: VAULT.md trusts no wider
grant) and the other parts of the private folder it uses (`node.json`, `settings.json`, `prefs.json`,
`files/`), never `/priv/pubky.music/` itself: pubky's canonical form drops a capability that a broader one
covers, so asking for the folder could lose the vault. Sessions approved before with the whole folder
still cover every part. It also asks for `/pub/pubky.music/:rw`, to move the older tags out of it, and for the
social and music folders (`/pub|priv/social/v1/:rw`, `/pub|priv/music/v1/:rw`), and `/priv/pubky.app/:rw`
and `/pub/pubky.app/:rw` for the move from older objects (`pm_pubky::homeserver::LOGIN_CAPABILITIES`). The
desktop app asks Ring only for `/priv/pubky.music/:r`, to read the node record and find the person's node.

## Settings groups
Settings is in four groups, the same in the app, the API and this page (`GET /v1/node` lists them):
- **You** (saved on the person's homeserver, follows them): `/priv/pubky.music/prefs.json` (Home layout,
  quality, appearance) and the sources vault (`/priv/pubky.music/vault/`).
- **This device** (only on that device, never on the node or the homeserver): in the web app's
  localStorage `pm.theme`, `pm.layout`, `pm.dense`, `pm.sidebar`, `pm.zone`, `pm.repeat`, `pm.browserQueue`,
  `pm.browserAt`, `pm.setupHidden`, and `pm.token` (the device's API token: a credential, never in a backup);
  in the apps, the offline store (`pm-offline`), the desktop app's `node.json`, and Android's USB DAC choice.
- **This node** (admin): node settings in the node's database, backed up to `settings.json` above (service
  apps: `GET /v1/node/apps`).
- **This homeserver** (admin, only with a bundled one): the bundled homeserver's own config, through
  `/v1/homeserver/bundled/*`; nothing on the person's homeserver.
