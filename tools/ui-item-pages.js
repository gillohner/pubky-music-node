// Real-browser checks for the one item page (docs/ux/08-item-page.md):
//   * feature matrix: every kind × source renders all six sections, the hero actions and the full ⋯ menu;
//   * click path: search → artist → album → song → Back ×3 lands on the results with query and scroll;
//   * deep link to an album → Back goes to its section;
//   * route table: routeFromPath(pathFor(v, arg)) round-trips for every view with a path.
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-item-pages.js <api token> [http://127.0.0.1:7788] [screenshot dir]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788'; const OUT = process.argv[4] || null;
const b64u = (x) => Buffer.from(x).toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const errors = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, colorScheme: 'dark' });
  await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
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
    const sections = await p.$$eval('#isongs, #iversions, #ipubky, #iyours, #iabout', (els) => els.map((e) => e.id));
    const hero = await p.$$eval('#iplay, #izone, #ifav, #imore', (els) => els.map((e) => e.id));
    let menu = [];
    const more = await p.$('#imore'); if (more) { await more.click(); await sleep(250); menu = await p.$$eval('.popover.menu .pop-item span', (els) => els.map((e) => e.textContent)); await p.keyboard.press('Escape'); await p.mouse.click(5, 5); }
    const title = await p.textContent('#main h1').catch(() => null);
    const songs = await p.$$eval('#isongs .srow', (els) => els.length).catch(() => 0);
    const ok = sections.length === 5 && hero.length === 4 && ['Play next', 'Add to queue', 'Copy link'].every((m) => menu.includes(m));
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}: "${title}" | sections ${sections.length + 1}/6 | hero ${hero.join(',')} | songs ${songs} | menu: ${menu.join(' · ')}`);
    if (!ok) errors.push(`${name} misses sections or actions`);
    if (OUT) await p.screenshot({ path: `${OUT}/item-${name.replace(/[^a-z0-9]+/gi, '-').toLowerCase()}.png`, fullPage: true });
  }
  // ---- click path: search → artist → album → song → Back ×3 ----
  await p.goto(BASE + '/'); await sleep(1500);
  await p.fill('#search', 'radiohead'); await p.press('#search', 'Enter');
  await p.waitForSelector('#main [data-src="tidal"][data-sartist]', { timeout: 40000 }).catch(() => {}); await sleep(1500);
  // Wait until every source has answered (the page is full height), then scroll.
  await p.waitForFunction(() => !/Asking your streaming services/.test(document.querySelector('#main').textContent), null, { timeout: 40000 }).catch(() => {});
  await sleep(500);
  await p.evaluate(() => { document.querySelector('#main').scrollTop = 180; });
  await sleep(300);
  const scroll0 = await p.evaluate(() => document.querySelector('#main').scrollTop);
  const path0 = new URL(p.url()); const steps = [path0.pathname + path0.search];
  const artistHit = await p.$('#main [data-src="tidal"][data-sartist]');
  if (!artistHit) errors.push('no TIDAL artist in the search results');
  else {
    await artistHit.click(); await waitPage(); steps.push(new URL(p.url()).pathname);
    const back1 = (await p.textContent('#main .back')).trim();
    const tile = await p.$('#ialbums [data-tile]'); if (tile) { await tile.click(); await waitPage(); steps.push(new URL(p.url()).pathname); }
    const back2 = (await p.textContent('#main .back')).trim();
    const song = await p.$('#isongs [data-ssong]'); if (song) { await song.click(); await waitPage(); steps.push(new URL(p.url()).pathname); }
    const back3 = (await p.textContent('#main .back')).trim();
    console.log('click path:', steps.join(' → '));
    console.log('back labels:', [back1, back2, back3].join(' | '));
    for (let k = 0; k < 3; k++) { await p.click('#main .back'); await sleep(2500); }
    await sleep(2500);
    const end = new URL(p.url()); const scroll1 = await p.evaluate(() => document.querySelector('#main').scrollTop); const q = await p.inputValue('#search');
    console.log('after Back ×3:', end.pathname + end.search, '| query', q, '| scroll', scroll0, '→', scroll1);
    if (end.pathname !== '/search' || q !== 'radiohead') errors.push('Back ×3 did not land on the search results with the query');
    if (Math.abs(scroll1 - scroll0) > 40) errors.push(`scroll not restored (${scroll0} → ${scroll1})`);
    if (!/Results for/.test(back1)) errors.push(`artist back label is "${back1}"`);
  }
  // ---- deep link: Back goes to the section ----
  const dl = await ctx.newPage(); await dl.goto(BASE + '/album/lib-1'); await dl.waitForSelector('#main #iabout', { timeout: 20000 }).catch(() => {});
  const label = (await dl.textContent('#main .back')).trim(); await dl.click('#main .back'); await sleep(1500);
  console.log('deep link back:', label, '→', new URL(dl.url()).pathname);
  if (new URL(dl.url()).pathname !== '/albums') errors.push('deep-link Back did not go to /albums');
  // ---- route table ----
  const rt = await p.evaluate(() => window.pmRoutes && window.pmRoutes.check());
  console.log('route table:', rt ? rt.join(' | ') : 'not exposed');
  if (!rt || rt.some((x) => x.startsWith('FAIL'))) errors.push('route table does not round-trip');
  await browser.close();
  console.log('errors:', errors.length ? errors : 'none');
  process.exitCode = errors.length ? 1 : 0;
})();
