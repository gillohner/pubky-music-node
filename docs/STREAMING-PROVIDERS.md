# Subscription streaming providers: what each one needs

The plug point is `pm_api::streaming::StreamingService` (search, item, stream URL) behind
`/v1/sources/{source}/*`; the Internet Archive and Audius use it today. A subscription service
adds an account flow in Settings (OAuth, tokens encrypted in `data_dir`), canonical-ID mapping
(ISRC/MusicBrainz) for favourites and playlists, and an honest quality ceiling in the signal path.
One account per user, never proxied to others (handoff §2). Reverse-engineered APIs only after
legal review.

| Provider | Credentials to obtain | Catalogue / library | Full-track audio | Notes |
| :-- | :-- | :-- | :-- | :-- |
| TIDAL | App at developer.tidal.com (OAuth 2 + PKCE client id/secret) | **built**: search, albums, tracks via `/v1/sources/tidal`; sign-in from the Sources page | **built**: `trackManifests` with the user's `playback` scope → HLS (best variant, init + segments) into the decoder; DRM refused, previews logged; verified against a mock, pending the first real sign-in | config `[tidal] client_id / client_secret` or `PM_TIDAL_*`; register `<public_url>/tidal/callback` as the app's redirect URI |
| Qobuz | App id + secret by email to api@qobuz.com (state the use case) | yes | yes, FLAC up to 24/192 for approved apps | best fit for bit-perfect playback; do this first |
| Deezer | App at developers.deezer.com | yes | partner-only; otherwise 30 s previews | ISRC available |
| Spotify | App at developer.spotify.com | yes | no raw audio (DRM); Connect / Web Playback only, lossy | browse and playlist source, Connect target |
| Apple Music | Apple developer account, MusicKit token | yes | no raw audio (DRM) | browse and playlist source |

What the owner provides: the credentials above, a subscription on the account used for the one
sign-in (run on the test laptop; the browser redirect lands on the node), and a read of the
provider's developer terms, which go into an ADR before the module is written.

What follows once they arrive: the provider module, "Connect …" in both Settings screens with
token refresh on the node, playlist import by canonical URL, the service in the Sources view.
