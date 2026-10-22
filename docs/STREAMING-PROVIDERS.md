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
| TIDAL, full tracks (unofficial) | A TIDAL **device client** id and secret the owner may use, entered in Sources › TIDAL › Full tracks (or `[tidal] unofficial_client_id` / `unofficial_client_secret`, `PM_TIDAL_UNOFFICIAL_CLIENT_*`); a paid TIDAL plan | catalogue and library stay on the official app | **built** (opt-in, off by default): device sign-in at link.tidal.com, then TIDAL's private `playbackinfopostpaywall` (BTS direct FLAC, or DASH segments read as one stream), Max (up to 24/192) or High (16/44.1); encrypted streams are refused, never decrypted | **Risk:** TIDAL has not approved this API for other apps, so it can stop working at any time and TIDAL could act on the account. The node ships no device client: it never uses Music Assistant's bundled ones and never extracts or decodes another project's credentials. A refused client or an unreadable answer switches the mode off, TIDAL falls back to official previews, and the TIDAL tab says why, with Try again |
| YouTube / YouTube Music, official (browser) | nothing; for search, the owner's own free **YouTube Data API key** (Sources › YouTube; checked before it is kept, never shown again) | **built**: search for music videos (category Music, embeddable only) in the YouTube tab and the scoped search, never in the everyday search (each search spends 100 of the key's 10,000 daily units); video pages | **built**: YouTube links on song pages ("Also on", Versions) play in the **This browser** zone through YouTube's IFrame Player API, in a visible player panel driven by the page's transport, media keys and repeat; mixed queues switch between it and the page's audio element | the audio never leaves YouTube's player, so node zones refuse `youtube:video:<id>` ("plays only in This browser"); YouTube's ads and terms apply; videos whose owner turned off embedding say so and the queue moves on. The unofficial yt-dlp mode (for zones) comes later |
| Qobuz | App id + secret by email to api@qobuz.com (state the use case) | yes | yes, FLAC up to 24/192 for approved apps | best fit for bit-perfect playback; do this first |
| Deezer | none for the catalogue (**built**: `/v1/sources/deezer`, in the unified search and in Versions by ISRC/UPC); an app at developers.deezer.com only for user library access | yes | partner-only; 30 s previews play today (`deezer:track:<id>`) | ISRC/UPC on every track/album |
| Spotify | App at developer.spotify.com (**built**: `[spotify] client_id / client_secret` or `PM_SPOTIFY_*`; redirect `<public_url>/spotify/callback`; in development mode the owner's account must be added under the app's user management) | **built**: search with ISRC/UPC in the unified search, album/artist pages, Versions by ISRC/UPC, library and playlist import by ISRC | no raw audio (DRM): **built** as "Play on Spotify" on the user's Connect devices (Premium) | one account per node, never proxied |
| Apple Music | Apple developer account, MusicKit token | yes | no raw audio (DRM) | browse and playlist source |
| Jamendo | Free app at devportal.jamendo.com (`[jamendo] client_id / client_secret` or `PM_JAMENDO_*`; redirect `<public_url>/jamendo/callback` for the sign-in) | **built**: search (albums and tracks), album and track pages, in the unified search and in playlist resolution (`jamendo.com/track/<id>` links) | **built**: full-length streams with no account, FLAC (`jamendo:track:<id>`, the stream URL is looked up at play time because it carries the app id; browsers get it relayed by the node) | Creative Commons licences on every track (shown on the item); no ISRC, so Jamendo joins other services only through MusicBrainz or Pinakes links |

**Modes.** A service can have an Official mode (its public API) and an Unofficial mode (its private API, may break,
off by default, its own warning and sign-in). Sources › TIDAL holds both: the official sign-in and Full tracks
(unofficial). Every source on the Sources page also has an on/off switch: off hides it from search, Home, item pages and
the play order, and keeps its sign-in.

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
| Podcasts (`podcasts`) | none (search through Pinakes, which keeps the Podcast Index key; Apple's iTunes Search API as the fallback, also keyless) | **built**: search, subscribe by search or RSS URL, episodes (`/v1/podcasts/…`); a source in `/v1/sources` | **built**: the episode's audio file into zones; resume where you left off (position recorded while a zone plays it; played past 95 %); newest unplayed across subscriptions for Home (`/v1/podcasts/new`) | feeds refresh every 6 hours; stored on the node |
| Internet radio (`radio`) | none | **built**: radio-browser.info (search by name, browse by country and tag, `/v1/radio/countries`, `/v1/radio/tags`), SomaFM's channels (`channels.json`), Radio Paradise's channels (`/v1/radio/curated`); a source in `/v1/sources` (search, station pages) | **built**: the stream URL into zones; Radio Paradise in lossless FLAC (5 of 6 channels), SomaFM up to 256 kbps MP3; in-stream titles as now playing | channel lists follow Music Assistant's SomaFM and Radio Paradise providers (Apache-2.0); stations saved on the node (`/v1/radio/stations`) |

| Plex | "Sign in with Plex" (a plex.tv PIN; the node finds your server) or the server's address and a token (Sources › Plex; keeps only the server's token) | **built**: search, albums, songs | **built**: original files into zones (`plex:track:<ratingKey>`), relayed to browsers | CI runs Plex's own Linux server (`tools/plex-source.py`); playlists later (Plex rarely carries MusicBrainz recording ids) |

| Audiobookshelf | An API key (Settings › API Keys), or an admin's user and password once: the node makes itself a key and keeps only that | **built**: audiobooks from every book library, by chapter; "continue listening" | **built**: original files into zones (`audiobookshelf:file:<item>/<ino>`), resumed from and reported back to Audiobookshelf's position, so a phone picks up where a zone left off | CI runs Audiobookshelf itself (`tools/abs-source.py`); its podcast libraries later (the node has its own podcasts) |

## Not offered, and why (STEWARD-PLAN step 5)

- **Needs approval first:** Qobuz (requested by email), SoundCloud (API access is by application).
- **Apple Music:** needs a paid developer account, and MusicKit plays in the browser only, never on the node's outputs.
- **No official playback API:** YouTube Music (beyond the embedded player above), Amazon Music, Bandcamp streaming, Mixcloud. The node's rule is no reverse
  engineering. The `qobuz-api` crate is out for the same reason (it scrapes the web player's credentials) and its GPL-3.0
  licence does not fit the node's MIT.
