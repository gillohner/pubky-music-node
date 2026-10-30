// Setting a node up in the web page, against a node with a config folder and a config token (CI's):
//   * Home's Get started shows the steps the node names (no music step: the config has a folder),
//     goes when hidden, stays hidden in a new browser (kept on the node), comes back from
//     Settings › Home for everyone, and goes by itself when only optional steps are left;
//   * Your library › Node library marks the config's folder and offers no Remove for it;
//   * Your library: My music lists homeserver files as songs (names, not hashes) with the save
//     status and Save now, edits a song's tags on the node, deletes a file; Add music offers
//     upload, a folder or share, and playlists; Node library takes a song out and brings it back;
//     a listener gets My music only, with no edit controls; Settings has no library or import tab;
//   * Group outputs offers the outputs that group in sync (Sonos, HEOS, BluOS, squeezelite, the
//     app's players on a DAC), by
//     kind, one kind at a time, says what a pick becomes, and doesn't offer one that shares no
//     sample rate with the ones picked;
//   * About: Log out for this browser, the node's token set in the config, or replaced and shown
//     once;
//   * Log in / Log out: the account menu's Log out ends the session and shows the Log in screen;
//     phones and tablets get Open in Pubky Ring first (QR on request), desktops the QR code; a
//     wrong identity is named ("This node belongs to …"), someone without access is told to ask;
//     Try again starts over and an approval logs in; an ended session comes back to Log in with
//     "Your session ended"; the API token is the small alternative, with where to find it; the
//     node's missing folders are a Log out line in Settings, not a banner on Home;
//   * an item page credits Wikipedia's text and a Commons picture (source, licence, links), and
//     redraws only About when it arrives in the background;
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
//   * search: results grouped (songs, albums, playlists, people, posts…), facet chips that narrow
//     them, `tag:` like `#`, and the keyboard: ↑/↓ walk the results, Enter opens.
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

    // ---- Your library › Node library: folders ----
    c = await ctx(); p = await c.newPage();
    const ns = await api('GET', '/node/settings');
    await p.goto(BASE + '/library/node');
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
    check((await p.textContent('#logout')).trim() === 'Log out', 'this browser has Log out');
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

    // ---- About credits: Wikipedia's text and a Commons picture carry their source and licence ----
    c = await ctx(); p = await c.newPage();
    const credited = { kind: 'artist', id: 'lib-777', uri: null, target: { artist_id: 777 }, title: 'Credit Test', source: 'Your library', source_id: 'library',
      songs: [], albums: [], versions: [], also_on: [], open_on: [], connect: [], about: [], section: 'artists', reading: false, pending: false, total_ms: 0,
      blurb: 'Credit Test ist eine Band.',
      about_text: { text: 'Credit Test ist eine Band.', source: 'Wikipedia', url: 'https://de.wikipedia.org/wiki/Credit_Test', lang: 'de', licence: 'CC BY-SA 4.0', licence_url: 'https://creativecommons.org/licenses/by-sa/4.0/' },
      image_credit: { author: '<b>Jane</b> Doe', licence: 'CC BY 2.0', licence_url: 'javascript:alert(1)', file_page: 'https://commons.wikimedia.org/wiki/File:Credit.jpg', source: 'Wikimedia Commons' } };
    await p.route(/\/v1\/item\/artist\/lib-777(\?.*)?$/, (r) => r.fulfill({ json: credited }));
    await p.goto(BASE + '/artist/lib-777');
    check(await visible(p, '#iabout .credit'), 'About shows a credit line');
    const credit = await p.textContent('#iabout .credit').catch(() => '');
    const links = await p.$$eval('#iabout .credit a', (as) => as.map((a) => [a.textContent, a.getAttribute('href')]));
    check(links.some(([t, h]) => t === 'Read more on Wikipedia' && h === 'https://de.wikipedia.org/wiki/Credit_Test') && links.some(([t, h]) => t === 'CC BY-SA 4.0' && /creativecommons/.test(h)), 'the text links its article and CC BY-SA');
    check(/Picture: <b>Jane<\/b> Doe · CC BY 2\.0 · Wikimedia Commons/.test(credit) && links.some(([t, h]) => t === 'Wikimedia Commons' && /File:Credit\.jpg/.test(h)), 'the picture names its author, licence and file page, as text');
    check(links.every(([, h]) => /^https?:/.test(h || '')), 'a credit link is only ever a web link');
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
    // ---- Library artist pages: About under the header, Also on next to Play (F6) ----
    const LONG = 'Mock Artist is a band from Zürich. '.repeat(30);
    await p.route(/\/v1\/item\/artist\/lib-900$/, (r) => r.fulfill({ json: pageJson('artist', 'lib-900', {
      source: 'Your library', source_id: 'library', image: IMG, songs: [SONGS[1]], albums: [ALBUMS[1]],
      blurb: LONG, about_text: { text: LONG, source: 'Wikipedia', title: 'Mock Artist', url: 'https://de.wikipedia.org/wiki/Mock_Artist', lang: 'de', licence: 'CC BY-SA 4.0', licence_url: 'https://creativecommons.org/licenses/by-sa/4.0/' },
      image_credit: { author: 'Jane Doe', licence: 'CC BY-SA 3.0', licence_url: 'https://creativecommons.org/licenses/by-sa/3.0/', file_page: 'https://commons.wikimedia.org/wiki/File:Mock.jpg', source: 'Wikimedia Commons' },
      about: [{ label: 'Origin', value: 'Zürich' }, { label: 'Active', value: '1998–' }],
      also_on: [{ source: 'tidal', id: 'artist:77', label: 'TIDAL', url: 'https://tidal.com/artist/77', page: 'tidal-77' }, { source: 'spotify', id: 'artist:abc', label: 'Spotify', url: 'https://open.spotify.com/artist/abc', page: 'spotify-abc' }],
    }) }));
    // TIDAL on and working here, Spotify not: the TIDAL icon opens its page here, Spotify's its site
    // (the node gives every link a page, so the fixture does too)
    await p.route(/\/v1\/sources$/, (r) => r.fulfill({ json: [{ id: 'tidal', name: 'TIDAL', enabled: true, healthy: true }, { id: 'spotify', name: 'Spotify', enabled: true, healthy: false }] }));
    await p.route(/\/v1\/play-order$/, (r) => r.fulfill({ json: { order: ['library', 'tidal', 'spotify'], services: [{ id: 'tidal', connected: true }, { id: 'spotify', connected: false }] } }));
    await p.goto(BASE + '/artist/lib-900'); await p.waitForSelector('#iabout');
    const ab = await p.evaluate(() => { const ids = [...document.querySelectorAll('#main > section, #main > .hero')].map((e) => e.id || e.className); const b = document.querySelector('#iblurb'); const cr = document.querySelector('#iabout .credit'); return { order: ids.join(','), lang: b && b.getAttribute('lang'), clamped: b && b.classList.contains('clamp') && b.scrollHeight > b.clientHeight, credit: cr && cr.textContent, links: cr ? [...cr.querySelectorAll('a')].map((a) => a.getAttribute('href')) : [], facts: document.querySelectorAll('#iabout dl dt').length, icons: [...document.querySelectorAll('.hero .actions .also-icons .also-ico')].map((a) => a.getAttribute('aria-label')), line: (document.querySelector('#ialso') || {}).textContent || '' }; });
    check(/^hero,iabout,isongs/.test(ab.order), `About sits under the header when there is an article (${ab.order})`);
    check(ab.lang === 'de' && ab.clamped && ab.facts === 2, `the article in its language, a few lines, then the facts (${ab.lang})`);
    check(/Text: Read more on Wikipedia · CC BY-SA 4\.0/.test(ab.credit) && /Picture: Jane Doe · CC BY-SA 3\.0 · Wikimedia Commons/.test(ab.credit) && ab.links.length === 4 && ab.links.every((h) => /^https:/.test(h)), `with its link and licence, and the photo's Commons credit (${(ab.credit || '').replace(/\s+/g, ' ')})`);
    await p.click('#iblurbmore');
    check(await p.evaluate(() => !document.querySelector('#iblurb').classList.contains('clamp') && document.querySelector('#iblurbmore').getAttribute('aria-expanded') === 'true'), 'More shows the whole text');
    check([...ab.icons].sort().join('|') === 'Open on Spotify (opens a new tab)|Open on TIDAL' && !/also/.test(ab.line), `Also on: one icon per service next to Play, not in the source line (${ab.icons.join(', ')})`);
    await p.click('.also-icons [data-also]'); await p.waitForTimeout(300);
    check(/\/artist\/tidal-77$/.test(await p.evaluate(() => location.pathname)), 'a service the node plays from opens its page here');
    await p.unroute(/\/v1\/item\/artist\/lib-900$/);
    await p.unroute(/\/v1\/sources$/);
    await p.unroute(/\/v1\/play-order$/);
    // without an article, About stays at the end
    await p.goto(BASE + '/album/lib-1'); await p.waitForSelector('#iabout');
    check(await p.evaluate(() => { const s = [...document.querySelectorAll('#main > section')]; return s[s.length - 1].id === 'iabout'; }), 'without an article, About stays at the end of the page');
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
      check(await p.evaluate(() => document.querySelector('#qpanel').contains(document.activeElement)), 'focus moved into the panel');
      await p.keyboard.press('Escape'); check(await gone(p, '#qpanel:not([hidden])'), 'Escape closes it');
      check(await p.evaluate(() => document.activeElement && document.activeElement.id === 'quality'), 'and focus is back on the pill');
      await p.click('#quality'); await p.waitForSelector('#qpanel:not([hidden]) .qp-zone');
      await p.click('#scrim'); check(await gone(p, '#qpanel:not([hidden])'), 'the scrim closes it too');
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
      // ---- Play from another source (Now playing › Sources) and Similar on item pages ----
      {
        // one song on repeat, so it is still playing when Sources opens (the test files are short)
        const two = (await api('GET', '/library/tracks?limit=10')).tracks.slice(0, 2);
        await api('PUT', `/zones/${encodeURIComponent(nz.id)}/repeat`, { repeat: 'all' }).catch(() => {});
        if (two[0]) await api('POST', `/zones/${encodeURIComponent(nz.id)}/queue`, { track_ids: [two[0].id], replace: true, play: true }).catch(() => {});
        let zs = null; { const end = Date.now() + 10000; for (;;) { zs = await api('GET', `/zones/${encodeURIComponent(nz.id)}`); if (zs.transport === 'playing' || Date.now() > end) break; await p.waitForTimeout(200); } }
        // CI's Null output plays faster than real time, so a short song on repeat loops at once: pause it
        // so the bar and Now playing stay on it (Play from here keeps a paused zone paused)
        await api('POST', `/zones/${encodeURIComponent(nz.id)}/transport`, { action: 'pause' }).catch(() => {});
        { const end = Date.now() + 10000; for (;;) { zs = await api('GET', `/zones/${encodeURIComponent(nz.id)}`); if (zs.transport === 'paused' || Date.now() > end) break; await p.waitForTimeout(200); } }
        const curUri = zs.now_playing && zs.now_playing.item.uri; const curId = Number((/^track:(\d+)$/.exec(curUri || '') || [])[1]);
        const other = two.find((t) => t.id !== curId);
        if (curId && other) {
          // the song page says the song is also another library file (stands in for TIDAL, Deezer…)
          await p.route(new RegExp(`/v1/item/song/lib-${curId}$`), (r) => r.fulfill({ json: { kind: 'song', id: `lib-${curId}`, target: { track_id: curId }, source: 'Your library', section: 'albums', songs: [], albums: [], versions: [], about: [], open_on: [], also_on: [{ source: 'library', label: 'Your library: the other file', uri: `track:${other.id}`, url: '' }] } }));
          let loadBody = null;
          await p.route(/\/v1\/zones\/[^/]+\/load$/, (r) => { loadBody = JSON.parse(r.request().postData() || '{}'); return r.continue(); });
          // the page learns the zone plays from the node's events: open Now playing once the bar shows
          // the song (its cover button), not while it still says the earlier refusal
          const shown = await p.waitForSelector('#np #npart', { timeout: 20000 }).then(() => true).catch(() => false);
          check(shown, 'the bar shows the song the zone plays');
          let sw = null;
          if (shown) {
            await p.click('#np #npart');
            // Now playing draws itself again on zone updates: a locator finds the tab afresh each try
            const opened = await p.locator('#nowplaying [data-npsheet="sources"]:visible').first().click({ timeout: 15000 }).then(() => true).catch(() => false);
            if (opened) sw = await p.waitForSelector('[data-npswitch]', { timeout: 8000 }).catch(() => null);
          }
          check(!!sw && /Play from here/.test(await sw.textContent()), 'Sources: Play from here on another source of the playing song');
          if (sw) {
            await p.locator('[data-npswitch]').first().click({ timeout: 10000 });
            const moved = await (async () => { const end = Date.now() + 10000; for (;;) { const z2 = await api('GET', `/zones/${encodeURIComponent(nz.id)}`); if (z2.now_playing && z2.now_playing.item.uri === `track:${other.id}`) return z2; if (Date.now() > end) return null; await p.waitForTimeout(250); } })();
            check(!!moved && loadBody && loadBody.items[loadBody.index].uri === `track:${other.id}` && loadBody.items.length === zs.queue.length && loadBody.transport === 'paused', `the zone plays it from there, same place in the queue (${loadBody ? `${loadBody.index}/${loadBody.items.length} at ${loadBody.position_ms} ms` : 'no load'})`);
          }
          await p.unroute(new RegExp(`/v1/item/song/lib-${curId}$`)); await p.unroute(/\/v1\/zones\/[^/]+\/load$/);
          await p.keyboard.press('Escape');
          // Similar: the library's own (playable rows), then what people tagged alike on Pubky
          await p.route(new RegExp(`/v1/item/song/lib-${curId}/extras$`), (r) => r.fulfill({ json: { related: [{ uri: 'https://musicbrainz.org/release/b1a9c0e9-d987-4042-ae91-78d6a3267d69', title: 'Tagged Alike', kind: 'album', also_on: [] }], concerts: [], similar: [{ kind: 'song', title: other.title, subtitle: (other.artists[0] || {}).name || '', id: `lib-${other.id}`, track_id: other.id, duration_ms: other.duration_ms, keys: [] }] } }));
          await p.goto(BASE + `/song/lib-${curId}`); await p.waitForSelector('#isimilar', { timeout: 15000 }).catch(() => {});
          const sim = await p.evaluate(() => { const s = document.querySelector('#isimilar'); return s && { head: s.querySelector('h3').textContent, parts: [...s.querySelectorAll('.simhead')].map((h) => h.textContent), rows: s.querySelectorAll('#isimlib .srow, #isimlib .track').length, play: !!s.querySelector('#isimlib [data-splay]'), tiles: s.querySelectorAll('#irelated .tile').length }; });
          check(sim && sim.head === 'Similar' && sim.parts.join('|') === 'In your library|Tagged alike on Pubky' && sim.rows === 1 && sim.play && sim.tiles === 1, `one Similar section: your library's songs with Play, then Pubky's (${JSON.stringify(sim)})`);
          await p.unroute(new RegExp(`/v1/item/song/lib-${curId}/extras$`));
        } else check(false, `Play from here needs a playing library song and another one (${curUri})`);
        await api('PUT', `/zones/${encodeURIComponent(nz.id)}/repeat`, { repeat: 'off' }).catch(() => {});
      }
      // A refused play from the transport: the wait ends at once with the node's words
      await api('POST', `/zones/${encodeURIComponent(nz.id)}/queue`, { track_ids: [(await api('GET', '/library/tracks?limit=1')).tracks[0].id], replace: true, play: false });
      await p.goto(BASE + '/tracks'); await p.waitForSelector('.track');
      await p.click('#zonebtn'); await p.waitForSelector(`#zonepop:not([hidden]) [data-zone="${nz.id}"]`); await p.click(`#zonepop [data-zone="${nz.id}"]`);
      await p.waitForTimeout(800);
      await p.route(/\/v1\/zones\/[^/]+\/transport$/, (r) => r.fulfill({ status: 409, json: { error: 'the zone is busy' } }));
      await p.click('#toggle'); await p.waitForTimeout(500);
      const np2 = await p.textContent('#np');
      check(!/Starting on/.test(np2) && /zone is busy/.test(np2), `a refused play ends the wait at once (${np2.trim().replace(/\s+/g, ' ')})`);
      await p.unroute(/\/v1\/zones\/[^/]+\/transport$/);
      // The DSP switches keep what was set: EQ's switched-off band, crossfeed's values
      await api('PUT', `/zones/${encodeURIComponent(nz.id)}/dsp`, { dither: true, resample: true, dsd_mode: 'auto', loudness: 'off', eq: [{ kind: 'peaking', freq_hz: 100, gain_db: 2, q: 1, enabled: true }, { kind: 'peaking', freq_hz: 1000, gain_db: -2, q: 1, enabled: false }], crossfeed: { level_db: -9, cutoff_hz: 650, delay_us: 280, enabled: true } });
      await p.click('#quality'); await p.waitForSelector('#qpanel:not([hidden]) [data-dsp="eq"]:not([disabled])');
      const flip = async (k) => { await p.locator(`#qpanel [data-dsp="${k}"]`).click({ force: true }); await p.waitForSelector(`#qpanel [data-dsp="${k}"]:not([disabled])`); await p.waitForTimeout(200); };
      await flip('eq'); await flip('eq');
      const dsp = await api('GET', `/zones/${encodeURIComponent(nz.id)}/dsp`);
      check(JSON.stringify(dsp.eq.map((b) => b.enabled)) === '[true,false]', `EQ off and on keeps the band that was off (${JSON.stringify(dsp.eq.map((b) => b.enabled))})`);
      await flip('crossfeed');
      const cf1 = (await api('GET', `/zones/${encodeURIComponent(nz.id)}/dsp`)).crossfeed;
      await flip('crossfeed');
      const cf2 = (await api('GET', `/zones/${encodeURIComponent(nz.id)}/dsp`)).crossfeed;
      check(cf1 && cf1.enabled === false && cf1.level_db === -9 && cf2 && cf2.enabled === true && cf2.level_db === -9 && cf2.cutoff_hz === 650, `crossfeed off and on keeps its values (${JSON.stringify(cf1)} → ${JSON.stringify(cf2)})`);
      check(await p.evaluate(() => document.activeElement && document.activeElement.dataset && document.activeElement.dataset.dsp === 'crossfeed'), 'the switch keeps the keyboard focus');
      await p.click('#scrim');
      // Move to… from the Queue page: a song this browser can't play is not moved into it
      await p.route(/\/v1\/zones\/[^/?]+$/, (r) => (r.request().method() === 'GET' ? r.fulfill({ json: { id: nz.id, name: nz.name, output: nz.output, queue: [{ id: 1, uri: 'spotify:track:abc', title: 'Spot' }], queue_index: 0, now_playing: { index: 0, item: { id: 1, uri: 'spotify:track:abc', title: 'Spot' }, position_ms: 0 }, transport: 'paused' } }) : r.continue()));
      await p.click('.nav-item[data-view="queue"]'); await p.waitForSelector('#qtransfer');
      await p.click('#qtransfer'); await p.waitForSelector('.popover.menu:not([hidden]) .pop-item');
      await p.locator('.popover.menu:not([hidden]) .pop-item', { hasText: 'This browser' }).click();
      check(await visible(p, '.toast.error', 4000) && /plays on the node’s zones only/.test(await p.textContent('#toast')), 'a song this browser can’t play is not moved into it (the zone keeps it)');
      await p.unroute(/\/v1\/zones\/[^/?]+$/);
      // and out of This browser, Move to… says “Moving to …” at once
      await p.click('#zonebtn'); await p.waitForSelector('#zonepop:not([hidden]) [data-zone="browser"]'); await p.click('#zonepop [data-zone="browser"]');
      await p.click('.nav-item[data-view="tracks"]'); await p.waitForSelector('.track'); await p.hover('.track'); await p.click('.track [data-act="play"]'); await p.waitForTimeout(800);
      await p.evaluate(() => document.querySelector('audio').pause());
      await p.route(/\/v1\/zones\/[^/]+\/load$/, async (r) => { await new Promise((res) => setTimeout(res, 1500)); return r.fulfill({ status: 409, json: { error: 'not now' } }); });
      await p.click('.nav-item[data-view="queue"]'); await p.waitForSelector('#qtransfer');
      await p.click('#qtransfer'); await p.waitForSelector('.popover.menu:not([hidden]) .pop-item');
      await p.locator('.popover.menu:not([hidden]) .pop-item', { hasText: nz.name }).first().click();
      await p.waitForTimeout(150);
      check(/Moving to/.test(await p.textContent('#toast')), `Move to… says “Moving to …” at once (${(await p.textContent('#toast')).trim()})`);
      await p.waitForTimeout(1800);
      await p.unroute(/\/v1\/zones\/[^/]+\/load$/);
      await c.close();
      await api('DELETE', `/zones/${encodeURIComponent(nz.id)}/queue`).catch(() => {});
      await api('DELETE', `/zones/${encodeURIComponent(nz.id)}/dsp`).catch(() => {});
      await api('PUT', '/node/settings', { dev_outputs: !!devBefore }).catch(() => {});
    }
    // ---- Your library ----
    c = await ctx(); p = await c.newPage();
    const choose = async (label) => { await p.waitForSelector('dialog[open] .choice'); await p.locator('dialog[open] .choice', { hasText: label }).first().click(); };
    // The owner: three tabs; without a Pubky session (CI's node) My music says to sign in
    await p.goto(BASE + '/library'); await p.waitForSelector('#libadd');
    const libTabs = await p.$$eval('#main [data-libtab]', (els) => els.map((e) => e.textContent.trim()));
    check(JSON.stringify(libTabs) === '["My music","Node library","Import"]', `the owner's tabs (${libTabs.join(', ')})`);
    check(/Sign in with Pubky/.test(await p.textContent('#main')), 'without a Pubky session My music asks to sign in');
    check(await p.locator('.nav-item[data-view="library"]').count() === 1, 'Your library is in the main navigation');
    await p.goto(BASE + '/settings'); await p.waitForSelector('#main .set-group', { state: 'attached' });
    const setNav = await p.textContent('#main');
    check(!/Library folders/.test(setNav) && !/Move from Roon/.test(setNav), 'Settings keeps settings: no library folders, no import');
    // My music with a session (mocked): songs by name, the save status, Save now, the ⋯ menu
    const libTracks = (await api('GET', '/library/tracks?limit=5')).tracks;
    const lt0 = libTracks[0];
    await p.route(/\/v1\/pubky$/, (r) => r.fulfill({ json: { user: 'k'.repeat(52), sync: null, needs_resign: false } }));
    await p.route(/\/v1\/storage$/, (r) => r.fulfill({ json: { homeserver: { mode: 'bundled', quota_mb: 1000, used_bytes: 8300000 }, saving: { state: 'saved', pending: 0, last_saved_at: Math.floor(Date.now() / 1000) - 120 } } }));
    const HASHED = '0123456789abcdef-one.flac';
    await p.route(/\/v1\/me\/music$/, (r) => r.fulfill({ json: { count: 2, bytes: 8300100, files: [
      { path: `/priv/pubky.music/files/${HASHED}`, name: 'one.flac', size: 8300000, song: { id: lt0.id, title: lt0.title, artist: 'The Band', duration_ms: 5000, edited: false } },
      { path: '/priv/pubky.music/files/fedcba9876543210-new.flac', name: 'new.flac', size: 100 },
    ] } }));
    let deleted = null;
    await p.route(/\/v1\/pubky\/files$/, (r) => { if (r.request().method() !== 'DELETE') return r.continue(); deleted = r.request().postDataJSON(); return r.fulfill({ json: [] }); });
    await p.goto(BASE + '/library'); await p.waitForSelector('#main .mrow');
    const mineText = await p.textContent('#main');
    check(mineText.includes(lt0.title) && !mineText.includes(HASHED), 'a homeserver file shows as its song, not its hashed name');
    check(/Being added/.test(mineText) && mineText.includes('new.flac'), 'a file the library hasn’t read yet shows by its own name');
    check(/Saved · 2 min ago/.test(await p.textContent('#libsaved')) && (await p.textContent('#sync')).trim() === 'Save now', 'the save status in a few words, with Save now');
    await p.click('#main [data-mmore]');
    const mItems = await p.$$eval('.popover.menu:not([hidden]) .pop-item', (els) => els.map((e) => e.textContent.trim()));
    check(['Edit tags…', 'Download', 'Delete from my homeserver'].every((x) => mItems.includes(x)), `a song’s ⋯: edit, download, delete (${mItems.join(', ')})`);
    // Edit tags: on the node, the file untouched; then back to the file's tags
    await p.locator('.popover.menu:not([hidden]) .pop-item', { hasText: 'Edit tags…' }).click();
    await p.waitForSelector('dialog[open] [data-k="title"]');
    check(await p.inputValue('dialog[open] [data-k="title"]') === '' && await p.getAttribute('dialog[open] [data-k="title"]', 'placeholder') === lt0.title, 'the edit dialog shows what the file says (an empty field keeps it)');
    await p.fill('dialog[open] [data-k="title"]', 'Edited by ui-setup'); await p.click('dialog[open] [data-save]');
    await p.waitForTimeout(600);
    check((await api('GET', `/library/tracks/${lt0.id}`)).title === 'Edited by ui-setup', 'Save edits the song on the node');
    const ed = await api('GET', `/library/tracks/${lt0.id}/edit`);
    check(ed.edit && ed.edit.title === 'Edited by ui-setup' && !ed.edit.artist && !ed.edit.album, `only the changed field is pinned (${JSON.stringify(ed.edit)})`);
    await api('DELETE', `/library/tracks/${lt0.id}/edit`);
    check((await api('GET', `/library/tracks/${lt0.id}`)).title === lt0.title, 'and it goes back to the file’s tags');
    // Delete a file from the homeserver, after asking
    await p.goto(BASE + '/library'); await p.waitForSelector('#main [data-mdel]');
    await p.click('#main [data-mdel]'); await choose('Delete'); await p.waitForTimeout(400);
    check(deleted && JSON.stringify(deleted.paths) === JSON.stringify(['/priv/pubky.music/files/fedcba9876543210-new.flac']), `Delete removes that file from the homeserver (${JSON.stringify(deleted)})`);
    // Add music: three ways in; Upload opens the drop zone, Import its tab, a folder is checked
    await p.click('#libadd');
    await p.waitForSelector('dialog[open] .choice');
    const ways = await p.$$eval('dialog[open] .choice .choice-label', (els) => els.map((e) => e.textContent.trim()));
    check(JSON.stringify(ways) === '["Upload files","Add a folder or network share","Import playlists"]', `Add music: ${ways.join(', ')}`);
    await choose('Upload files');
    check(await visible(p, '#libupload #updrop'), 'Upload files opens the drop zone');
    await p.click('#libadd'); await choose('Add a folder or network share'); await choose('A folder on the node');
    await p.waitForSelector('dialog[open] [data-field]'); await p.fill('dialog[open] [data-field]', '/no/such/folder/ui-setup'); await choose('Add folder');
    check(await visible(p, '.toast.error'), 'a folder that isn’t there is refused before it is saved');
    await p.click('#libadd'); await choose('Import playlists');
    check(await p.locator('#libplimport').waitFor({ state: 'attached', timeout: 8000 }).then(() => true).catch(() => false) && new URL(p.url()).pathname === '/library/import' && /Move from Roon/.test(await p.textContent('#main')), 'Import playlists opens Your library › Import (playlist files, Move from Roon)');
    await p.unroute(/\/v1\/pubky$/); await p.unroute(/\/v1\/storage$/); await p.unroute(/\/v1\/me\/music$/); await p.unroute(/\/v1\/pubky\/files$/);
    // Node library: a song taken out from its ⋯ menu, then brought back
    const before = (await api('GET', '/library/tracks?limit=50')).tracks.length;
    await p.goto(BASE + '/tracks'); await p.waitForSelector('.track'); await p.hover('.track'); await p.click('.track [data-act="more"]');
    await p.waitForSelector('.popover.menu:not([hidden]) .pop-item');
    await p.locator('.popover.menu:not([hidden]) .pop-item', { hasText: 'Remove from library' }).click();
    await choose('Remove'); await p.waitForTimeout(600);
    check((await api('GET', '/library/tracks?limit=50')).tracks.length === before - 1, 'Remove from library takes the song out');
    await p.goto(BASE + '/library/node'); await p.waitForSelector('#main [data-unhide]');
    check(/Taken out of the library/.test(await p.textContent('#main')), 'Node library lists it as taken out');
    await p.click('#main [data-unhide]');
    const backN = await (async () => { const end = Date.now() + 20000; for (;;) { const n = (await api('GET', '/library/tracks?limit=50')).tracks.length; if (n === before || Date.now() > end) return n; await p.waitForTimeout(300); } })();
    check(backN === before, `Bring back returns it (${backN} of ${before})`);
    await c.close();
    // A listener: My music only, no edit controls
    c = await ctx(); p = await c.newPage();
    await p.route(/\/v1\/me$/, async (r) => { const res = await r.fetch(); const j = await res.json(); return r.fulfill({ json: { ...j, role: 'listener', zones: null } }); });
    await p.goto(BASE + '/library'); await p.waitForSelector('#main [data-libtab]');
    check(JSON.stringify(await p.$$eval('#main [data-libtab]', (els) => els.map((e) => e.textContent.trim()))) === '["My music"]' && !(await p.$('#libadd')), 'a listener gets My music only, without Add music');
    await p.goto(BASE + '/tracks'); await p.waitForSelector('.track'); await p.hover('.track'); await p.click('.track [data-act="more"]');
    await p.waitForSelector('.popover.menu:not([hidden]) .pop-item');
    const lItems = await p.$$eval('.popover.menu:not([hidden]) .pop-item', (els) => els.map((e) => e.textContent.trim()));
    check(lItems.includes('Play next') && !lItems.includes('Edit tags…') && !lItems.includes('Remove from library'), `no edit controls for a listener (${lItems.join(', ')})`);
    await c.close();
    // ---- Search: groups, facets, tag:, keyboard ----
    // A mocked /v1/search stream (the real sources are pm-api's tests): the page groups, narrows
    // by facets, sends tag: as the node expects, and walks the results from the keyboard.
    c = await ctx(); p = await c.newPage();
    const KEY = 'k'.repeat(52);
    const ndjson = (lines) => lines.map((l) => JSON.stringify(l)).join('\n') + '\n';
    const seen = [];
    await p.route(/\/v1\/search\?/, (r) => {
      const u = new URL(r.request().url()); seen.push(u.searchParams.get('q'));
      const body = ndjson([
        { source: 'library', status: 'ok', ms: 3, hits: [
          { kind: 'song', title: 'Old Song', subtitle: 'Band', id: 'lib-5', track_id: 5, year: 1999, format: 'FLAC 16/44.1', quality: 'lossless', keys: [] },
          { kind: 'song', title: 'New Song', subtitle: 'Band', id: 'lib-6', track_id: 6, year: 2015, format: 'MP3', quality: 'lossy', keys: [] },
          { kind: 'album', title: 'Old Album', subtitle: 'Band', id: 'lib-1', year: 1999, keys: [] },
          { kind: 'playlist', title: 'Saturday chill', subtitle: '3 songs · your playlist', id: 'pl1', keys: [] }] },
        { source: 'pubky', status: 'ok', ms: 40, hits: [{ kind: 'song', title: 'Shared Song', subtitle: 'Someone', id: 'tidal-9', uri: 'tidal:track:9', page: 'https://tidal.com/track/9', site: 'tidal.com', year: 2015, keys: [] }] },
        { source: 'people', status: 'ok', ms: 50, hits: [{ kind: 'person', title: 'Sat', subtitle: 'plays bass', id: KEY, page: `https://pubky.app/profile/${KEY}`, keys: [] }] },
        { source: 'posts', status: 'ok', ms: 60, hits: [{ kind: 'post', title: 'Great music tonight', subtitle: 'Author One', id: 'post-a:1', page: 'https://pubky.app/post/a/1', keys: [] }] },
      ]);
      return r.fulfill({ status: 200, headers: { 'content-type': 'application/x-ndjson', 'x-pm-sources': 'library,pubky,people,posts' }, body });
    });
    await p.route(/\/v1\/search\/tags\?/, (r) => r.fulfill({ json: { mine: [], others: [] } }));
    await p.goto(BASE + '/'); await p.waitForSelector('#search');
    await p.fill('#search', 'sat'); await p.press('#search', 'Enter');
    await p.waitForSelector('#sresults [data-skind="post"]');
    const heads = await p.$$eval('#sresults .section h3', (els) => els.map((e) => e.textContent.trim()));
    check(['Albums', 'Songs', 'Playlists', 'People', 'Posts'].every((h) => heads.includes(h)), `results come grouped (${heads.join(', ')})`);
    check((await p.locator('#sresults [data-skind="person"] .list-row').count()) === 1 && (await p.locator('#sresults [data-skind="playlist"] .list-row').count()) === 1, 'a person and your playlist are rows of their own');
    const facetText = await p.textContent('#sresults .facets');
    check(/Quality/.test(facetText) && /Lossless/.test(facetText) && /Years/.test(facetText) && /1990s/.test(facetText) && /2010s/.test(facetText), `facets are counted from the results (${facetText.replace(/\s+/g, ' ').trim().slice(0, 120)})`);
    check((await p.locator('#sresults .facets [data-facet^="service:pubky"]').count()) === 1 && (await p.locator('#sresults .facets [data-facet="service:people"], #sresults .facets [data-facet="service:posts"]').count()) === 0, 'people and posts count as Pubky: one Source chip for the three');
    await p.click('#sresults [data-facet="decade:2010s"]'); await p.waitForTimeout(200);
    const songsNow = await p.$$eval('#sresults [data-skind="song"] .srow .name', (els) => els.map((e) => e.textContent.trim()));
    check(songsNow.includes('New Song') && songsNow.includes('Shared Song') && !songsNow.includes('Old Song') && (await p.locator('#sresults [data-skind="album"]').count()) === 0, `a facet narrows every group (${songsNow.join(', ')})`);
    await p.click('#sresults [data-facet=""]'); await p.waitForTimeout(200);
    check((await p.$$eval('#sresults [data-skind="song"] .srow', (els) => els.length)) === 3, 'Clear brings them all back');
    check(await p.evaluate(() => !!document.activeElement && document.activeElement.matches('#sresults [data-facet]')), 'a facet chip keeps the keyboard focus across the re-render');
    check(await p.evaluate(() => { const s = document.querySelector('#search'); return s.getAttribute('role') === 'combobox' && s.getAttribute('aria-controls') === 'sresults' && s.getAttribute('aria-expanded') === 'true'; }), 'the search box is a combobox over the results');
    // the keyboard: ↓ ↓ walk the results, the box says where it is, Enter opens the person's page
    await p.focus('#search');
    await p.keyboard.press('ArrowDown'); await p.keyboard.press('ArrowDown');
    const k = await p.evaluate(() => { const el = document.querySelector('#sresults .kfocus'); return { has: !!el, id: el && el.id, active: document.querySelector('#search').getAttribute('aria-activedescendant'), focusStays: document.activeElement && document.activeElement.id === 'search' }; });
    check(k.has && k.id && k.active === k.id && k.focusStays, `↓ walks the results while the box keeps focus (${k.id})`);
    check(/\(2 of \d+\)/.test(await p.textContent('#sresults-said')), 'the active row is said politely');
    // typing drops the position: Enter then searches the new words, not the old row
    await p.keyboard.type('u');
    check(await p.evaluate(() => !document.querySelector('#search').hasAttribute('aria-activedescendant') && !document.querySelector('#sresults .kfocus')), 'typing drops the highlighted row');
    await p.keyboard.press('Backspace'); await p.waitForTimeout(400);
    await p.keyboard.press('ArrowDown'); await p.keyboard.press('ArrowDown');
    const n = await p.$$eval('#sresults .srow, #sresults .tile, #sresults [data-sother], #sresults .toprow', (els) => els.findIndex((e) => e.classList.contains('kfocus')));
    const total = await p.$$eval('#sresults .srow, #sresults .tile, #sresults [data-sother], #sresults .toprow', (els) => els.length);
    const personAt = await p.$$eval('#sresults .srow, #sresults .tile, #sresults [data-sother], #sresults .toprow', (els) => els.findIndex((e) => (e.dataset.sother || '').startsWith('person:')));
    for (let i = n; i < personAt; i++) await p.keyboard.press('ArrowDown');
    await p.keyboard.press('Enter');
    const path = await (async () => { const end = Date.now() + 5000; for (;;) { const v = await p.evaluate(() => location.pathname); if (v.startsWith('/person/')) return v; if (Date.now() > end) return v; await p.waitForTimeout(100); } })();
    check(path === `/person/${KEY}`, `Enter opens the row in focus (${path}; ${total} results)`);
    // tag: goes to the node as it is (the node reads it like #)
    await p.goto(BASE + '/'); await p.waitForSelector('#search');
    await p.fill('#search', 'tag:chill'); await p.press('#search', 'Enter'); await p.waitForSelector('#sresults [data-skind="song"]');
    check(seen.includes('tag:chill'), `tag:chill is sent as typed (${seen.join(' | ')})`);
    await p.unroute(/\/v1\/search\?/); await p.unroute(/\/v1\/search\/tags\?/);
    await c.close();

    // ---- About comes in the background: only About and the portrait are redrawn ----
    c = await ctx(); p = await c.newPage();
    let asked = 0;
    const later = { ...credited, id: 'lib-778', target: { artist_id: 778 }, title: 'Pending Test', blurb: null, about_text: null, image_credit: null, about_pending: true };
    await p.route(/\/v1\/item\/artist\/lib-778(\?.*)?$/, (r) => { asked++; return r.fulfill({ json: asked < 2 ? later : { ...later, about_pending: false, blurb: 'Pending Test is a band.', about_text: { text: 'Pending Test is a band.', source: 'Wikipedia', url: 'https://en.wikipedia.org/wiki/Pending_Test', lang: 'en', licence: 'CC BY-SA 4.0', licence_url: 'https://creativecommons.org/licenses/by-sa/4.0/' } } }); });
    await p.goto(BASE + '/artist/lib-778');
    check(await visible(p, '#main .hero'), 'the pending page shows');
    await p.evaluate(() => { document.querySelector('#main .hero').dataset.mark = 'same'; });
    check(await visible(p, '#iabout .blurb', 9000), 'About arrives by itself');
    check(await p.evaluate(() => document.querySelector('#main .hero')?.dataset.mark === 'same'), 'without drawing the page again');
    check(await p.evaluate(() => { const b = document.querySelector('#iblurbmore'); return !!document.querySelector('#iblurb.clamp') && !!b && typeof b.onclick === 'function'; }), 'the article that arrived is clamped, with its More button working');
    check(asked === 2, `the page is asked again once About is due (asked ${asked} times, want 2)`);
    await c.close();

    // ---- Home: Listening now (L3) ----
    if (nz) {
      await api('PUT', '/node/settings', { dev_outputs: true });
      c = await ctx(); p = await c.newPage();
      const K = (ch) => ch.repeat(52);
      const PRES = [
        { key: 'owner', you: true, mine: false, playing: true, title: 'My own song', zone: { id: nz.id, name: nz.name }, can_join: false, along: [] },
        { key: K('a'), name: 'Alice', you: false, mine: false, playing: true, title: 'Night Drive', artist: 'Synth Band', zone: { id: 'z-alice', name: 'Kitchen' }, can_join: true, along: [] },
        { key: K('b'), name: 'Bob', you: false, mine: false, playing: true, title: 'Hidden Song', zone: null, can_join: false, along: [] },
        { key: K('c'), name: 'Carol', you: false, mine: true, your_zone: nz.id, playing: true, title: 'Together', zone: { id: 'z-carol', name: 'Living room' }, can_join: true, along: [] },
      ];
      await p.route(/\/v1\/presence$/, (r) => r.fulfill({ json: PRES }));
      await p.route(/\/v1\/me\/presence$/, (r) => r.fulfill({ json: { visible: 'hidden' } }));
      let alongBody = null; let alongStatus = 409;
      // the node answers with the new group zone: their music, your speakers with theirs
      const GROUP = { ...nz, id: `group:${nz.output}+alice`, name: 'Kitchen + Null output', output: `group:${nz.output}+alice` };
      await p.route(/\/v1\/presence\/along$/, (r) => { alongBody = JSON.parse(r.request().postData() || '{}'); if (alongStatus !== 200) return r.fulfill({ status: alongStatus, json: { error: 'someone else is listening on that zone: listen along from yours' } }); PRES[1] = { ...PRES[1], mine: true, your_zone: nz.id }; return r.fulfill({ json: GROUP }); });
      await p.goto(BASE + '/'); await p.waitForSelector('#h-listening .list-row', { timeout: 15000 }).catch(() => {});
      const rows = await p.$$eval('#h-listening .list-row', (els) => els.map((e) => ({ name: e.querySelector('.name').textContent.replace(/\s+/g, ' ').trim(), sub: (e.querySelector('.sub') || {}).textContent || '', join: !!e.querySelector('[data-alongjoin]'), pill: (e.querySelector('.pill') || {}).textContent || '' })));
      check(rows.length === 3 && !rows.some((r) => /My own song/.test(r.name)), `Listening now lists the others, not you (${rows.length})`);
      check(rows[0] && /^Alice is listening to Night Drive · Synth Band$/.test(rows[0].name) && rows[0].sub === 'on Kitchen' && rows[0].join, `who, what and where, with Listen along (${rows[0] && rows[0].name} / ${rows[0] && rows[0].sub})`);
      check(rows[1] && /^Bob is listening/.test(rows[1].name) && !rows[1].sub && !rows[1].join, 'a zone not open to you: who, without a place or a button');
      check(rows[2] && rows[2].pill === 'Listening along' && !rows[2].join, 'the one you listen along with says so');
      // This browser can't join a group: Listen along asks which of your zones
      await p.click('[data-alongjoin="0"]'); await p.waitForSelector('.popover.menu:not([hidden]) [data-mi]', { timeout: 5000 }).catch(() => {});
      const menuText = await p.evaluate(() => (document.querySelector('.popover.menu:not([hidden])') || {}).textContent || '');
      check(/Listen from/.test(menuText) && menuText.includes(nz.name), `from This browser it asks which speakers listen along (${menuText.replace(/\s+/g, ' ').slice(0, 80)})`);
      await p.locator('.popover.menu:not([hidden]) [data-mi]', { hasText: nz.name }).first().click();
      const refusal = await (async () => { const end = Date.now() + 8000; for (;;) { const v = await p.textContent('#toast').catch(() => ''); if (/listen along from yours/i.test(v || '')) return v; if (Date.now() > end) return v; await p.waitForTimeout(150); } })();
      check(/someone else is listening on that zone/i.test(refusal || ''), `a refusal in the node's words (409: ${refusal})`);
      check(alongBody && alongBody.key === K('a') && alongBody.zone === nz.id, `the request names them and your zone (${JSON.stringify(alongBody && { zone: alongBody.zone })})`);
      check(await p.evaluate(() => { const b = document.querySelector('[data-alongjoin="0"]'); return !!b && !b.disabled && /Listen along with Alice/.test(b.getAttribute('aria-label')); }), 'after a refusal the button is back, named again');
      // and now it works: the page switches to the group and the row says you listen along
      alongStatus = 200;
      await p.click('[data-alongjoin="0"]'); await p.locator('.popover.menu:not([hidden]) [data-mi]', { hasText: nz.name }).first().click();
      const ok = await (async () => { const end = Date.now() + 8000; for (;;) { const v = await p.textContent('#toast').catch(() => ''); if (/Listening along with Alice/.test(v || '')) return true; if (Date.now() > end) return false; await p.waitForTimeout(150); } })();
      check(ok && (await p.textContent('#zonename')).trim() === GROUP.name, `joined: a toast and the group zone in use (${(await p.textContent('#zonename')).trim()})`);
      check(await waitText(p, '#h-listening .list-row:first-child .pill', 'Listening along'), 'the row says you listen along now');
      await p.unroute(/\/v1\/presence$/); await p.unroute(/\/v1\/me\/presence$/); await p.unroute(/\/v1\/presence\/along$/);
      await c.close();
      await api('PUT', '/node/settings', { dev_outputs: !!devBefore }).catch(() => {});
    }

    // ---- Log in / Log out (TASK-login-ux) ----
    {
      const OWNER_K = 'o'.repeat(52); const OTHER_K = 'b'.repeat(52);
      const QR = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#fff"/></svg>';
      let poll = { status: 'pending' }; let starts = 0; let logouts = 0;
      // A context whose token (if any) is set once, so a Log out or a reload doesn't bring it back
      const loginCtx = async (opts, token) => {
        const cx = await browser.newContext(opts);
        if (token) await cx.addInitScript((t) => { if (!sessionStorage.getItem('seeded')) { sessionStorage.setItem('seeded', '1'); localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); } }, token);
        await cx.route(/\/v1\/auth\/pubky\/start$/, (r) => { starts++; return r.fulfill({ json: { id: `flow${starts}`, url: 'pubkyauth:///?caps=/pub/pubky.app/:r&secret=c2VjcmV0&relay=https://relay.example/link', owner: OWNER_K, owner_face: { id: OWNER_K, name: 'Alice' }, node_name: 'CI node', renews: false, qr_svg: QR } }); });
        await cx.route(/\/v1\/auth\/pubky\/poll/, (r) => r.fulfill({ json: poll }));
        await cx.route(/\/v1\/me\/token$/, (r) => { logouts++; return r.continue(); });
        return cx;
      };
      const before = (p, a, b) => p.evaluate(([x, y]) => { const ea = document.querySelector(x); const eb = document.querySelector(y); return !!(ea && eb && (ea.compareDocumentPosition(eb) & Node.DOCUMENT_POSITION_FOLLOWING)); }, [a, b]);

      // Desktop: Log out from the account menu, back to the Log in screen with the QR code first
      c = await loginCtx({ viewport: { width: 1280, height: 900 } }, TOKEN); p = await c.newPage();
      await p.goto(BASE + '/');
      check(await visible(p, '#me', 20000), 'the account button is on the desktop shell');
      await p.waitForSelector('#main .section, #main .pagehead, #main h1', { timeout: 20000 }).catch(() => {});
      await p.click('#me');
      await p.locator('.popover.menu .pop-item', { hasText: 'Log out' }).click();
      // Polled: Log out reloads the page, which a single wait doesn't outlive
      const loginShown = async (ms) => { const end = Date.now() + ms; for (;;) { if (await p.isVisible('#loginpage').catch(() => false)) return true; if (Date.now() > end) return false; await p.waitForTimeout(200); } };
      check(await loginShown(15000), 'Log out shows the Log in screen');
      check(await waitText(p, '#loginwhy', 'You logged out.'), 'it says you logged out');
      check(logouts === 1, 'the node was asked to end this browser’s session');
      check(await p.evaluate(() => localStorage.getItem('pm.token') === null && localStorage.getItem('pm.zone') === null), 'the browser forgot the token and the account’s state');
      check(!(await p.isVisible('.shell')) && !(await p.isVisible('#np')), 'no half of the app behind it');
      check(await visible(p, '#loginbody .loginqr'), 'desktop: the QR code');
      check(await before(p, '#loginbody .loginqr', '#loginring') && !(await p.$eval('#loginring', (e) => e.classList.contains('primary'))), 'desktop: QR first, Open in Pubky Ring a link below');
      check(/^pubkyauth:/.test(await p.getAttribute('#loginring', 'href')), 'the link opens Pubky Ring');
      check((await p.textContent('#loginh')).trim() === 'Log in to CI node', 'the screen names the node');
      const hint = await p.textContent('#logintoken');
      check(/api-token/.test(hint) && /PM_TOKEN/.test(hint) && await p.locator('#logintoken a[href*="troubleshooting.md"]').count() === 1, 'the API token stays as the alternative, saying where it is');
      await p.click('#logintoken summary'); await p.fill('#tok', 'not-the-token'); await p.click('#tokform button[type=submit]');
      check(await waitText(p, '#tokerr', 'The node doesn\u2019t take that token.') && await p.isVisible('#loginpage'), 'a wrong token is said on the screen');
      await p.fill('#tok', TOKEN); await p.click('#tokform button[type=submit]');
      check(await p.locator('#loginpage').waitFor({ state: 'hidden', timeout: 15000 }).then(() => true).catch(() => false) && await visible(p, '#me'), 'the right token logs in');
      await c.close();

      // Phone (and tablet): Open in Pubky Ring first, the QR code on request
      for (const [w, h] of [[390, 844], [834, 1194]]) {
        c = await loginCtx({ viewport: { width: w, height: h }, isMobile: true, hasTouch: true }); p = await c.newPage();
        poll = { status: 'pending' };
        await p.goto(BASE + '/');
        check(await visible(p, '#loginring', 15000), `${w}: signed out, the Log in screen`);
        check(await p.$eval('#loginbody', (b) => b.firstElementChild && b.firstElementChild.id === 'loginring') && await p.$eval('#loginring', (e) => e.classList.contains('primary')), `${w}: Open in Pubky Ring is the main button, first`);
        check(!(await p.$('#loginbody .loginqr')), `${w}: no QR code until asked`);
        await p.click('#loginqrbtn');
        check(await visible(p, '#loginbody .loginqr'), `${w}: Show QR code shows it`);
        check(await p.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${w}: nothing wider than the screen`);
        if (w !== 390) { await c.close(); continue; }
        // Another identity approved: said plainly, with both people, and Try again
        poll = { status: 'denied', reason: 'not_owner', user: OTHER_K, approver: { id: OTHER_K, name: 'Bob' }, owner_face: { id: OWNER_K, name: 'Alice' } };
        check(await waitText(p, '#loginrefused', 'This node belongs to Alice. You approved as Bob. Pick Alice in Pubky Ring and try again.'), 'a wrong identity is named: whose node, who approved, what to do');
        check(await p.locator('#loginbody .loginwho .face').count() === 2 && await p.isVisible('#loginagain'), 'with both people and Try again');
        poll = { status: 'pending' }; const s0 = starts;
        await p.click('#loginagain');
        check(await visible(p, '#loginring') && starts === s0 + 1, 'Try again starts a new log in');
        poll = { status: 'denied', reason: 'no_access', user: OTHER_K, approver: { id: OTHER_K, name: 'Bob' }, owner_face: { id: OWNER_K, name: 'Alice' } };
        check(await waitText(p, '#loginrefused', 'Bob doesn’t have access to the music here. Ask Alice to give you access.'), 'someone without music access is told to ask the owner');
        poll = { status: 'pending' }; await p.click('#loginagain'); await p.waitForSelector('#loginring');
        poll = { status: 'ok', token: TOKEN, role: 'owner', user: OWNER_K };
        check(await p.locator('#loginpage').waitFor({ state: 'hidden', timeout: 15000 }).then(() => true).catch(() => false), 'an approval logs in by itself');
        check(await visible(p, '#me') && await p.evaluate((t) => localStorage.getItem('pm.token') === t, TOKEN), 'and the app is there, with the session kept');
        await c.close();
      }

      // A session that ended: the same Log in screen, saying so
      c = await loginCtx({ viewport: { width: 1280, height: 900 } }, 'pml_' + 'x'.repeat(43)); p = await c.newPage();
      poll = { status: 'pending' };
      await p.goto(BASE + '/');
      check(await visible(p, '#loginpage', 20000), 'an ended session brings the Log in screen');
      check(await waitText(p, '#loginwhy', 'Your session ended. Log in again.'), 'saying the session ended');
      check(await p.evaluate(() => localStorage.getItem('pm.token') === null), 'and the old token is gone');
      await c.close();

      // The node's own session lacks folders: a Log out line in Settings, no banner on Home
      c = await ctx(); p = await c.newPage();
      await c.route(/\/v1\/pubky$/, (r) => r.fulfill({ json: { user: 'pubky' + OWNER_K, unsynced: 0, needs_resign: true, missing: ['/pub/music/v1/'], notice: 'This node needs your approval for new folders on your homeserver. Log out and log in again with Pubky Ring to give it.', sync: null } }));
      await p.goto(BASE + '/settings/accounts');
      check(await visible(p, '#pkresign [data-resign]', 20000), 'Settings says the node needs approval, with Log out');
      check(!(await p.$('#pklogin')), 'no separate "Sign in again"');
      await p.goto(BASE + '/'); await p.waitForSelector('#h-setup', { timeout: 20000 }).catch(() => {}); await p.waitForTimeout(800);
      check(!(await p.$('#main .notice.resign')), 'Home has no sign-in banner');
      await c.close();
    }
  } finally {
    await api('PUT', '/node/settings', { first_steps_hidden: !!before.first_steps_hidden }).catch(() => {});
    await browser.close();
  }
  if (problems.length) { console.log(`${problems.length} failed`); process.exit(1); }
  console.log('setup ok');
})().catch((e) => { console.error(e); process.exit(1); });
