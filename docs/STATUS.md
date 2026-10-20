# Status against the handoff milestones

Updated 2026-10-20. "Verified" means run against real hardware or a live upstream; "built" means
implemented with automated tests but not yet exercised outside this repository's CI.

| # | Milestone | State | Notes |
| :-- | :-- | :-- | :-- |
| M0 | Workspace, `Output` trait, null/WAV outputs, CI | verified | |
| M1 | Library + local bit-perfect playback | verified | FiiO KA17: 16/44.1, 24/96, 24/192 bit-perfect, `snd-aloop` byte-identical (PR #1). Native DSD silent on that DAC, cause open. |
| M2 | Endpoint daemon, own protocol, gapless, groups | built | `pm-stream`/`pm-endpoint`/`pm-out-net` (ADR 0007), mDNS, loopback test bit-exact. Groups: fan-out with latency pre-roll and drift reporting (no correction). Needs a Pi run and a two-endpoint sync measurement. |
| M3 | Native API + minimal client | built | `/v1` HTTP + WebSocket; a built-in web UI at `/`; `pm-app` (egui) with browse, search, queue, zones, volume, signal path, playlists, discover, DSP. Never run on a display by its author: `docs/LAPTOP-TEST-PROMPT.md`. |
| M4 | OpenSubsonic | built | XML and JSON; ID3 browsing, search, stream with ranges, cover art, playlists, scrobble. Needs a client run (Symfonium, Feishin, Amperfy). |
| M5 | Remote access by pubkey | partial | Node publishes its address privately on the homeserver; app finds it from a session secret (ADR 0008). No Pubky live transport or relay exists yet; wire stays HTTPS. |
| M6 | OpenHome / UPnP (Linn DSM) | built | SSDP discovery, AVTransport (SetAVTransportURI/Play/Stop) and RenderingControl volume; renderers fetch the zone's live WAV pull stream (bit-exact, tested against a fake renderer). Needs a real DSM/WiiM/Bluesound run; OpenHome-specific services (Playlist, Product, standby) not yet. |
| M7 | Slimproto, AirPlay 2, Chromecast | not started | |
| M8 | DSP engine | partial | Sample-rate conversion (`Lossy`, FFT sinc), parametric EQ, convolution from an IR WAV, headphone crossfeed, ReplayGain loudness (all `Enhanced`), headroom, software volume, TPDF dither; per-zone, persisted, editable in the app. Missing: PCM↔DSD, drift correction, selectable SRC filters. Bypass is bit-perfect (tested). |
| M9 | Pubky social | partial | Objects are v1 `PubkyAppPost` envelopes in `/priv/pubky.app/posts/` (ADR 0006); playlists, reviews, activity; private sync with the homeserver; private audio files. Sign-in is by exported session secret; Pubky Ring flow and public publishing wait for the social primitives. |
| M10 | Pinakes integration | partial | Trends, tags, friends' activity joined to the library in the app. Depends on Pinakes' music step (`key=`, `sort=trending`) landing upstream; client falls back meanwhile. |
| M11 | Roon bridge | built, untested | `sidecars/roon-bridge` (official Extension API): scrobble to `/v1/activity`, playlist import via `/v1/playlists/import`. Needs a Roon Core. |
| M12 | Streaming sources | not started | Blocked on service accounts and API terms (handoff §11). The source factory is the plug point. |

## Hard constraints (handoff §2) audit

1. No Roon protocol cloning: the Roon bridge uses only the public Extension API.
2. No public audio: uploads go to `/priv/pubky.music/files/` only (refused elsewhere); `/pub/`
   is never written with audio; track files are served only to authenticated callers.
3. Personal subscription: the node serves one user; no streaming integration exists yet.
4. Canonical IDs: playlists, activity and imports accept only canonical music URLs; library tracks
   without a MusicBrainz id or ISRC are refused from social data with an explanatory error.
5. Bit-perfect by default: every conversion is a named stage with a quality level; defaults add
   none; tests cover bypass, pad, dither, resample and EQ paths.
6. Reverse engineering: none.

## Deployment shapes

Directly on hardware, Docker, Umbrel (`docs/deploy.md`); endpoints on other machines; the native
app on desktops; any OpenSubsonic client on phones.
