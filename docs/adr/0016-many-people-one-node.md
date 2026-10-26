# ADR 0016: One node, many people: a library each, shared with permissions; node apps and personal sign-ins apart

Status: **accepted** (2026-10-26, STEWARD-MULTI-USER.md step 1; mn-infra with mn-app). The owner answered
the open questions (STEWARD-OWNER-ANSWERS-1026.md); the answers are written into the decision below and
listed at the end.

## Context

Today a node has one library and one person:
- **The library** is one SQLite database (ADR 0002). Tracks are keyed by file path, from local folders and
  mounts, WebDAV shares, Drive/OneDrive folders, and the owner's homeserver files: read directly on the
  bundled homeserver, or mirrored into `mirror_dir`.
- **The owner's data** (favourites, tags, plays, playlists, reviews) lives in the same database, as a cache
  of what the node writes to the owner's homeserver through the owner's one Pubky session (`PubkySync`).
- **Others** are *listeners* (#46): a personal token, read access to the owner's library, play on the zones
  the owner opens. They have no library, data or interface of their own.
- **Credentials** mix two kinds in one place:
  - the node's apps: Spotify client id, Qobuz app, SoundCloud client, YouTube and Last.fm keys;
  - the owner's personal sign-ins: TIDAL, Spotify, Qobuz session, Last.fm session.

  The sources vault (STEWARD-VAULT.md) is moving the personal ones onto the person's account.

The owner wants every person on a node to have their own library, data and interface, all files on
homeservers (remote ones allowed, with a quota warning), libraries shared with permissions in both
directions, and the node's apps set once by the admin.

## Decision

### 1. Who and what

| Thing | What it is | Where it lives |
| :-- | :-- | :-- |
| **Person** | a Pubky key on this node; `admin` (the node's owner, one) or `member` | node DB `people` (today's roster, extended) |
| **Library** | a set of files with one owner. `person:<z32>`: their files on their homeserver (`/priv/pubky.music/files/`). `local`: the admin library (folders, mounts, WebDAV, drives; the owner's NAS) | node DB `libraries` |
| **Share** | `(library, person, level)`, level `listen` or `listen+download`; revocable, never implied | node DB `shares`, the node's own record. **Also on the sharer's homeserver** (`/priv/pubky.music/shares.json`) **only when that is the node's own bundled homeserver**, so a reinstalled node gets them back. For a person on a remote homeserver the node keeps them locally and never writes them there |
| **Grant** | the Ring session a person gives the node: read their files, write their own data | node data dir, one file per person, 0600; deleted when they leave |
| **Person's data** | favourites, tags, plays, playlists, reviews, interface settings | on **their** homeserver (ADR 0014 paths); the node DB caches them per person |

- **The admin has no special read right.** They see a member's library only if the member shares it. The
  admin does keep the node's controls: people, zones, node apps, and removing a person.
- **Every person, the admin included, has an own library** (it may be empty), any libraries shared with
  them, and the admin library if it was shared with them. Nobody gets the admin library by default.

### 2. How the node reads each person's files
- **On the node's bundled homeserver:** straight from its files folder (`<files>/<z32>/…`), as today.
- **On a remote homeserver:**
  - When the person joins, they approve the node in Ring with `/priv/pubky.music/files/:r`. That is read
    only, and narrower than the brief's `/priv/pubky.music/:r`, which would also cover the vault and history.
    Their data capabilities come with it (the private folder's parts, without the vault, and the
    social/music folders, as LOGIN_CAPABILITIES lists them; since #117 the private folder is asked for
    by its parts, so that normalising the set can't drop the vault). A new part, such as `shares.json`
    below, has to be added to LOGIN_CAPABILITIES, and sessions approved before are then asked again.
  - The node lists the files through that session, reads tags with ranged GETs (FLAC and ID3 metadata sit at
    the start; MP4 may need the end too), and fetches the audio when it plays or a device downloads (#97).
  - It keeps a capped cache, not a master copy.
  - **The node never uploads or writes files to a remote homeserver.** Its session there is read only.
    A person adding music to their own remote homeserver does it from their own client, with their own
    session. On the node's bundled homeserver the node may write for them (uploads, "Copy to my
    homeserver").
  - When the session is refused (revoked in Ring, or the person removed), the library shows "lost access".
    Nothing is deleted.
- **Quota:**
  - Homeserver 0.16.2 shows a session neither its quota nor its usage. The admin API has
    `/users/{z32}/quota`, so the node knows both on its own bundled homeserver. On a remote one:
    "quota unknown", with the used size from the file listing and a pointer to that homeserver's operator.
  - Uploads warn before they would pass a known quota. A homeserver that refuses an upload answers `507`,
    and the node says "this homeserver is full".

### 3. The scan index
- `tracks` gains `library` (the library id). A file is identified by `(library, location)`:
  - `location` is the path for local files;
  - `pubky://<z32>/priv/pubky.music/files/…` for homeserver files, the same whether the node reads them
    directly or through a session, so moving between the two keeps the rows.
- The same song in two libraries is two rows. They meet through the canonical URI (ADR 0001:
  MusicBrainz, then ISRC, then a service URL).
- **Per-person data is keyed on canonical URIs and the person**, never on file rows:
  - `favourites`, `tags`, `plays` and `music_posts` gain a `person` column;
  - my tag on Anna's song is mine, written to my homeserver, and points at the song, not at Anna's file.
  - Songs without a canonical id keep today's node-only fallback, now per person and per library.
- **Visibility is checked on every read:** a caller sees their own library, plus the libraries shared with
  them (as separate sections, never merged). Search, Home rows and item pages filter the same way.
  Downloads need `listen+download` on the file's library; streaming needs `listen`. Zones stay per
  person, as People › Zones today.

### 4. Node apps and personal sign-ins
- **Node apps** (Settings › Node › Service apps, admin only), one screen, each with "where to get this" and
  a test button:
  - Spotify client id (with the 25-users-in-development-mode note), Qobuz app id and secret, SoundCloud
    client id;
  - the YouTube Data key, the Last.fm key and secret, Deezer and Jamendo apps, the fanart.tv key, and the
    node's own Google and OneDrive apps.

  Members never see them. They stay node settings, as most already are. **The vault doesn't carry them:**
  #96 currently bridges `spotify.app`, `qobuz.app`, `soundcloud.client`, `lastfm.app` and
  `tidal.unofficial_client` along with the sessions, and those come out of its bridges before it merges.
- **Personal sign-ins** (Settings › My accounts): TIDAL, Spotify, Qobuz session, Last.fm session, Bandcamp
  cookie, Deezer, Plex, Jellyfin, Subsonic. Each lives in that person's vault and the node's cache *for
  that person*. Nobody else on the node can use them. A zone playing my TIDAL uses my token, held in
  memory for that play.

### 5. Migration from today (once, nothing deleted)
- **Today's library becomes two:**
  - the local folders, mounts, shares and drives become the `local` admin library;
  - the owner's homeserver files become `person:<owner>`;
  - `mirror_dir` stays a cache of the owner's files.
- **Every existing row** gets its `library`, and the owner's favourites, tags, plays and playlists get
  `person = owner`.
- **Each listener** in the roster becomes a member with an empty library, and the admin library shared
  `listen+download`, so they keep download and offline rights (the owner's answer). Their tokens keep
  working. Their zones carry over.
- **Interface settings** (Home rows, layout, quality) become the owner's, and are written to the owner's
  homeserver `/priv/pubky.music/settings.json`.
- **App credentials** stay node settings and move to the admin screen. The owner's personal sign-ins move
  to the owner's vault (#96).
- The API keeps today's shapes. A request without a person context answers as the caller, and an old
  client sees its own library plus its shares as before.

## Consequences
- **The node holds every member's grant session** (to read their files, and write their data when they act
  through the node). It is scoped to the folders above, stored 0600, and deleted when they leave. Revoking
  it in Ring cuts the node off. This is the main trust a member gives a node, and the join screen should say
  so in one line.
- **Remote libraries cost bandwidth:** a first scan reads each file's metadata, and playing fetches the
  audio. The cache cap and a per-person scan schedule keep it bounded. Large remote libraries are slower to
  appear than local ones.
- **The listener role goes away.** Code that checks `Role::Listener` becomes "a person, with these
  shares".
- **When a person leaves, the node deletes their cached data at once:** their rows, tags, plays,
  playlists, interface settings, shares and grant session. There is no grace period. Their homeserver
  copy is theirs.
- **The interface becomes per person,** stored on each person's homeserver, so it follows them to another
  node.

## The owner's answers (2026-10-26)
1. Existing listeners keep download and offline rights on the owner's library (`listen+download`).
2. Shares live on the sharer's homeserver when it is the node's own bundled one. For a remote homeserver
   the node keeps them locally, as a cache it owns, and doesn't write them there.
3. When someone leaves, their cached data goes at once.
4. A person on a remote homeserver gives the node a read-only session on their files folder. The node never
   uploads or writes files there.

## Build order (STEWARD-MULTI-USER.md §4)
1. This ADR.
2. Node apps vs personal sign-ins (with mn-sources' chooser).
3. Per-person libraries and the remote read path, with the quota warning.
4. Shares and permissions, and the shared sections in the UI (mn-app).
5. Per-person interface settings.

Tests throughout: two people on the bundled homeserver and one on a remote one; share, revoke, tag the same
song separately, and a member with an empty library.
