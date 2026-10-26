# One design for phone, tablet and desktop (design note, step 1)

STEWARD-UX-RESPONSIVE.md step 1. **Waiting for the owner's look before anything is built.** The same note as a page, with the wireframes scaled side by side: `responsive/note.html` (published for the owner; link in ~/QUESTIONS.md). The study behind it: `mobile-study.md`.

## Rules
- **Screens don't scroll; lists do.** Now playing, the quality sheet and the zone picker fit the screen. Library, album, artist, playlist and search scroll; their headers fold into a compact bar.
- **Secondary things are sheets.** Queue, Lyrics, Details (signal path, file, credits), Sources (also on) and Tags open over Now playing: a bottom sheet on a phone, a side panel on a tablet, a docked panel on a desktop.
- **Thumbs first.** 44 px targets; transport and the secondary row in the bottom half; swipe a row for Play next, long-press for its menu.
- **Same parts at every width;** only the arrangement and density change. Safe areas, both themes, reduced motion, font scale 1.3.

## Phone (under 600 px)
| Library, mini player, tab bar | Now playing, one screen | A sheet over Now playing | Settings, four groups |
| :-: | :-: | :-: | :-: |
| ![](responsive/phone-library.png) | ![](responsive/phone-now-playing.png) | ![](responsive/phone-queue-sheet.png) | ![](responsive/phone-settings.png) |

- **Tabs:** Home, Search, Library, Zones. The mini player above them carries the quality and the zone.
- **Library:** my library first; shared libraries and Downloads as their own sections.
- **Now playing:** art; title with the heart; the quality label (opens the quality sheet); scrubber; transport. One row opens the five sheets; swipe down collapses it.
- **Settings:** four groups, by where each setting is saved (STEWARD-SETTINGS-GROUPS.md). Each group header says where in one line:
  - **You:** saved on your homeserver, so it follows you to every node and device. My accounts, My library and sharing, Home, layout and appearance, playback quality.
  - **This phone:** only on this device. The USB DAC and bit-perfect, Downloads and cache.
  - **This node (admin):** for everyone on this node. Service apps, People, Library folders, zones and outputs, Remote access and updates.
  - **This homeserver (admin, only when the node runs its own):** sign-ups and invites, users, quotas, storage, domain.
  - Every setting sits in exactly one group. Where a node default and your own choice both exist (playback quality), the node group holds the default and You holds the override; each row says which applies. On tablets and desktops the groups are the left pane.

## Tablet (600–1024 px)
![](responsive/tablet-now-playing.png)

A rail instead of tabs. Now playing in two columns: the player left, the sheet as a side panel right. Lists become two-column grids.

## Desktop (over 1024 px)
![](responsive/desktop-album.png)

- **Arrangement:** a sidebar (with a "Shared with you" group), dense lists with columns (title, artist, quality, time), the queue/lyrics/details panel docked right, and the player bar along the bottom.
- **Now playing:** a full view with the panel beside the art.

## Colour from where it plays (§6)
| Spotify | TIDAL | Qobuz |
| :-: | :-: | :-: |
| ![](responsive/colour-spotify.png) | ![](responsive/colour-tidal.png) | ![](responsive/colour-qobuz.png) |

- **The accent:** while a song plays, the player surfaces take one accent from where it streams: the quality label, the scrubber, the source chip and the play button. Local files and the homeserver keep ours.
- **The cover:** Now playing gets a soft gradient from it.
- **A tint only:** no logos beyond the small source mark, no copied layouts or fonts. Lists keep the app's own theme.
- **Contrast:** each accent is checked in both themes and nudged where it fails: text on the background at least 4.5:1, the button's ink on the accent at least 4.5:1.

  | Source | Light | Dark | Dark ink |
  | :-- | :-- | :-- | :-- |
  | Spotify | `#13803B` | `#1ED760` | yes |
  | TIDAL | `#007C89` | `#33E8E0` | yes |
  | Qobuz | `#1F5BD6` | `#6E9BFF` | yes |
  | ours | `#5b45d6` | `#a596ff` | yes |

  Each source declares its accent next to its features when this is built.
- **Settings › Appearance:** Colour from source (on), Colour from album art (on), or neither. With reduced motion the colour switches without a fade.

## Layout option
Settings › Home and layout:
- **Layout:** Automatic, Compact or Comfortable.
- **Dense lists**, on tablets and desktops.
- **Home rows.**

Per person, saved on their homeserver (STEWARD-MULTI-USER.md).

## Decisions for the owner
1. **The fourth tab:** Zones (drawn), or Queue?
2. **Swipe gestures on rows,** or long-press only?
3. **Lyrics:** in place of the art, or in the sheet (drawn)?

## Build order after the look
Each step is one PR:
1. the shell;
2. Now playing and its sheets;
3. lists;
4. Settings.

Each step gets screenshots at 390, 820 and 1440 px in both themes, and runs the overlap, accessibility and click-every-button checks at all three widths.
