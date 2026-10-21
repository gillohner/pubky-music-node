# Subscription streaming providers: what each one needs

The plug point is `pm_api::streaming::StreamingService` (search, item, stream URL) behind
`/v1/sources/{source}/*`; the Internet Archive, Audius, TIDAL and the Deezer catalogue use it today. A subscription service
adds an account flow in Settings (OAuth, tokens encrypted in `data_dir`), canonical-ID mapping
(ISRC/MusicBrainz) for favourites and playlists, and an honest quality ceiling in the signal path.
One account per user, never proxied to others (handoff §2). Reverse-engineered APIs only after
legal review.

| Provider | Credentials to obtain | Catalogue / library | Full-track audio | Notes |
| :-- | :-- | :-- | :-- | :-- |
| TIDAL | App at developer.tidal.com (OAuth 2 + PKCE client id/secret) | **built**: search, albums, tracks via `/v1/sources/tidal`; sign-in from the Sources page | **built and verified live** (2026-10-20): manifests → HLS FLAC into the decoder works; on the THIRD_PARTY developer tier TIDAL answers `FULL_REQUIRES_HIGHER_ACCESS_TIER` and serves 30 s previews, so full tracks need TIDAL to raise the app's access tier (request it through the developer portal / tidal-music GitHub discussions, "Open API Access Request") | collection and playlists import as favourites / Pubky playlists by ISRC; config `[tidal] client_id / client_secret` or `PM_TIDAL_*`; register `<public_url>/tidal/callback` as the app's redirect URI |
| Qobuz | App id + secret by email to api@qobuz.com (state the use case) | yes | yes, FLAC up to 24/192 for approved apps | best fit for bit-perfect playback; do this first |
| Deezer | none for the catalogue (**built**: `/v1/sources/deezer`, in the unified search and in Versions by ISRC/UPC); an app at developers.deezer.com only for user library access | yes | partner-only; 30 s previews play today (`deezer:track:<id>`) | ISRC/UPC on every track/album |
| Spotify | App at developer.spotify.com (**built**: `[spotify] client_id / client_secret` or `PM_SPOTIFY_*`; redirect `<public_url>/spotify/callback`; in development mode the owner's account must be added under the app's user management) | **built**: search with ISRC/UPC in the unified search, album/artist pages, Versions by ISRC/UPC, library and playlist import by ISRC | no raw audio (DRM): **built** as "Play on Spotify" on the user's Connect devices (Premium) | one account per node, never proxied |
| Apple Music | Apple developer account, MusicKit token | yes | no raw audio (DRM) | browse and playlist source |
| Jamendo | Free app at devportal.jamendo.com (`[jamendo] client_id / client_secret` or `PM_JAMENDO_*`; redirect `<public_url>/jamendo/callback` for the sign-in) | **built**: search (albums and tracks), album and track pages, in the unified search and in playlist resolution (`jamendo.com/track/<id>` links) | **built**: full-length streams with no account, FLAC (`jamendo:track:<id>`, the stream URL is looked up at play time because it carries the app id; browsers get it relayed by the node) | Creative Commons licences on every track (shown on the item); no ISRC, so Jamendo joins other services only through MusicBrainz or Pinakes links |

Full-length audio is granted to the **application**, never to the person signing in: no identity scheme
(Pubky, Nostr, a different OAuth account) changes the tier TIDAL assigned to the app, and borrowing another
client's identifiers is the reverse-engineered route this project only takes after legal review. The TIDAL
redirect URI follows the address the browser used when `public_url` is unset (reverse proxies: forward
`X-Forwarded-Proto` and `X-Forwarded-Host`).

What the owner provides: the credentials above, a subscription on the account used for the one
sign-in (run on the test laptop; the browser redirect lands on the node), and a read of the
provider's developer terms, which go into an ADR before the module is written.

What follows once they arrive: the provider module, "Connect …" in both Settings screens with
token refresh on the node, playlist import by canonical URL, the service in the Sources view.

| Your server (OpenSubsonic) | The server's address, a user and password (Sources › Your server; kept in the node's database) | **built**: search, albums, songs, playlists (import by MusicBrainz id or ISRC) | **built**: original files into zones (`subsonic:track:<id>`), relayed to browsers | Navidrome, Airsonic, Gonic, Ampache, LMS, another Pubky Music node; token auth on every request |

| Jellyfin / Emby | The server's address and a user (Sources › Jellyfin; signs in once, keeps only the access token) | **built**: search, albums, songs, playlists (import by MusicBrainz recording id) | **built**: original files into zones (`jellyfin:track:<id>`), relayed to browsers | the owner's server is jellyfin.riginode.xyz |

## Not offered, and why (STEWARD-PLAN step 5)

- **Needs approval first:** Qobuz (requested by email), SoundCloud (API access is by application).
- **Plex:** the owner has an account but no Plex server, so there is nothing to stream from; build it when there is one.
- **Apple Music:** needs a paid developer account, and MusicKit plays in the browser only, never on the node's outputs.
- **No official playback API:** YouTube Music, Amazon Music, Bandcamp streaming, Mixcloud. The node's rule is no reverse
  engineering. The `qobuz-api` crate is out for the same reason (it scrapes the web player's credentials) and its GPL-3.0
  licence does not fit the node's MIT.
