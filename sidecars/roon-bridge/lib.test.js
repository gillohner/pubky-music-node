import test from "node:test";
import assert from "node:assert/strict";
import { Scrobbler, importPlaylists, makeApi, trackKey, keyLines } from "./lib.js";

const quiet = { log() {}, error() {} };

function fakeFetch(handler) {
  const calls = [];
  const fetchImpl = async (url, init) => {
    const body = init.body ? JSON.parse(init.body) : undefined;
    calls.push({ url, method: init.method, body, auth: init.headers.authorization });
    const [status, json] = handler(url, init.method, body);
    return { ok: status < 400, status, text: async () => JSON.stringify(json) };
  };
  return { fetchImpl, calls };
}

test("api adds the token and surfaces node errors with their status", async () => {
  const { fetchImpl, calls } = fakeFetch((url) => (url.endsWith("/v1/info") ? [200, { name: "n" }] : [422, { error: "no canonical identity" }]));
  const api = makeApi({ node: "http://node:7788/", token: "t0k", fetchImpl });
  assert.deepEqual(await api("GET", "/info"), { name: "n" });
  assert.equal(calls[0].url, "http://node:7788/v1/info");
  assert.equal(calls[0].auth, "Bearer t0k");
  await assert.rejects(api("POST", "/activity", {}), (e) => e.status === 422 && /no canonical identity/.test(e.message));
});

test("scrobbles once per track after the halfway mark, keyed per zone", async () => {
  const { fetchImpl, calls } = fakeFetch(() => [200, { uri: "https://musicbrainz.org/recording/x" }]);
  const s = new Scrobbler(makeApi({ fetchImpl }), quiet);
  const np = (seek) => ({ three_line: { line1: "Get Lucky", line2: "Daft Punk", line3: "RAM" }, length: 369, seek_position: seek });
  const zone = (seek, state = "playing") => ({ zone_id: "z1", state, now_playing: np(seek) });
  assert.equal(await s.maybeScrobble(zone(0)), false, "first sight only remembers the track");
  assert.equal(await s.maybeScrobble(zone(100)), false, "before the threshold");
  assert.equal(await s.maybeScrobble(zone(185)), true, "past half of 369 s");
  assert.equal(await s.maybeScrobble(zone(300)), false, "posted once");
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].body, { track: { title: "Get Lucky", artist: "Daft Punk", album: "RAM" }, source: "roon", duration_ms: 369000 });
  // Paused zones never scrobble; a seek-only update reuses the remembered identity.
  assert.equal(await s.maybeScrobble(zone(200, "paused")), false);
  const next = { zone_id: "z1", state: "playing", now_playing: { three_line: { line1: "Instant Crush", line2: "Daft Punk", line3: "RAM" }, length: 1200, seek_position: 10 } };
  await s.maybeScrobble(next);
  const rebuilt = s.seekUpdate({ zone_id: "z1", seek_position: 250 });
  assert.equal(rebuilt.now_playing.three_line.line1, "Instant Crush");
  assert.equal(await s.maybeScrobble(rebuilt), true, "4 minutes beats half of a 20-minute track");
  assert.equal(calls.length, 2);
  assert.equal(s.seekUpdate({ zone_id: "unknown", seek_position: 1 }), null);
});

test("a 422 from the node is a skip, not a failure", async () => {
  const { fetchImpl } = fakeFetch(() => [422, { error: "no canonical identity" }]);
  const s = new Scrobbler(makeApi({ fetchImpl }), quiet);
  const z = { zone_id: "z", state: "playing", now_playing: { three_line: { line1: "Mystery", line2: "Nobody", line3: "" }, length: 100, seek_position: 60 } };
  await s.maybeScrobble(z);
  assert.equal(await s.maybeScrobble(z), false);
});

test("playlist import walks the Browse hierarchy and posts described tracks", async () => {
  // A fake Roon Browse API: root → two playlists; each playlist lists a
  // "Play Playlist" action without subtitle, then tracks with subtitles.
  const lists = {
    root: [{ title: "Road trip", item_key: "pl1" }, { title: "Empty", item_key: "pl2" }, { title: "No key" }],
    pl1: [{ title: "Play Playlist" }, { title: "Get Lucky", subtitle: "Daft Punk" }, { title: "Mystery Song", subtitle: "Nobody" }],
    pl2: [{ title: "Play Playlist" }],
  };
  let current = "root";
  const browseApi = {
    browse(opts, cb) {
      if (opts.pop_all || opts.pop_levels) current = "root";
      if (opts.item_key) current = opts.item_key;
      cb(null, { list: { count: lists[current].length } });
    },
    load(opts, cb) {
      cb(null, { items: lists[current].slice(opts.offset, opts.offset + opts.count) });
    },
  };
  const { fetchImpl, calls } = fakeFetch((url, method, body) => [200, { id: "p", name: body.name, parsed: body.tracks.length, imported: 1, unresolved: body.tracks.slice(1) }]);
  const report = await importPlaylists(browseApi, makeApi({ fetchImpl }), quiet);
  assert.equal(calls.length, 1, "only the playlist with tracks is imported");
  assert.equal(calls[0].body.name, "Road trip");
  assert.deepEqual(calls[0].body.tracks, [
    { title: "Get Lucky", artist: "Daft Punk", album: null },
    { title: "Mystery Song", artist: "Nobody", album: null },
  ]);
  assert.equal(report.length, 2);
  assert.equal(report[0].result.imported, 1);
  assert.equal(report[1].skipped, true);
});

test("track keys round-trip", () => {
  const np = { three_line: { line1: "a", line2: "b", line3: "c" } };
  assert.deepEqual(keyLines(trackKey(np)), np.three_line);
});
