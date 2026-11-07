// Cover links in a zone's queue carry no sign-in: a mixed list played to a zone sends each row's
// cover link as the page gave it (no `token=`), and the browser adds its sign-in only where it
// loads the picture (the dashboard's art). Isolated fixture, no node.
// node tools/ui-queue-covers.js [repo] [shots]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const assert = require('node:assert/strict'); const fs = require('node:fs'); const path = require('node:path');
const { create, go, track, ORIGIN } = require('./ui-fixture');
const shots = path.resolve(process.argv[3] || path.join(process.env.TMPDIR || '.', 'queue-covers-shots'));
const COVER = '/v1/library/albums/7/cover';
const THEIRS = 'https://cdn.example/t.jpg';
const fmt = { kind: 'pcm', rate_hz: 44100, bits: 16, channels: 2 };
// A 1×1 PNG for the cover requests, which are recorded with their query (what the browser loads)
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64');
const covers = async (context) => { const seen = []; await context.route(/\/v1\/library\/albums\/7\/cover/, (r) => { seen.push(new URL(r.request().url()).search); return r.fulfill({ status: 200, contentType: 'image/png', body: PNG }); }); return seen; };
const until = async (f, label) => { const t = Date.now(); while (Date.now() - t < 7000) { const v = await f(); if (v) return v; await new Promise((r) => setTimeout(r, 100)); } throw new Error('Timed out: ' + label); };

(async () => {
  const browser = await chromium.launch();
  try {
    // An album page with a library song and a streamed song: played to a zone, the page sends the
    // rows itself (a mixed list), each with the cover link the page was given
    const song = (no, extra) => ({ no, title: `Song ${no}`, artist: 'Sefa', artist_id: 'lib-11', duration_ms: 200000, uri: null, track_id: null, id: `lib-${no}`, page: `lib-${no}`, ...extra });
    const albumPage = { kind: 'album', id: 'lib-7', uri: null, target: { album_id: 7 }, title: 'Journey', artist: { name: 'Sefa', id: 'lib-11' }, album: null, year: 2020, image: COVER, source: 'library', source_id: '7', plays_from: { service: 'library', label: 'Your library' }, connect: [], reading: false, pending: false,
      songs: [song(1, { track_id: 101, image: COVER }), song(2, { uri: 'https://radio.example/two', image: THEIRS })], total_ms: 400000, albums: [], versions: [], also_on: [], about: [], open_on: [], related: [], concerts: [], about_pending: false, section: 'library' };
    const np = { item: { id: 1, uri: 'track:101', title: 'Song 1', artist: 'Sefa', album: 'Journey', duration_ms: 200000, image: COVER }, index: 0, position_ms: 1000, duration_ms: 200000, source_format: fmt, output_format: fmt, signal_path: { stages: [] } };
    const zone = { id: 'z1', name: 'Kitchen', output: 'null', transport: 'playing', queue: [np.item], queue_index: 0, queue_version: 1, repeat: 'off', radio: false, error: null, acked: 1, sync: 'single', crossfade_ms: 0, volume: null, now_playing: np };
    const fixtures = { '/item/album/lib-7': { body: albumPage }, '/item/album/lib-7/extras': { body: { collections: [], related: [], concerts: [], similar: [] } }, '/library/tracks/101': { body: { ...track, id: 101, title: 'Song 1' } }, '/zones': { body: [zone] }, '/zones/z1': { body: zone }, '/zones/z1/queue': { body: [] }, '/outputs': { body: [{ id: 'null', name: 'Kitchen', kind: 'null', caps: {} }] } };
    const run = await create(browser, 1280, { zone: 'z1', fixtures });
    const { page, state } = run;
    const loaded = await covers(run.context);
    // The event stream's snapshot names the zones; the page keeps the chosen one when it is there
    await run.context.routeWebSocket(/\/v1\/events/, (ws) => { ws.onMessage((m) => { let j; try { j = JSON.parse(m); } catch { return; } if (j.type === 'ping') ws.send(JSON.stringify({ type: 'pong', id: j.id })); }); ws.send(JSON.stringify({ type: 'snapshot', seq: 0, boot: 'fixture-boot', library_rev: 0, zones: [zone] })); });
    await go(page, '/album/lib-7');
    await page.locator('#isongs .srow').first().waitFor();
    await page.locator('#iplay').click();
    const sent = await until(() => state.hits.find((h) => h.m === 'POST' && h.v === '/zones/z1/queue'), 'the list is queued on the zone');
    const body = sent.body || '';
    assert.doesNotMatch(body, /token=/, 'no sign-in in what is sent: ' + body);
    const q = JSON.parse(body).items;
    assert.equal(q.length, 2, body);
    assert.deepEqual([q[0].uri, q[0].image], ['track:101', COVER], 'the library row keeps its cover link as the page gave it');
    assert.deepEqual([q[1].uri, q[1].image], ['https://radio.example/two', THEIRS], 'another site\'s picture is as it was');
    console.log('ok   a mixed list played to a zone sends cover links without a sign-in');
    // Where the browser loads the picture, it signs in
    await go(page, '/now-playing');
    await until(() => loaded.length, 'the dashboard loads the playing song\'s art');
    assert.ok(loaded.every((q) => q === '?token=fixture-owner'), 'the art loads with this browser\'s sign-in: ' + loaded.join(' '));
    console.log('ok   the dashboard loads the cover with the browser\'s sign-in');
    assert.deepEqual(state.errors, [], 'no page errors');
    fs.mkdirSync(shots, { recursive: true });
    await page.screenshot({ path: path.join(shots, 'dashboard.png') });
    await run.context.close();
    // The node drops the sign-in from absolute links on its own address too: the dashboard signs
    // in to such a cover on this page's origin, the same as a relative one
    const absItem = { ...np.item, image: ORIGIN + COVER };
    const absZone = { ...zone, queue: [absItem], now_playing: { ...np, item: absItem } };
    const abs = await create(browser, 1280, { zone: 'z1', fixtures: { ...fixtures, '/zones': { body: [absZone] }, '/zones/z1': { body: absZone } } });
    const absLoaded = await covers(abs.context);
    await abs.context.routeWebSocket(/\/v1\/events/, (ws) => { ws.onMessage((m) => { let j; try { j = JSON.parse(m); } catch { return; } if (j.type === 'ping') ws.send(JSON.stringify({ type: 'pong', id: j.id })); }); ws.send(JSON.stringify({ type: 'snapshot', seq: 0, boot: 'fixture-boot', library_rev: 0, zones: [absZone] })); });
    await go(abs.page, '/now-playing');
    await until(() => absLoaded.length, 'the dashboard loads an absolute cover on the node');
    assert.ok(absLoaded.every((q) => q === '?token=fixture-owner'), 'an absolute cover on the node loads with the sign-in: ' + absLoaded.join(' '));
    console.log('ok   an absolute cover on the node\'s own address loads with the browser\'s sign-in');
    assert.deepEqual(abs.state.errors, [], 'no page errors');
    await abs.context.close();
    // A "This browser" queue saved before (localStorage) whose cover already holds a sign-in:
    // the queue page loads it with one sign-in, not two
    const old = await create(browser, 1280, { fixtures: { '/library/albums': { body: [] } } });
    const oldLoaded = await covers(old.context);
    await old.context.addInitScript((c) => { localStorage.setItem('pm.browserQueue', JSON.stringify([{ id: 1, uri: 'https://radio.example/two', title: 'Old', artist: 'Sefa', image: c + '?%74oken=old-session&TOKEN=older' }])); localStorage.setItem('pm.browserAt', JSON.stringify({ index: 0, pos: 0 })); }, COVER);
    await go(old.page, '/now-playing');
    await until(() => oldLoaded.length, 'the saved queue shows its cover');
    assert.ok(oldLoaded.every((q) => q === '?token=fixture-owner'), 'one sign-in, this browser\'s: ' + oldLoaded.join(' '));
    console.log('ok   a cover saved with an old sign-in loads with this browser\'s only');
    assert.deepEqual(old.state.errors, [], 'no page errors');
    await old.context.close();
    // The same saved queue moved to a zone from a page opened under another of the node's names:
    // the cover goes as the node's own link, relative, without the sign-in it was saved with
    // (an ordinary output: a zone on the developer Null output is listed only while it is current)
    const movedZone = { ...zone, output: 'kitchen', queue: [], now_playing: null, transport: 'stopped' };
    const mv = await create(browser, 1280, { fixtures: { '/library/albums': { body: [] }, '/zones': { body: [movedZone] }, '/zones/z1': { body: movedZone }, '/zones/z1/load': { body: { ...movedZone, queue: [{ id: 9, uri: 'https://radio.example/two', title: 'Old', image: COVER }] } }, '/outputs': { body: [{ id: 'kitchen', name: 'Kitchen', kind: 'alsa', caps: {} }] } } });
    await mv.context.addInitScript((c) => { localStorage.setItem('pm.browserQueue', JSON.stringify([{ id: 1, uri: 'https://radio.example/two', title: 'Old', artist: 'Sefa', image: c + '?token=old-session' }])); localStorage.setItem('pm.browserAt', JSON.stringify({ index: 0, pos: 0 })); }, ORIGIN + COVER);
    await mv.context.routeWebSocket(/\/v1\/events/, (ws) => { ws.onMessage((m) => { let j; try { j = JSON.parse(m); } catch { return; } if (j.type === 'ping') ws.send(JSON.stringify({ type: 'pong', id: j.id })); }); ws.send(JSON.stringify({ type: 'snapshot', seq: 0, boot: 'fixture-boot', library_rev: 0, zones: [movedZone] })); });
    await go(mv.page, '/queue');
    await mv.page.locator('#zonebtn').click();
    await mv.page.locator('[data-move="z1"]').click();
    const moved = await until(() => mv.state.hits.find((h) => h.m === 'POST' && h.v === '/zones/z1/load'), 'the browser queue is moved to the zone');
    const movedItems = JSON.parse(moved.body || '{}').items || [];
    assert.equal(movedItems.length, 1, moved.body);
    assert.equal(movedItems[0].image, COVER, 'the cover goes as the node\'s own relative link, without the saved sign-in: ' + moved.body);
    assert.doesNotMatch(moved.body || '', /token=/, moved.body);
    console.log('ok   a saved queue moved to a zone sends the cover as the node\'s own link, without its sign-in');
    assert.deepEqual(mv.state.errors, [], 'no page errors');
    await mv.context.close();
    console.log('queue covers: PASS');
  } finally { await browser.close(); }
})().catch((e) => { console.error(e); process.exit(1); });
