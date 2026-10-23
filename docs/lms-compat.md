# LMS (Lyrion Music Server) compatibility: estimate and recommendation

STEWARD-COMPAT-HA-MA.md §2 asks what a useful subset of the Logitech/Lyrion Music Server control API would take,
so that Home Assistant's `squeezebox` integration and LMS apps can control the node. This is an estimate only;
nothing is built.

**Recommendation: no, not now.** Home Assistant already has two better paths: the Pubky Music integration and
DLNA. A partial LMS emulation would mostly serve third-party LMS apps, and each of those leans on a different
corner of LMS. Revisit only if the owner wants a specific app, such as iPeng, Squeezer or Material.

**One small fix is worth doing now, in mn-infra's Slimproto crate.** Today the node's LMS discovery responder
says it serves LMS JSON-RPC when it doesn't. Details in "What happens today" below.

## What happens today

The node is already a Slimproto **server**: squeezelite and Squeezebox hardware connect to it on TCP 3483, and
its UDP 3483 discovery responder answers their broadcasts. The responder answers with these TLVs:

- `NAME` (the node's name);
- `JSON` (the node's web port);
- `VERS 7.9.0`;
- `UUID`.

`JSON` tells controllers where LMS JSON-RPC lives, but the node has no `/jsonrpc.js`. I checked this with a real
Home Assistant (2026.10.2) against the dev node:

1. Settings › Add integration › *Squeezebox (Lyrion Music Server)* › discovery lists **"Pubky Music Node
   (172.17.0.15)"**.
2. Choosing it fails with **"cannot connect"**.

The same happens in any LMS controller that discovers servers. Squeezelite itself only needs the server's
address, so it doesn't care.

**Suggested fix (mn-infra, `pm-out-slimproto`):** answer `NAME` (and `UUID`) but not `JSON`, while there is no
JSON-RPC. Home Assistant's flow then no longer offers the node, and nothing that works today breaks. I haven't
changed it: the crate is mn-infra's, and the suggestion is in QUESTIONS.md.

## What Home Assistant's `squeezebox` integration needs

It uses `pysqueezebox` over HTTP JSON-RPC: `POST /jsonrpc.js` with `{"method": "slim.request", "params": [player,
[command…]]}`, optional basic auth, polled every few seconds. There is no CLI socket and no Comet.

| Area | Commands | Notes for the node |
|---|---|---|
| Server | `serverstatus - -` (`uuid`, `version`, `mac`, `players_loop`), `players` | a stable `uuid` (the node id); `version` must look like LMS 8+ |
| Player state | `status - 1 tags:acdIKlNorTuxQ` (`mode`, `time`, `power`, `mixer volume`, `playlist_cur_index`, `playlist_tracks`, `playlist_timestamp`, `playlist shuffle`, `playlist repeat`, `sync_master`, `sync_slaves`, `playlist_loop` with `title`, `artist`, `album`, `duration`, `url`, `coverid`, `artwork_url`) | zone state maps well. Player ids must be MAC-like (`aa:bb:…`): derive one from each zone id |
| Transport | `play`, `pause [0\|1]`, `stop`, `power 0\|1`, `time <s>`, `playlist index <n\|+1\|-1>` | direct mapping to `/zones/{id}/transport`; `power` means standby, which few outputs have |
| Volume | `mixer volume <0-100>`, `mixer muting 0\|1` | only zones with hardware volume, as everywhere |
| Queue | `playlist load\|play\|insert\|add <url>`, `playlist clear`, `playlist shuffle <0\|1>`, `playlist repeat <0\|1\|2>`, `playlist save/resume` (announcements) | URLs and the node's URIs; shuffle is a mode in LMS but a one-off on the node |
| Groups | `sync <player>`, `sync -` | maps to group zones, but LMS syncs *players* in place while the node makes a new group zone. That's the awkward part |
| Browse | `albums`, `artists`, `genres`, `playlists`, `titles` (with `tags:jla`/`tags:ju`, `sort:new`, `role_id`), `favorites items … want_url:1`, `search:` | albums, artists, genres, playlists and search map to the library API. `favorites`, `radios` and `apps` would need LMS-style menu trees |
| Images | `/music/<coverid>/cover.jpg`, `/imageproxy/…` | serve album covers under LMS paths |
| Alarms | `alarms`, `alarm add/update/delete`, `playerpref alarmsEnabled` | the integration reads them; returning none is fine |

## Cost

| Piece | Work | Size |
|---|---|---|
| JSON-RPC subset for Home Assistant: the table above without favorites trees or alarms, MAC-like ids, LMS image paths, tests against `pysqueezebox` | 3 to 5 days | about 1,500 lines of Rust plus a CI check with real HA |
| LMS apps (Squeezer, iPeng, Material, Jivelite): the Comet/Bayeux endpoint (`/cometd`) with subscriptions, the CLI on TCP 9090, `menu` and `browselibrary` trees, `favorites` and `radios` menus | plus 1 to 2 weeks | each app trips on different LMS details, and every LMS release can move them |

## Value

- **Home Assistant:** little. The Pubky Music integration already does more than `squeezebox` would get: the
  library and search in the media browser, play-from, push updates, and no polling. DLNA covers people who don't
  install anything.
- **LMS apps:** real for people who live in them, but only with the large second piece. A half-working LMS facade
  is worse than none, because the apps would discover the node and then fail in confusing places.
- **Squeezebox hardware and squeezelite as players:** already works through the Slimproto server. That's the part
  of LMS that matters most, and it's done.

Decision for the owner: keep "no" unless a specific LMS app is wanted. Then build the JSON-RPC subset first, test
it with that app, and only then the Comet/CLI layer it needs.
