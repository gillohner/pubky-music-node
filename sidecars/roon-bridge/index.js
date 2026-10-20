#!/usr/bin/env node
// Roon bridge (handoff §7.4): a Roon Extension built on the official Extension
// API, nothing reverse-engineered. Two jobs:
//
//   1. Scrobble: every track Roon finishes playing is posted to the node as
//      listening activity (POST /v1/activity). The node resolves it to a
//      canonical identity in its own library; tracks it cannot identify are
//      skipped, never guessed.
//   2. Import: `--import-playlists` walks Roon's Playlists through the Browse
//      API and creates them on the node (POST /v1/playlists/import), which
//      reports the tracks it could not resolve.
//
// Configuration (environment):
//   PM_NODE   node origin, default http://127.0.0.1:7788
//   PM_TOKEN  bearer token from the node's [api] token
//
// Roon must authorize the extension once under Settings → Extensions.
// This sidecar has not run against a Roon Core yet; treat the Browse walk
// in particular as a first draft to verify with a real core.

import RoonApi from "node-roon-api";
import RoonApiStatus from "node-roon-api-status";
import RoonApiTransport from "node-roon-api-transport";
import RoonApiBrowse from "node-roon-api-browse";
import { NODE, Scrobbler, importPlaylists, makeApi } from "./lib.js";

const IMPORT = process.argv.includes("--import-playlists");
const api = makeApi();
const scrobbler = new Scrobbler(api);

// ---- extension ----

const roon = new RoonApi({
  extension_id: "org.pubky.music.bridge",
  display_name: "Pubky Music Bridge",
  display_version: "0.1.0",
  publisher: "Pubky Music",
  email: "bridge@pubky.music.invalid",
  website: "https://github.com/gillohner/pubky-music-node",
  core_paired(core) {
    console.log(`paired with Roon Core "${core.display_name}"`);
    status.set_status(`Scrobbling to ${NODE}`, false);
    const transport = core.services.RoonApiTransport;
    transport.subscribe_zones((response, msg) => {
      const zones = [...(msg?.zones || []), ...(msg?.zones_changed || []), ...(msg?.zones_added || [])];
      for (const z of zones) scrobbler.maybeScrobble(z).catch((e) => console.error(e));
      for (const z of msg?.zones_seek_changed || []) {
        const zone = scrobbler.seekUpdate(z);
        if (zone) scrobbler.maybeScrobble(zone).catch(() => {});
      }
    });
    if (IMPORT) {
      importPlaylists(core.services.RoonApiBrowse, api)
        .then(() => console.log("playlist import finished"))
        .catch((e) => console.error(`playlist import failed: ${e.message}`));
    }
  },
  core_unpaired(core) {
    console.log(`unpaired from "${core.display_name}"`);
    status.set_status("Waiting for Roon Core", true);
  },
});

const status = new RoonApiStatus(roon);
roon.init_services({
  required_services: [RoonApiTransport, RoonApiBrowse],
  provided_services: [status],
});
status.set_status("Waiting for Roon Core", true);

api("GET", "/info")
  .then((i) => console.log(`node: ${i.name} v${i.version}, ${i.library.tracks} tracks`))
  .catch((e) => console.error(`cannot reach the node at ${NODE}: ${e.message}`));
roon.start_discovery();
