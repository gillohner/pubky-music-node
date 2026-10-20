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

const NODE = (process.env.PM_NODE || "http://127.0.0.1:7788").replace(/\/+$/, "");
const TOKEN = process.env.PM_TOKEN || "";
const IMPORT = process.argv.includes("--import-playlists");

async function api(method, path, body) {
  const res = await fetch(`${NODE}/v1${path}`, {
    method,
    headers: {
      "content-type": "application/json",
      ...(TOKEN ? { authorization: `Bearer ${TOKEN}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = { error: text };
  }
  if (!res.ok) {
    const err = new Error(`${method} ${path}: ${res.status} ${json?.error ?? text}`);
    err.status = res.status;
    throw err;
  }
  return json;
}

// ---- scrobbling ----

// Roon reports the same now_playing repeatedly; remember what we already
// posted per zone and post once a track has played past the halfway mark
// or 4 minutes, whichever comes first (the usual scrobble rule).
const zoneState = new Map(); // zone_id -> { key, posted, length, seek }

function trackKey(np) {
  const l = np?.three_line || {};
  return [l.line1, l.line2, l.line3].join("\u0001");
}

async function maybeScrobble(zone) {
  const np = zone.now_playing;
  if (!np || zone.state !== "playing") return;
  const key = trackKey(np);
  const st = zoneState.get(zone.zone_id) || { key: null, posted: false };
  if (st.key !== key) {
    zoneState.set(zone.zone_id, { key, posted: false, length: np.length, seek: np.seek_position });
    return;
  }
  st.seek = np.seek_position;
  if (st.posted) return;
  const length = np.length || 0;
  const seek = np.seek_position || 0;
  const threshold = length ? Math.min(length / 2, 240) : 30;
  if (seek < threshold) return;
  st.posted = true;
  const l = np.three_line || {};
  try {
    const r = await api("POST", "/activity", {
      track: { title: l.line1 || "", artist: l.line2 || null, album: l.line3 || null },
      source: "roon",
      duration_ms: length ? length * 1000 : null,
    });
    console.log(`scrobbled: ${l.line1} — ${l.line2} → ${r.uri}`);
  } catch (e) {
    if (e.status === 422) console.log(`skipped (no canonical identity): ${l.line1} — ${l.line2}`);
    else console.error(`scrobble failed: ${e.message}`);
  }
}

// ---- playlist import ----

function browse(browseApi, opts) {
  return new Promise((resolve, reject) =>
    browseApi.browse(opts, (err, r) => (err ? reject(new Error(String(err))) : resolve(r))),
  );
}

function load(browseApi, opts) {
  return new Promise((resolve, reject) =>
    browseApi.load(opts, (err, r) => (err ? reject(new Error(String(err))) : resolve(r))),
  );
}

async function listAll(browseApi, hierarchy, multi_session_key, count) {
  const items = [];
  for (let offset = 0; offset < count; offset += 100) {
    const page = await load(browseApi, { hierarchy, multi_session_key, offset, count: 100 });
    items.push(...(page.items || []));
  }
  return items;
}

async function importPlaylists(browseApi) {
  const key = "pm-import";
  await browse(browseApi, { hierarchy: "playlists", multi_session_key: key, pop_all: true });
  const root = await browse(browseApi, { hierarchy: "playlists", multi_session_key: key });
  const playlists = await listAll(browseApi, "playlists", key, root.list?.count || 0);
  console.log(`found ${playlists.length} Roon playlists`);
  for (const pl of playlists) {
    if (!pl.item_key) continue;
    const r = await browse(browseApi, { hierarchy: "playlists", multi_session_key: key, item_key: pl.item_key });
    const entries = await listAll(browseApi, "playlists", key, r.list?.count || 0);
    // Roon lists a "Play Playlist" action first; tracks are the items with a subtitle.
    const tracks = entries
      .filter((e) => e.subtitle)
      .map((e) => ({ title: e.title, artist: e.subtitle, album: null }));
    await browse(browseApi, { hierarchy: "playlists", multi_session_key: key, pop_levels: 1 });
    if (tracks.length === 0) {
      console.log(`"${pl.title}": no tracks, skipped`);
      continue;
    }
    try {
      const res = await api("POST", "/playlists/import", { name: pl.title, tracks });
      console.log(`"${pl.title}": ${res.imported}/${tracks.length} tracks imported`);
      for (const u of res.unresolved) console.log(`   unresolved: ${u.title} — ${u.artist ?? ""}`);
    } catch (e) {
      console.error(`"${pl.title}": ${e.message}`);
    }
  }
}

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
      for (const z of zones) maybeScrobble(z).catch((e) => console.error(e));
      for (const z of msg?.zones_seek_changed || []) {
        const st = zoneState.get(z.zone_id);
        if (st) {
          // Seek updates carry only position; reuse the last full zone object's identity.
          maybeScrobble({ zone_id: z.zone_id, state: "playing", now_playing: { three_line: keyLines(st.key), length: st.length, seek_position: z.seek_position } }).catch(() => {});
        }
      }
    });
    if (IMPORT) {
      importPlaylists(core.services.RoonApiBrowse)
        .then(() => console.log("playlist import finished"))
        .catch((e) => console.error(`playlist import failed: ${e.message}`));
    }
  },
  core_unpaired(core) {
    console.log(`unpaired from "${core.display_name}"`);
    status.set_status("Waiting for Roon Core", true);
  },
});

function keyLines(key) {
  const [line1, line2, line3] = (key || "").split("\u0001");
  return { line1, line2, line3 };
}

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
