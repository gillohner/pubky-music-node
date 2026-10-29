// Setting a node up in the web page, against a node with a config folder and a config token (CI's):
//   * Home's Get started shows the steps the node names (no music step: the config has a folder),
//     goes when hidden, stays hidden in a new browser (kept on the node), comes back from
//     Settings › Home for everyone, and goes by itself when only optional steps are left;
//   * Library folders marks the config's folder and offers no Remove for it;
//   * Group outputs offers the outputs that group in sync (Sonos, HEOS, BluOS, squeezelite, the
//     app's players on a DAC), by
//     kind, one kind at a time, says what a pick becomes, and doesn't offer one that shares no
//     sample rate with the ones picked;
//   * About: "Use a different token in this browser", the node's token set in the config, or
//     replaced and shown once; the login dialog says where the token is;
//   * covers carry a provider badge (where it plays from: the library or the service) on tiles,
//     song rows, the album page and Now playing, in the corner, named, in the service's colour.
//   * Play to: while something plays or is queued, every other device gets Move here (This zone
//     gets Move to…): the song at its position, the queue and repeat move, paused stays paused,
//     out of and into This browser too, and the menu follows; the node's errors show in the menu;
//     the device line says the protocol and model in words, with the id as a tooltip;
//   * the sound pill in the player bar says where it plays from (the provider's mark), the path in
//     short and whether DSP is on (on phones: the device and a quality badge), and opens a panel
//     with the zone and device, the source, the signal path, this zone's DSP with quick switches
//     (and a link to all of it) and volume;
//   * pressing play answers at once: the pressed button spins and the bar says "Starting on
//     <device>…" until it plays, or shows the zone's error; Move to says "Moving to …" too.
// The rules behind the steps, the folders and the groups are pm-api's tests/node_setup.rs.
//   node tools/ui-setup.js <api token> [http://127.0.0.1:7788]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || ''; const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const problems = [];
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) problems.push(what); };
const api = async (method, path, body) => { const r = await fetch(BASE + '/v1' + path, { method, headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined }); if (!r.ok) throw new Error(`${method} ${path}: ${r.status}`); return r.status === 204 ? null : r.json(); };
const visible = (p, sel, ms = 8000) => p.locator(sel).first().waitFor({ state: 'visible', timeout: ms }).then(() => true).catch(() => false);
// Waits until the element's text is `text` (polling: the CSP forbids waitForFunction's eval).
const waitText = async (p, sel, text, ms = 10000) => { const end = Date.now() + ms; for (;;) { const v = await p.textContent(sel).catch(() => null); if (v != null && v.trim() === text) return true; if (Date.now() > end) return false; await p.waitForTimeout(150); } };
const gone = (p, sel, ms = 8000) => p.locator(sel).first().waitFor({ state: 'detached', timeout: ms }).then(() => true).catch(() => false);
const rate = (r) => ({ pcm_rates: r, max_pcm_bits: 24, max_channels: 2, dsd: null, hw_volume: null, standby: false, source_switch: false, gapless: true, groupable: false, fixed_latency_ms: null, quality_ceiling: 'bit_perfect' });
const OUTS = [
  { id: 'upnp:RINCON_A', name: 'Sonos Kitchen', protocol: 'upnp', caps: rate([44100, 48000]), group_by: 'sonos' },
  { id: 'upnp:RINCON_B', name: 'Sonos Living', protocol: 'upnp', caps: rate([44100, 48000]), group_by: 'sonos' },
  { id: 'bluos:aa01', name: 'Bluesound Study', protocol: 'bluos', caps: rate([44100, 48000, 96000]), group_by: 'bluos' },
  { id: 'slim:00:04:20:00:00:01', name: 'Pi Bedroom', protocol: 'slim', caps: { ...rate([44100, 96000]), groupable: true } },
  { id: 'slim:00:04:20:00:00:02', name: 'Pi Hall', protocol: 'slim', caps: { ...rate([44100]), groupable: true } },
  { id: 'slim:00:04:20:00:00:03', name: 'Old Pi', protocol: 'slim', caps: { ...rate([8000]), groupable: true } },
  { id: 'net:192.168.1.20:7790/alsa:hw:1,0', name: 'Desk · USB DAC', protocol: 'net', caps: { ...rate([44100, 96000]), groupable: true } },
  { id: 'net:192.168.1.21:7790/alsa:hw:0,0', name: 'Studio · DAC', protocol: 'net', caps: { ...rate([44100]), groupable: true } },
  { id: 'net:192.168.1.22:7790/cpal:Speakers', name: 'Mac · Speakers', protocol: 'net', caps: rate([44100, 48000]) },
  { id: 'upnp:streamer', name: 'Streamer', protocol: 'upnp', caps: rate([44100, 48000, 192000]) },
  { id: 'airplay:phone', name: 'Old phone', protocol: 'airplay', caps: rate([44100]) },
];
const GROUPABLE = OUTS.filter((o) => o.group_by || o.caps.groupable).map((o) => o.id);

(async () => {
  const browser = await chromium.launch();
  const ctx = async () => { const c = await browser.newContext({ viewport: { width: 1280, height: 900 } }); await c.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN); return c; };
  const before = await api('GET', '/node/settings');
  try {
    // ---- Get started ----
    let c = await ctx(); let p = await c.newPage();
    const real = await api('GET', '/me/first-steps');
    await p.goto(BASE + '/');
    if (real.show) {
      check(await visible(p, '#h-setup .setup-card'), 'Home shows Get started');
      const shown = await p.$$eval('#h-setup [data-step]', (els) => els.map((e) => e.dataset.step));
      check(JSON.stringify(shown) === JSON.stringify(real.steps.map((x) => x.id)), `the steps the node names (${shown.join(', ')})`);
      check(!shown.includes('music'), 'no "Add your music": the config has a folder');
      await p.click('#setuphide');
      check(await gone(p, '#h-setup .setup-card'), 'Hide these steps hides them');
      check((await api('GET', '/node/settings')).first_steps_hidden === true, 'kept on the node');
      await c.close(); c = await ctx(); p = await c.newPage();
      await p.goto(BASE + '/'); await p.waitForSelector('#h-setup', { state: 'attached' }); await p.waitForTimeout(1500);
      check(!(await p.$('#h-setup .setup-card')), 'still hidden in another browser');
      await p.goto(BASE + '/settings/nodehome');
      await p.check('#firststeps');
      await p.waitForTimeout(500);
      check((await api('GET', '/node/settings')).first_steps_hidden === false, 'Settings › Home for everyone shows them again');
      await p.goto(BASE + '/');
      check(await visible(p, '#h-setup .setup-card'), 'and Home has them again');
    } else check(false, `the CI node should have steps to do: ${JSON.stringify(real)}`);
    // Only optional steps left: no card (the node says show: false)
    await p.route(/\/v1\/me\/first-steps$/, (r) => r.fulfill({ json: { show: false, hidden: false, steps: [{ id: 'play', done: true, optional: false }, { id: 'contact', done: true, optional: false }, { id: 'pubky', done: false, optional: true }] } }));
    await p.goto(BASE + '/'); await p.waitForSelector('#h-setup', { state: 'attached' }); await p.waitForTimeout(1500);
    check(!(await p.$('#h-setup .setup-card')), 'no card once only optional steps are left');
    await p.unroute(/\/v1\/me\/first-steps$/);
    await c.close();

    // ---- Library folders ----
    c = await ctx(); p = await c.newPage();
    const ns = await api('GET', '/node/settings');
    await p.goto(BASE + '/settings/library');
    await p.waitForSelector('#folderadd');
    const rows = await p.$$eval('#main code.path', (els) => els.map((e) => { const r = e.closest('.set-row'); return { path: e.textContent, text: r.textContent, remove: !!r.querySelector('[data-folder-del]') }; }));
    for (const f of ns.config_folders) {
      const row = rows.find((r) => r.path === f);
      check(!!row && /From config/.test(row.text), `${f} is marked as from the config`);
      check(!!row && !row.remove, `${f} has no Remove`);
    }

    // ---- Group outputs ----
    await p.route(/\/v1\/outputs$/, (r) => r.fulfill({ json: OUTS }));
    let made = null;
    await p.route(/\/v1\/zones$/, (r) => { if (r.request().method() !== 'POST') return r.continue(); made = r.request().postDataJSON(); return r.fulfill({ json: { id: 'g', name: made.name, output: 'group:x', transport: 'stopped', sync: 'loose' } }); });
    await p.goto(BASE + '/settings/playback');
    await p.waitForSelector('#groupouts');
    const offered = await p.$$eval('#groupouts input', (els) => els.map((e) => e.value));
    check(JSON.stringify(offered.slice().sort()) === JSON.stringify(GROUPABLE.slice().sort()), `the outputs that group in sync are offered, the others not (${offered.join(', ')})`);
    const kinds = await p.$$eval('#groupouts .groupkind .small', (els) => els.map((e) => e.textContent));
    check(['Sonos', 'BluOS', 'Squeezelite', 'The app'].every((k) => kinds.some((x) => x.startsWith(k))), `by kind (${kinds.join(' · ')})`);
    const box = (id) => p.locator(`#groupouts input[value="${id}"]`);
    await box('upnp:RINCON_A').check(); await box('upnp:RINCON_B').check();
    check(/Sonos groups them itself/.test(await p.textContent('#groupsync')), 'two Sonos: Sonos groups them');
    check(await box('bluos:aa01').isDisabled() && await box('slim:00:04:20:00:00:01').isDisabled(), 'another kind is not offered with them');
    await p.fill('#groupname', 'Downstairs'); await p.click('#groupmake');
    await p.waitForTimeout(500);
    check(made && JSON.stringify(made.outputs) === JSON.stringify(['upnp:RINCON_A', 'upnp:RINCON_B']), `the group the page asks for (${JSON.stringify(made)})`);
    await p.goto(BASE + '/settings/playback'); await p.waitForSelector('#groupouts');
    await box('slim:00:04:20:00:00:01').check(); await box('slim:00:04:20:00:00:02').check();
    check(/In sync: the node keeps them in time/.test(await p.textContent('#groupsync')), 'two squeezelite players: the node keeps them in time');
    check(await box('slim:00:04:20:00:00:03').isDisabled(), 'a player that shares no sample rate with them is not offered');
    check(await box('net:192.168.1.20:7790/alsa:hw:1,0').isDisabled(), 'the app\u2019s players are another kind');
    await p.goto(BASE + '/settings/playback'); await p.waitForSelector('#groupouts');
    await box('net:192.168.1.20:7790/alsa:hw:1,0').check(); await box('net:192.168.1.21:7790/alsa:hw:0,0').check();
    check(/In sync: the node keeps them in time/.test(await p.textContent('#groupsync')), 'two of the app\u2019s players on a DAC: the node keeps them in time');
    check(await box('slim:00:04:20:00:00:01').isDisabled(), 'squeezelite players are not offered with them');
    await c.close();

    // ---- About: tokens ----
    c = await ctx(); p = await c.newPage();
    await p.goto(BASE + '/settings/about');
    await p.waitForSelector('#logout');
    check((await p.textContent('#logout')).trim() === 'Use a different token in this browser', 'the browser token button says what it does');
    check(!(await p.$('#tokennew')) && /Set in the node’s config/.test(await p.textContent('#main')) && await p.locator('#main a[href*="troubleshooting.md"]').count() === 1, 'a config token: changed in the config, with the guide');
    await p.route(/\/v1\/node\/settings$/, async (r) => { if (r.request().method() !== 'GET') return r.continue(); const res = await r.fetch(); const j = await res.json(); return r.fulfill({ json: { ...j, token_replaceable: true } }); });
    let rotated = 0; await p.route(/\/v1\/node\/token$/, (r) => { rotated++; return r.fulfill({ json: { token: 'f'.repeat(64) } }); });
    await p.goto(BASE + '/settings/about');
    await p.click('#tokennew');
    await p.click('dialog .choice');
    check(await visible(p, '#tokenout code'), 'the new token is shown');
    check(rotated === 1 && (await p.textContent('#tokenout code')) === 'f'.repeat(64), 'once, as the node made it');
    check(await p.evaluate(() => localStorage.getItem('pm.token')) === 'f'.repeat(64), 'this browser uses it');
    await Promise.all([p.waitForNavigation({ timeout: 10000 }).catch(() => null), p.click('#tokendone')]);
    check(await p.evaluate(() => performance.getEntriesByType('navigation')[0].type) === 'reload', 'Copy and reload reloads the page');
    await c.close();

    // ---- Provider badges on covers ----
    // A mocked TIDAL artist page: a TIDAL album and a library album as tiles, a streamed and a
    // library song as rows; then album pages from TIDAL and from the library.
    c = await ctx(); p = await c.newPage();
    const IMG = BASE + '/icon.svg';
    const pageJson = (kind, id, extra) => ({ kind, id, uri: null, target: {}, title: 'Mock ' + kind, artist: kind === 'artist' ? null : { id: 'tidal-77', name: 'Mock Artist' }, album: null, year: 2020, image: kind === 'artist' ? null : IMG, source: 'TIDAL', source_id: 'tidal', section: 'home', plays_from: null, connect: [], reading: false, pending: false, songs: [], total_ms: 0, albums: [], versions: [], also_on: [], opened_as: null, plays_note: null, song_uri: null, link: null, on_pubky: null, ...extra });
    const SONGS = [{ no: 1, title: 'Streamed song', artist: 'Mock Artist', duration_ms: 1000, uri: 'tidal:1', track_id: null, page: null, image: IMG, id: 'tidal-1', artist_id: 'tidal-77', album: 'Mock album', album_id: 'tidal-5' }, { no: 2, title: 'Local song', artist: 'Mock Artist', duration_ms: 1000, uri: 'track:5', track_id: 5, page: null, image: IMG, id: 'lib-5', artist_id: null, album: 'Local album', album_id: 'lib-1' }];
    const ALBUMS = [{ id: 'tidal-5', title: 'Mock album', image: IMG, year: 2020 }, { id: 'lib-1', title: 'Local album', image: IMG, year: 2019 }];
    await p.route(/\/v1\/item\/(artist|album)\/[a-z0-9-]+(\/extras)?$/, (r) => {
      const u = new URL(r.request().url()); if (u.pathname.endsWith('/extras')) return r.fulfill({ json: {} });
      const [, , kind, id] = u.pathname.replace('/v1/', '/').split('/');
      if (kind === 'artist') return r.fulfill({ json: pageJson('artist', id, { songs: SONGS, albums: ALBUMS }) });
      return r.fulfill({ json: pageJson('album', id, id.startsWith('lib-') ? { source: 'Your library', source_id: 'library', songs: [SONGS[1]] } : { songs: [SONGS[0]] }) });
    });
    // A badge in a button (a tile) is decoration with the words at the end of the button's name; elsewhere it is named itself
    const badge = async (sel) => p.$eval(sel, (el) => { const b = el.querySelector('.src-badge.cover'); if (!b) return null; const cs = getComputedStyle(b); const art = b.closest('.art, .thumbwrap'); const ab = art.getBoundingClientRect(); const bb = b.getBoundingClientRect(); const btn = b.closest('button'); const sr = btn && btn.querySelector('.sr-only'); return { label: b.getAttribute('aria-label') || (sr ? sr.textContent.replace(/^, from /, 'From ') : null), role: b.getAttribute('role'), hidden: b.getAttribute('aria-hidden') === 'true', color: cs.color, bg: cs.backgroundColor, ring: cs.boxShadow, absolute: cs.position === 'absolute', inside: bb.left >= ab.left && bb.bottom <= ab.bottom + 0.5 && bb.left - ab.left < ab.width / 3 && ab.bottom - bb.bottom < ab.height / 3, square: Math.abs(ab.width - ab.height) < 1.5 }; }).catch(() => null);
    await p.goto(BASE + '/artist/tidal-77');
    await p.waitForSelector('#ialbums [data-tile="1"]');
    const t0 = await badge('#ialbums [data-tile="0"]'); const t1 = await badge('#ialbums [data-tile="1"]');
    check(t0 && t0.label === 'From TIDAL' && t0.hidden && !t0.role, `a TIDAL album tile says where it is from, at the end of its name, the badge itself decoration (${t0 && t0.label})`);
    check(t1 && t1.label === 'From your library', `a library album tile too (${t1 && t1.label})`);
    check(t0 && t0.absolute && t0.inside && t0.square, 'in the cover’s bottom corner, over it (the cover stays square)');
    check(t0 && t0.color === 'rgb(51, 232, 224)' && t1 && t1.color === 'rgb(255, 255, 255)', `the service’s colour, white for the library (${t0 && t0.color}, ${t1 && t1.color})`);
    check(t0 && /rgba\(12, 12, 14, 0\.88\)/.test(t0.bg) && /rgba\(255, 255, 255, 0\.3\)/.test(t0.ring), 'a dark disc with a light ring, for light and dark covers');
    const r0 = await badge('#isongs [data-srow="0"]'); const r1 = await badge('#isongs [data-srow="1"]');
    check(r0 && r0.label === 'From TIDAL' && r0.role === 'img' && r0.inside && r1 && r1.label === 'From your library' && r1.inside, `song rows with a cover carry it too, named (${r0 && r0.label}, ${r1 && r1.label})`);
    check(!(await p.$('.hero .src-badge.cover')), 'an artist has no cover, so no badge');
    await p.goto(BASE + '/album/tidal-5'); await p.waitForSelector('.hero .art');
    const h0 = await badge('.hero .art');
    check(h0 && h0.label === 'From TIDAL' && h0.inside, `the album page’s cover says where it is from (${h0 && h0.label})`);
    await p.goto(BASE + '/album/lib-1'); await p.waitForSelector('.hero .art');
    const h1 = await badge('.hero .art');
    check(h1 && h1.label === 'From your library', `a library album’s cover too (${h1 && h1.label})`);
    await p.unroute(/\/v1\/item\/(artist|album)\/[a-z0-9-]+(\/extras)?$/);
    // Now playing: the first library track plays in This browser; the bar's art and the big cover
    await p.goto(BASE + '/tracks'); await p.waitForSelector('.track'); await p.hover('.track'); await p.click('.track [data-act="play"]');
    check(await visible(p, '#npart .src-badge.cover[title="From your library"][aria-hidden="true"]'), 'the player bar’s cover says where it plays from (decoration in its button)');
    await p.click('#npart');
    check(await visible(p, '.np-big .src-badge.cover[aria-label="From your library"]'), 'and so does the Now playing cover');
    const big = await badge('.np-big .art');
    check(big && big.inside && big.absolute, 'in its corner, over the cover');
    await c.close();
    // ---- Move to, in the Play to menu ----
    // A paused library song in This browser moves to the Null output zone (a developer output,
    // switched on for this) at its position, paused, and back; the device line is in words; the
    // node's errors show in the menu; the Queue page has the same Move to.
    const devBefore = (await api('GET', '/node/settings')).dev_outputs;
    await api('PUT', '/node/settings', { dev_outputs: true });
    const nz = (await api('GET', '/zones')).find((z) => String(z.output).startsWith('null'));
    if (nz) {
      await api('DELETE', `/zones/${encodeURIComponent(nz.id)}/queue`).catch(() => {});
      c = await ctx(); p = await c.newPage();
      // The node knows no model for the Null output; say one, as AirPlay or UPnP would
      await p.route(/\/v1\/outputs$/, async (r) => { const res = await r.fetch(); const j = await res.json(); return r.fulfill({ json: j.map((o) => (o.id === nz.output ? { ...o, model: 'HomePod mini' } : o)) }); });
      await p.goto(BASE + '/tracks'); await p.waitForSelector('.track'); await p.hover('.track'); await p.click('.track [data-act="play"]');
      check(await visible(p, '#npart'), 'a library song plays in This browser');
      await p.waitForTimeout(1200);
      await p.evaluate(() => document.querySelector('audio').pause()); await p.waitForTimeout(300);
      await p.click('#zonebtn'); await p.waitForSelector('#zonepop:not([hidden]) [data-zone]');
      const row = (id) => p.locator(`#zonepop .pop-row:has([data-zone="${id}"])`);
      check(await row(nz.id).locator('[data-move]').count() === 1 && await row('browser').locator('[data-move]').count() === 0, 'every other device gets Move here while something is queued');
      await p.waitForTimeout(800); // the outputs arrive and the device lines re-render
      const sub = row(nz.id).locator('.sub').first();
      const subText = await sub.evaluate((e) => { const c = e.cloneNode(true); c.querySelectorAll('.sr-only').forEach((x) => x.remove()); return c.textContent; }); // the id is there too, for screen readers only
      check(subText === 'Null output, for developers · HomePod mini' && (await sub.getAttribute('title')) === nz.output, `the device line is in words, the id a tooltip (${subText})`);
      check(await p.locator('#zpmove').count() === 1, 'and This zone has Move to…');
      await row(nz.id).locator('[data-move]').click();
      check(await waitText(p, '#zonename', nz.name), 'Move here: the zone in use is the new one');
      const zs = await api('GET', `/zones/${encodeURIComponent(nz.id)}`);
      check(zs.queue.length >= 1 && zs.transport === 'paused' && zs.now_playing && zs.now_playing.index === 0, `the queue and the song moved to the zone, paused (${zs.transport}, ${zs.queue.length} items, index ${zs.now_playing && zs.now_playing.index})`);
      check(await p.evaluate(() => !document.querySelector('audio').getAttribute('src')), 'the browser stopped');
      check(!(await p.$('#zonepop[hidden]')) && await row(nz.id).locator('.pop-item.on').count() === 1 && await row('browser').locator('[data-move]').count() === 1, 'the menu follows the new zone, with Move here on This browser');
      await row('browser').locator('[data-move]').click();
      check(await waitText(p, '#zonename', 'This browser'), 'Move here on This browser: the browser is the zone in use again');
      await p.waitForTimeout(800);
      const back = await p.evaluate(() => { const a = document.querySelector('audio'); return { src: !!a.getAttribute('src'), paused: a.paused }; });
      const zs2 = await api('GET', `/zones/${encodeURIComponent(nz.id)}`);
      check(back.src && back.paused && zs2.queue.length === 0, `the song is back in the browser, still paused; the zone is empty (${JSON.stringify(back)}, ${zs2.queue.length} items)`);
      // The node's errors show in the menu
      await p.route(/\/v1\/zones\/[^/]+\/load$/, (r) => r.fulfill({ status: 403, json: { error: 'the owner has not opened that zone to listeners' } }));
      await row(nz.id).locator('[data-move]').click();
      check(await visible(p, '#zonepop .pop-err'), 'an error from the node shows in the menu');
      check(/not opened that zone to listeners/.test((await p.textContent('#zonepop .pop-err')) || ''), 'in the node’s words');
      await p.unroute(/\/v1\/zones\/[^/]+\/load$/);
      // The Queue page's Move to
      await p.goto(BASE + '/queue'); await p.waitForSelector('#main .list, #main .empty', { state: 'attached' }).catch(() => {});
      await p.waitForTimeout(1500); await p.click('.nav-item[data-view="queue"]'); // the developer zone shows once the settings arrived
      await p.waitForSelector('#qtransfer');
      check((await p.textContent('#qtransfer')).trim() === 'Move to…', 'the Queue page offers Move to… for This browser too');
      await c.close();
    } else check(false, 'the node should have a Null output zone for this');
    await api('PUT', '/node/settings', { dev_outputs: !!devBefore }).catch(() => {});

    // ---- The sound pill and its panel; immediate feedback on play ----
    if (nz) {
      await api('PUT', '/node/settings', { dev_outputs: true });
      await api('DELETE', `/zones/${encodeURIComponent(nz.id)}/queue`).catch(() => {});
      await api('DELETE', `/zones/${encodeURIComponent(nz.id)}/dsp`).catch(() => {});
      c = await ctx(); p = await c.newPage();
      // In This browser: the pill names the source, the panel says the browser decodes the file
      await p.goto(BASE + '/tracks'); await p.waitForSelector('.track'); await p.hover('.track'); await p.click('.track [data-act="play"]');
      check(await visible(p, '#quality .qsrc svg'), 'the pill carries the provider’s mark');
      check(await waitText(p, '#quality .qtext', 'Played by this browser'), 'and says the browser plays it');
      await p.click('#quality'); await p.waitForSelector('#qpanel:not([hidden]) .qp-zone');
      check((await p.textContent('#qpanel .qp-zone .name')).trim() === 'This browser' && (await p.textContent('#qpanel .qp-src .name')).trim() === 'Your library', 'the panel: the zone and the source');
      check(/decodes the file itself/.test(await p.textContent('#qpanel')) && await p.locator('#qpvol').count() === 1, 'no signal path claim for the browser; volume is there');
      check(await p.getAttribute('#quality', 'aria-expanded') === 'true', 'the pill says the panel is open');
      await p.click('#scrim'); check(await gone(p, '#qpanel:not([hidden])'), 'the scrim closes it');
      // On a node zone (paused there, moved from the browser): the path in short, the panel in full
      await p.evaluate(() => document.querySelector('audio').pause()); await p.waitForTimeout(300);
      await p.click('#zonebtn'); await p.waitForSelector('#zonepop:not([hidden]) [data-move]');
      await p.locator(`#zonepop .pop-row:has([data-zone="${nz.id}"]) [data-move]`).click();
      check(await waitText(p, '#zonename', nz.name), 'moved to the Null output zone');
      await p.click('#scrim'); await gone(p, '#zonepop:not([hidden])'); // the menu followed the move; close it
      const pill = async () => p.$eval('#quality', (q) => ({ cls: q.className, text: q.querySelector('.qtext').textContent, dsp: !q.querySelector('.qdsp').hidden, src: !!q.querySelector('.qsrc svg'), dev: q.querySelector('.qshort .qdev') && q.querySelector('.qshort .qdev').textContent, badge: q.querySelector('.qshort .qbadge') && q.querySelector('.qshort .qbadge').textContent }));
      let pl = await pill();
      check(/^FLAC \d+\/[\d.]+ → \d+\/[\d.]+ · Bit-perfect$/.test(pl.text) && /q-bit_perfect/.test(pl.cls), `the pill: format and rate in, out, and the verdict (${pl.text})`);
      check(pl.src && !pl.dsp, 'the provider’s mark, and no DSP badge while no DSP stage is in the path');
      check(pl.dev === nz.name && /^\d+\/[\d.]+$/.test(pl.badge || ''), `the phone form is ready: the device and a quality badge (${pl.dev} · ${pl.badge})`);
      await p.click('#quality'); await p.waitForSelector('#qpanel:not([hidden]) [data-dsp]');
      check((await p.textContent('#qpanel .qp-zone .name')).trim() === nz.name && (await p.textContent('#qpanel .qp-zone .sub')).startsWith('Null output'), 'the panel: zone and device in words');
      check(await p.locator('#qpanel .path-row').count() >= 2, 'the full signal path, stage by stage');
      check(await p.locator('#qpanel [data-dsp]').count() === 4 && await p.locator('#qpanel [data-dsp="eq"]').isDisabled() && !(await p.locator('#qpanel [data-dsp="loudness"]').isDisabled()), 'four DSP switches; EQ is off-limits with no bands set');
      await p.locator('#qpanel [data-dsp="loudness"]').check({ force: true }); await p.waitForTimeout(800);
      const dsp1 = await api('GET', `/zones/${encodeURIComponent(nz.id)}/dsp`);
      check(dsp1.loudness && dsp1.loudness !== 'off' && await p.locator('#qpanel [data-dsp="loudness"]').isChecked(), `a switch sets this zone’s DSP (loudness ${dsp1.loudness})`);
      await p.locator('#qpanel [data-dsp="loudness"]').uncheck({ force: true }); await p.waitForTimeout(800);
      check((await api('GET', `/zones/${encodeURIComponent(nz.id)}/dsp`)).loudness === 'off', 'and back off');
      await p.click('#qpdsp'); await p.waitForSelector('#dspbox .card.dsp', { timeout: 10000 }).catch(() => {});
      check(await p.locator('#dspbox .card.dsp').count() === 1, 'All DSP settings opens this zone’s DSP editor');
      // Phones: the pill shows the device and the badge, not the words
      await p.setViewportSize({ width: 390, height: 844 }); await p.waitForTimeout(300);
      const phone = await p.$eval('#quality', (q) => ({ shown: q.offsetWidth > 0 && getComputedStyle(q).display !== 'none', text: getComputedStyle(q.querySelector('.qtext')).display, short: getComputedStyle(q.querySelector('.qshort')).display, mark: !!q.querySelector('.qsrc svg'), width: q.offsetWidth }));
      check(phone.shown && phone.text === 'none' && phone.short !== 'none' && phone.mark && phone.width <= 150, `on a phone the pill stays, short: the mark, the device and a badge (${phone.width}px)`);
      await p.setViewportSize({ width: 1280, height: 900 });
      // Pressing play answers at once: a slow node, then a node that refuses
      await api('DELETE', `/zones/${encodeURIComponent(nz.id)}/queue`).catch(() => {});
      await p.route(/\/v1\/zones\/[^/]+\/queue$/, async (r) => { if (r.request().method() !== 'POST') return r.continue(); await new Promise((res) => setTimeout(res, 1500)); return r.continue(); });
      await p.goto(BASE + '/tracks'); await p.waitForSelector('.track');
      // the browser context resets the zone to This browser on every page: pick the node zone again
      await p.click('#zonebtn'); await p.waitForSelector(`#zonepop:not([hidden]) [data-zone="${nz.id}"]`); await p.click(`#zonepop [data-zone="${nz.id}"]`);
      check(await waitText(p, '#zonename', nz.name), 'playing to the Null output zone');
      await p.hover('.track'); await p.click('.track [data-act="play"]');
      const early = await p.$eval('#np', (n) => ({ text: n.textContent, spin: !!n.querySelector('.spin') }));
      check(/Starting on Null output/.test(early.text) && early.spin, `the bar says it is starting, at once (${early.text.trim().replace(/\s+/g, ' ')})`);
      check(await p.locator('.track .rowplay.starting').count() === 1, 'the pressed play button spins');
      const okStart = await (async () => { const end = Date.now() + 15000; for (;;) { const t2 = await p.textContent('#np'); if (!/Starting on/.test(t2)) return t2; if (Date.now() > end) return null; await p.waitForTimeout(200); } })();
      check(okStart != null && !/Couldn’t start/.test(okStart) && await p.locator('.track .rowplay.starting').count() === 0, 'then the song, once the zone plays');
      await p.unroute(/\/v1\/zones\/[^/]+\/queue$/);
      await p.route(/\/v1\/zones\/[^/]+\/queue$/, (r) => (r.request().method() === 'POST' ? r.fulfill({ status: 500, json: { error: 'the output refused the stream' } }) : r.continue()));
      await p.hover('.track'); await p.click('.track [data-act="play"]');
      check(await visible(p, '#np .warn-text'), 'a node that refuses: the bar says so');
      check(/refused the stream/.test(await p.textContent('#np')), 'in the node’s words');
      await p.unroute(/\/v1\/zones\/[^/]+\/queue$/);
      await c.close();
      await api('DELETE', `/zones/${encodeURIComponent(nz.id)}/queue`).catch(() => {});
      await api('DELETE', `/zones/${encodeURIComponent(nz.id)}/dsp`).catch(() => {});
      await api('PUT', '/node/settings', { dev_outputs: !!devBefore }).catch(() => {});
    }

    // ---- The login dialog ----
    c = await browser.newContext(); p = await c.newPage();
    await p.goto(BASE + '/');
    await p.waitForSelector('#login[open]');
    const hint = await p.textContent('#login details');
    check(/api-token/.test(hint) && /PM_TOKEN/.test(hint) && /Umbrel/.test(hint), 'the login dialog says where the token is');
    check(await p.locator('#login a[href*="troubleshooting.md"]').count() === 1, 'and links the guide');
    await c.close();
  } finally {
    await api('PUT', '/node/settings', { first_steps_hidden: !!before.first_steps_hidden }).catch(() => {});
    await browser.close();
  }
  if (problems.length) { console.log(`${problems.length} failed`); process.exit(1); }
  console.log('setup ok');
})().catch((e) => { console.error(e); process.exit(1); });
