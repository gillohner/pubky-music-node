// Real-browser checks for the one item page (docs/ux/08-item-page.md):
//   * feature matrix: every kind × source renders all six sections, the hero actions and the full ⋯ menu;
//   * click path: search → artist → album → song → Back ×3 lands on the results with query and scroll;
//   * deep link to an album → Back goes to its section;
//   * route table: routeFromPath(pathFor(v, arg)) round-trips for every view with a path;
//   * direct load: every kind of page id opened fresh (a bookmark, a shared link, a reload) asks
//     /v1/item and renders, never the Albums skeleton (steward review, 2026-10-22).
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-item-pages.js <api token> [http://127.0.0.1:7788] [screenshot dir]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
// Waits until fn(arg) is truthy in the page and returns its value. Playwright's waitForFunction rebuilds the
// predicate with new Function inside the page, which the node's Content-Security-Policy forbids, so this polls
// through evaluate instead.
const waitFor = async (page, fn, arg = null, timeout = 30000) => { const end = Date.now() + timeout; for (;;) { const v = await page.evaluate(fn, arg).catch(() => null); if (v) return v; if (Date.now() > end) throw new Error('timed out waiting in the page'); await new Promise((r) => setTimeout(r, 100)); } };
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788'; const OUT = process.argv[4] || null;
const b64u = (x) => Buffer.from(x).toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const errors = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, colorScheme: 'dark' });
  await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
  // The pages run under the node's Content-Security-Policy: anything it refuses is a bug
  ctx.on('console', (m) => { if (m.type() === 'error' && /Content Security Policy/i.test(m.text())) errors.push('CSP: ' + m.text()); });
  const p = await ctx.newPage();
  p.on('pageerror', (e) => errors.push('page error: ' + e.message));
  const waitPage = async () => { await p.waitForSelector('#main #iabout, #main .notice', { timeout: 30000 }).catch(() => {}); await sleep(300); };
  // ---- feature matrix ----
  const pages = [
    ['library album', '/album/lib-1'],
    ['library artist', '/artist/lib-1'],
    ['TIDAL album (HYDRA 3D)', '/album/u-' + b64u('https://tidal.com/album/199574241')],
    ['TIDAL artist (Radiohead)', '/artist/u-' + b64u('https://tidal.com/artist/64518')],
    ['Deezer album', '/album/u-' + b64u('https://www.deezer.com/album/41839661')],
    ['Pubky-only link (Apple Music)', '/album/u-' + b64u('https://music.apple.com/au/album/1874125260')],
  ];
  // a library song: the first track of the library album
  await p.goto(BASE + '/album/lib-1'); await waitPage();
  const songId = await p.$eval('#isongs [data-ssong]', (a) => a.textContent).catch(() => null);
  if (songId) { await p.click('#isongs [data-ssong]'); await waitPage(); pages.push(['library song', new URL(p.url()).pathname]); }
  for (const [name, path] of pages) {
    await p.goto(BASE + path); await waitPage();
    // Song pages have no one-song list; every page has Tags and About
    const isSong = path.startsWith('/song/');
    const sections = await p.$$eval('#isongs, #itags, #iabout', (els) => els.map((e) => e.id));
    const want = isSong ? 2 : 3;
    const hero = await p.$$eval('#iplay, #izone, #ifav, #imore', (els) => els.map((e) => e.id));
    let menu = [];
    const more = await p.$('#imore'); if (more) { await more.click(); await sleep(250); menu = await p.$$eval('.popover.menu .pop-item span', (els) => els.map((e) => e.textContent)); await p.keyboard.press('Escape'); await p.mouse.click(5, 5); }
    const title = await p.textContent('#main h1').catch(() => null);
    const songs = await p.$$eval('#isongs .srow', (els) => els.length).catch(() => 0);
    const ok = sections.length === want && hero.length === 4 && ['Play next', 'Add to queue', 'Copy link'].every((m) => menu.includes(m));
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}: "${title}" | sections ${sections.join(',')} | hero ${hero.join(',')} | songs ${songs} | menu: ${menu.join(' · ')}`);
    if (!ok) errors.push(`${name} misses sections or actions`);
    if (OUT) await p.screenshot({ path: `${OUT}/item-${name.replace(/[^a-z0-9]+/gi, '-').toLowerCase()}.png`, fullPage: true });
  }
  // ---- click path: search → artist → album → song → Back ×3 ----
  await p.goto(BASE + '/'); await sleep(1500);
  await p.fill('#search', 'radiohead'); await p.press('#search', 'Enter');
  await p.waitForSelector('#main [data-skind="artist"] [data-tile]', { timeout: 40000 }).catch(() => {}); await sleep(1500);
  // Wait until every source has answered (the page is full height), then scroll.
  await waitFor(p, () => document.querySelector('#sprog') && document.querySelector('#sprog').hidden, null, 40000).catch(() => {});
  await sleep(500);
  await p.evaluate(() => { document.querySelector('#main').scrollTop = 180; });
  await sleep(300);
  const scroll0 = await p.evaluate(() => document.querySelector('#main').scrollTop);
  const path0 = new URL(p.url()); const steps = [path0.pathname + path0.search];
  const tiles = await p.$$('#main [data-skind="artist"] [data-tile]');
  let artistHit = null; for (const tl of tiles) { if (/radiohead/i.test(await tl.textContent())) { artistHit = tl; break; } }
  if (!artistHit) errors.push('no Radiohead artist tile in the search results');
  else {
    // The position the page has when it is left (the click scrolls the tile into view).
    await artistHit.scrollIntoViewIfNeeded(); await sleep(200);
    const scrollLeft = await p.evaluate(() => document.querySelector('#main').scrollTop);
    await artistHit.click(); await waitPage(); steps.push(new URL(p.url()).pathname);
    const back1 = (await p.textContent('#main .back')).trim();
    const tile = await p.$('#ialbums [data-tile]'); if (tile) { await tile.click(); await waitPage(); steps.push(new URL(p.url()).pathname); }
    const back2 = (await p.textContent('#main .back')).trim();
    const song = await p.$('#isongs [data-ssong]'); if (song) { await song.click(); await waitPage(); steps.push(new URL(p.url()).pathname); }
    const back3 = (await p.textContent('#main .back')).trim();
    console.log('click path:', steps.join(' → '));
    console.log('back labels:', [back1, back2, back3].join(' | '));
    // Back, or on wide screens the breadcrumb standing in for it
    for (let k = 0; k < 3; k++) { await p.click('#main .back:not(.dupback), #main [data-as-back]'); await sleep(2500); }
    await sleep(2500);
    const end = new URL(p.url()); const scroll1 = await p.evaluate(() => document.querySelector('#main').scrollTop); const q = await p.inputValue('#search');
    console.log('after Back ×3:', end.pathname + end.search, '| query', q, '| scroll when left', scrollLeft, '→ restored', scroll1);
    if (end.pathname !== '/search' || q !== 'radiohead') errors.push('Back ×3 did not land on the search results with the query');
    if (Math.abs(scroll1 - scrollLeft) > 40) errors.push(`scroll not restored (${scrollLeft} → ${scroll1})`);
    if (!/Results for/.test(back1)) errors.push(`artist back label is "${back1}"`);
  }
  // ---- deep link: Back goes to the parent (album → its artist, else the section) ----
  const dl = await ctx.newPage(); await dl.goto(BASE + '/album/lib-1'); await dl.waitForSelector('#main #iabout', { timeout: 20000 }).catch(() => {});
  const crumb = await dl.$$eval('#main .crumbs li', (e) => e.map((x) => x.textContent.trim())).catch(() => []);
  const label = (await dl.textContent('#main .back')).trim(); await dl.click('#main .back:not(.dupback), #main [data-as-back]'); await sleep(1500);
  const landed = new URL(dl.url()).pathname;
  console.log('deep link back:', label, '→', landed, '| breadcrumb', crumb.join(' › '));
  if (crumb.length >= 2 ? !landed.startsWith('/artist/') : landed !== '/albums') errors.push(`deep-link Back went to ${landed}`);
  if (crumb.length >= 2 && label !== crumb[0]) errors.push(`back label "${label}" is not the parent "${crumb[0]}"`);
  // ---- half-star rating (1–10): hover previews, a click saves, the same click clears ----
  const rp = await ctx.newPage(); await rp.goto(BASE + '/song/mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69');
  if (await rp.waitForSelector('#stars', { timeout: 20000 }).then(() => true).catch(() => false)) {
    await rp.hover('[data-rate="7"]');
    const fills = await rp.$$eval('#stars .star', (s) => s.map((x) => x.className.replace('star ', '')).join(','));
    await rp.click('[data-rate="7"]');
    const saved = await waitFor(rp, () => (document.querySelector('#ratingval') || {}).textContent === '3½ of 5', null, 10000).then(() => true).catch(() => false);
    await rp.click('[data-rate="7"]');
    const cleared = await waitFor(rp, () => (document.querySelector('#ratingval') || {}).textContent === '', null, 10000).then(() => true).catch(() => false);
    console.log('rating: hover', fills, '| saved 3½', saved, '| cleared', cleared);
    if (fills !== 'f2,f2,f2,f1,f0' || !saved || !cleared) errors.push('half-star rating');
  } else console.log('rating: skipped (no rating row on this node)');
  // ---- back closes an open menu and stays on the page ----
  if (await rp.$('#imore')) {
    const at = rp.url();
    await rp.click('#imore'); await rp.waitForSelector('.popover.menu:not([hidden])', { timeout: 5000 }).catch(() => {});
    await rp.goBack(); await sleep(600);
    const open = await rp.$('.popover.menu:not([hidden])');
    console.log('back with a menu open: closed', !open, '| same page', rp.url() === at);
    if (open || rp.url() !== at) errors.push('back did not just close the menu');
  }
  await rp.close();

  // ---- direct load: each kind of page id, fresh, as from a bookmark or a reload ----
  const firstTrack = await p.evaluate(async (t) => { try { const r = await fetch('/v1/library/tracks?limit=1', { headers: { authorization: 'Bearer ' + t } }); const j = await r.json(); return j.tracks[0] && j.tracks[0].id; } catch { return null; } }, TOKEN);
  const direct = [
    ...(firstTrack != null ? [`/song/lib-${firstTrack}`] : []),
    '/song/tidal-534050211', '/song/deezer-3135556', '/song/spotify-4u7EnebtmKWzUH433cf5Qv', '/song/youtube-bSnlKl_PoQU',
    '/song/isrc-GBUM71029604', '/song/mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69',
    '/album/lib-1', '/album/tidal-199574241', '/album/deezer-302127',
    '/artist/lib-1', '/artist/deezer-27', '/artist/tidal-64518',
  ];
  for (const path of direct) {
    const dp = await ctx.newPage();
    dp.on('pageerror', (e) => errors.push(`${path}: page error: ${e.message}`));
    let asked = false; dp.on('request', (r) => { if (new URL(r.url()).pathname.startsWith('/v1/item/')) asked = true; });
    await dp.goto(BASE + path);
    await dp.waitForSelector('#main #iabout, #main .notice', { timeout: 30000 }).catch(() => {});
    const rendered = !!(await dp.$('#main #iabout, #main .notice'));
    const title = await dp.textContent('#main h1').catch(() => null);
    const stayed = new URL(dp.url()).pathname === path;
    const ok = asked && rendered && stayed;
    console.log(`${ok ? 'ok  ' : 'FAIL'} direct ${path}: ${asked ? 'asked /v1/item' : 'never asked /v1/item'} | ${rendered ? `"${title}"` : 'not rendered'}${stayed ? '' : ` | moved to ${new URL(dp.url()).pathname}`}`);
    if (!ok) errors.push(`direct load of ${path}`);
    await dp.close();
  }

  // ---- links from data are web links only (security review 2026-10-27, #7) ----
  // The node already drops a javascript: link; the page must not render one either, so the answer is
  // changed on its way to the page.
  {
    const lp = await ctx.newPage();
    let ran = false; await lp.exposeFunction('pmScriptLinkRan', () => { ran = true; });
    await lp.route('**/v1/item/**', async (route) => {
      const r = await route.fetch(); const j = await r.json();
      j.about = [...(j.about || []), { label: 'Homepage', value: 'evil', href: 'javascript:window.pmScriptLinkRan()' }, { label: 'Site', value: 'band', href: 'https://band.example/' }];
      await route.fulfill({ response: r, json: j });
    });
    await lp.goto(BASE + '/album/lib-1'); await lp.waitForSelector('#main #iabout', { timeout: 30000 }).catch(() => {});
    const hrefs = await lp.$$eval('#iabout a', (as) => as.map((a) => a.getAttribute('href') || ''));
    const bad = hrefs.filter((h) => !/^https?:/i.test(h));
    const evil = await lp.$('#iabout a:text("evil")'); if (evil) { await evil.click({ modifiers: [] }).catch(() => {}); await sleep(300); }
    console.log('about links:', hrefs.join(' | '), '| a script link ran:', ran);
    if (!hrefs.includes('https://band.example/')) errors.push('a web link in About did not render');
    if (bad.length || ran) errors.push(`About rendered a link that isn't http(s): ${bad.join(', ')}`);
    await lp.close();
  }
  // ---- route table ----
  const rt = await p.evaluate(() => window.pmRoutes && window.pmRoutes.check());
  console.log('route table:', rt ? rt.join(' | ') : 'not exposed');
  if (!rt || rt.some((x) => x.startsWith('FAIL'))) errors.push('route table does not round-trip');
  await browser.close();
  console.log('errors:', errors.length ? errors : 'none');
  process.exitCode = errors.length ? 1 : 0;
})();
