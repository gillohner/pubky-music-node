// The bridge's logic without the Roon wiring, so it can be tested with
// fake zones, a fake Browse API and a fake (or real) node.

export const NODE = (process.env.PM_NODE || "http://127.0.0.1:7788").replace(/\/+$/, "");
export const TOKEN = process.env.PM_TOKEN || "";

/** Call the node's /v1 API. `fetchImpl` is swappable for tests. */
export function makeApi({ node = NODE, token = TOKEN, fetchImpl = fetch } = {}) {
  const base = String(node).replace(/\/+$/, "");
  return async function api(method, path, body) {
    const res = await fetchImpl(`${base}/v1${path}`, {
      method,
      headers: {
        "content-type": "application/json",
        ...(token ? { authorization: `Bearer ${token}` } : {}),
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
  };
}

// ---- scrobbling ----

export function trackKey(np) {
  const l = np?.three_line || {};
  return [l.line1, l.line2, l.line3].join("\u0001");
}

export function keyLines(key) {
  const [line1, line2, line3] = (key || "").split("\u0001");
  return { line1, line2, line3 };
}

/**
 * Roon reports the same now_playing repeatedly; remember what we already
 * posted per zone and post once a track has played past the halfway mark
 * or 4 minutes, whichever comes first (the usual scrobble rule).
 */
export class Scrobbler {
  constructor(api, log = console) {
    this.api = api;
    this.log = log;
    this.zoneState = new Map(); // zone_id -> { key, posted, length, seek }
  }

  /** A seek-only update: rebuild a zone object from what we remember. */
  seekUpdate(z) {
    const st = this.zoneState.get(z.zone_id);
    if (!st) return null;
    return { zone_id: z.zone_id, state: "playing", now_playing: { three_line: keyLines(st.key), length: st.length, seek_position: z.seek_position } };
  }

  async maybeScrobble(zone) {
    const np = zone.now_playing;
    if (!np || zone.state !== "playing") return false;
    const key = trackKey(np);
    const st = this.zoneState.get(zone.zone_id) || { key: null, posted: false };
    if (st.key !== key) {
      this.zoneState.set(zone.zone_id, { key, posted: false, length: np.length, seek: np.seek_position });
      return false;
    }
    st.seek = np.seek_position;
    if (st.posted) return false;
    const length = np.length || 0;
    const seek = np.seek_position || 0;
    const threshold = length ? Math.min(length / 2, 240) : 30;
    if (seek < threshold) return false;
    st.posted = true;
    const l = np.three_line || {};
    try {
      const r = await this.api("POST", "/activity", {
        track: { title: l.line1 || "", artist: l.line2 || null, album: l.line3 || null },
        source: "roon",
        duration_ms: length ? length * 1000 : null,
      });
      this.log.log(`scrobbled: ${l.line1} — ${l.line2} → ${r.uri}`);
      return true;
    } catch (e) {
      if (e.status === 422) this.log.log(`skipped (no canonical identity): ${l.line1} — ${l.line2}`);
      else this.log.error(`scrobble failed: ${e.message}`);
      return false;
    }
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

/**
 * Walk Roon's Playlists hierarchy and import each one on the node.
 * Returns `[{ name, tracks, result | error }]` for reporting and tests.
 */
export async function importPlaylists(browseApi, api, log = console) {
  const key = "pm-import";
  const report = [];
  await browse(browseApi, { hierarchy: "playlists", multi_session_key: key, pop_all: true });
  const root = await browse(browseApi, { hierarchy: "playlists", multi_session_key: key });
  const playlists = await listAll(browseApi, "playlists", key, root.list?.count || 0);
  log.log(`found ${playlists.length} Roon playlists`);
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
      log.log(`"${pl.title}": no tracks, skipped`);
      report.push({ name: pl.title, tracks, skipped: true });
      continue;
    }
    try {
      const res = await api("POST", "/playlists/import", { name: pl.title, tracks });
      log.log(`"${pl.title}": ${res.imported}/${tracks.length} tracks imported`);
      for (const u of res.unresolved) log.log(`   unresolved: ${u.title} — ${u.artist ?? ""}`);
      report.push({ name: pl.title, tracks, result: res });
    } catch (e) {
      log.error(`"${pl.title}": ${e.message}`);
      report.push({ name: pl.title, tracks, error: e.message });
    }
  }
  return report;
}
