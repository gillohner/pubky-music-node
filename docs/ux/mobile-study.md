# How music apps lay out a phone, a tablet and a desktop (study, 2026-10-26)

STEWARD-UX-RESPONSIVE.md §1. From using the apps as a person does and from their public pages, as of autumn 2026; no screenshots or assets of theirs are used in our app. Our own wireframes are in `responsive.md`.

## Per app

| | Phone: Now playing | Phone: around it | Quality shown | Tablet and desktop |
| :-- | :-- | :-- | :-- | :-- |
| **Apple Music** | One screen: art, title and artist with a ⋯ menu, scrubber, transport, volume. One bottom row: Lyrics, AirPlay, Queue. Lyrics and the queue replace the art in place; swipe down closes. | Tab bar (Home, New, Radio, Library, Search); a floating mini player above it. | A Lossless or Dolby badge under the scrubber; tap explains the format. | iPad and Mac: a sidebar; the player sits in the top bar; lyrics and queue open as a right panel. |
| **Spotify** | Art, title with Add, scrubber, transport; a row with device, share and queue. Below it the page scrolls into cards (lyrics, the artist, credits, tour dates). | Tab bar (Home, Search, Your Library, plus one); mini player above it with the device picker and a progress line. | Not shown. | Desktop: library sidebar left, content middle, a "Now playing" panel right, player bar at the bottom. |
| **TIDAL** | Art, title, scrubber, transport; lyrics and credits as tabs or a swipe; queue from an icon. | Tab bar; mini player above it. | A quality badge (MAX, High) next to the title; tap for the stream's format. | Desktop: sidebar, player bar at the bottom, queue as a panel. |
| **Qobuz** | Art, title, scrubber, transport; queue as a sheet. | Tab bar; mini player. | Bit depth and sample rate under the title; tap for the quality detail. | Desktop: sidebar and a bottom player. |
| **Plexamp** | Art (or a visualiser) filling the screen, controls under it; swipe up for the queue and lyrics. | Few tabs; a compact mini player. | Optional, in the details. | The same layout widened; the desktop app can be a narrow mini window. |
| **Symfonium** | Art, controls; the queue is a bottom sheet you drag up over it. | Tabs and their order are configurable; a mini player. | Format and bitrate in the details; per-output settings. | Layouts per screen size; a two-pane tablet layout. |
| **HiBy Music** | Art, controls, and the stream's sample rate, bit depth and output mode (USB exclusive) as badges. | Tabs; mini player; DSP (MSEB, EQ) one tap away. | Prominent; the output mode too. | Phone-first; little special for tablets. |

## What recurs, and what we take
- **One screen for Now playing** (all but Spotify, whose long scroll is the counter-example): art, title and artist, the scrubber, transport, and **one row of secondary actions**. **We take it.**
- **Queue and lyrics as sheets or tabs**, replacing the art or sliding over it, never stacked below. **We take it:** Queue, Lyrics, Details, Sources (also on) and Tags are sheets on phones, panels on bigger screens.
- **A persistent mini player above a bottom tab bar**, which expands with a tap or swipe up and collapses with a swipe down. **We take it.**
- **The quality is a label you can tap** (TIDAL, Qobuz, HiBy, Apple's badge). **We have it** (#94): it opens the quality sheet.
- **The output is part of the player** (Spotify's device picker, HiBy's output mode). **We take it:** the zone or output pill sits in the mini player and on Now playing, and the DAC by name (STEWARD-ANDROID-USB-DAC.md).
- **Wider screens rearrange the same parts:** a sidebar or rail instead of tabs, the queue docked right, a player bar at the bottom. **We take it,** with a Layout option (Symfonium shows people want one).
