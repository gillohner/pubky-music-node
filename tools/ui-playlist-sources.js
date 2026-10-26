// Real-browser check for where playlist songs play from (STEWARD-UX-BRIEF §3):
//   * every row has a source chip, the summary bar counts and filters by source;
//   * a chip opens the song's sources plus "Auto"; picking one pins it (pin mark), Auto unpins;
//   * the playlist's "Play from" order moves with the arrow buttons and Alt+arrows, and
//     "Use my node's order" goes back to the global one; Settings has the global order.
// Uses the first playlist that has songs and puts every change back. Skips without one.
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-playlist-sources.js <api token> [http://127.0.0.1:7788] [screenshot dir]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
// A checkbox's state, waiting through a re-render that briefly replaces it
const checkedOf = async (p, sel) => { for (let k = 0; k < 15; k++) { const v = await p.evaluate((x) => { const c = document.querySelector(x); return c ? c.checked : null; }, sel); if (v !== null) return v; await p.waitForTimeout(200); } return null; };
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788'; const OUT = process.argv[4] || null;
const errors = []; const fails = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) fails.push(what); };
const api = async (method, path, body) => { const r = await fetch(BASE + '/v1' + path, { method, headers: { Authorization: 'Bearer ' + TOKEN, 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined }); return r.status === 204 ? null : r.json(); };
(async () => {
  const lists = await api('GET', '/playlists');
  const pl = (lists || []).find((p) => p.items > 0);
  if (!pl) { console.log('skip: no playlist with songs on this node'); return; }
  const before = await api('GET', `/playlists/${pl.id}`);
  const pf0 = await api('GET', `/playlists/${pl.id}/play-from`); // warm the lookups
  const globalBefore = (await api('GET', '/play-order')).order;
  const browser = await chromium.launch();
  try {
    for (const width of [1280, 390]) {
      const ctx = await browser.newContext({ viewport: { width, height: width < 600 ? 844 : 800 }, colorScheme: 'dark' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
      const p = await ctx.newPage();
      p.on('pageerror', (e) => errors.push('page error: ' + e.message));
      await p.goto(`${BASE}/playlist/${pl.id}`);
      await p.waitForSelector('#plsongs [data-psrc]', { timeout: 60000 });
      const rows = await p.$$eval('#plsongs .srow', (r) => r.length);
      const chips = await p.$$eval('#plsongs [data-psrc]', (r) => r.length);
      check(rows === pl.items && chips === rows, `${width}: every row has a source chip (${chips}/${rows})`);
      const summary = await p.$$eval('.plsummary [data-pfilter]', (b) => b.map((x) => x.textContent.trim()));
      check(summary.length >= 2 && summary[0].startsWith(String(pl.items)), `${width}: summary bar ${JSON.stringify(summary)}`);
      if (OUT) await p.screenshot({ path: `${OUT}/playlist-sources-${width}-dark.png` });
      if (width === 390) { await ctx.close(); continue; }
      // filter by the second summary chip
      await p.click('.plsummary [data-pfilter]:nth-child(2)'); await sleep(400);
      const shown = await p.$$eval('#plsongs .srow:not(.filtered)', (r) => r.length);
      const n = Number((summary[1].match(/\d+/) || [0])[0]);
      check(shown === n, `filter shows ${shown} of ${rows} (expected ${n})`);
      await p.click('.plsummary [data-pfilter=""]'); await sleep(400);
      // pin the first available song through its chip
      const k = pf0.items.findIndex((i) => i.plays_from && i.sources.filter((x) => x.available).length >= 1);
      if (k >= 0) {
        await p.click(`[data-psrc="${k}"]`); await sleep(300);
        const menu = await p.$$eval('.popover.menu .pop-item span', (e) => e.map((x) => x.firstChild.textContent));
        check(menu[0] === 'Auto (follow the playlist)' && menu.length >= 2, `chip menu: ${JSON.stringify(menu)}`);
        if (OUT) await p.screenshot({ path: `${OUT}/playlist-source-menu-1280-dark.png` });
        await p.click('.popover.menu .pop-item:nth-of-type(2)'); await p.waitForSelector(`[data-psrc="${k}"].pinned`, { timeout: 30000 }).catch(() => {});
        check(!!(await p.$(`[data-psrc="${k}"].pinned svg use[href="#i-pin"]`)), 'picking a source pins it (pin mark on the chip)');
        await p.click(`[data-psrc="${k}"]`); await sleep(300); await p.click('.popover.menu .pop-item:nth-of-type(1)');
        await p.waitForFunction((kk) => { const c = document.querySelector(`[data-psrc="${kk}"]`); return c && !c.classList.contains('pinned'); }, k, { timeout: 30000 }).catch(() => {});
        check(!(await p.$(`[data-psrc="${k}"].pinned`)), 'Auto unpins it');
      }
      // the playlist's own order
      const orderOf = () => p.$$eval('#plorder .porder-row .grow', (e) => e.map((x) => x.firstChild.textContent.trim()));
      const o1 = await orderOf();
      await p.click('#plorder [data-odown="0"]'); await p.waitForFunction((first) => { const r = document.querySelector('#plorder .porder-row .grow'); return r && r.firstChild.textContent.trim() !== first; }, o1[0], { timeout: 30000 }).catch(() => {});
      const o2 = await orderOf();
      check(o2[0] === o1[1] && o2[1] === o1[0] && !(await checkedOf(p, '#plownorder')), `arrow button moves it: ${o1.join('›')} → ${o2.join('›')}, own order`);
      await p.focus('#plorder [data-ogrip="1"]'); await p.keyboard.press('Alt+ArrowUp');
      await p.waitForFunction((first) => { const r = document.querySelector('#plorder .porder-row .grow'); return r && r.firstChild.textContent.trim() === first; }, o1[0], { timeout: 30000 }).catch(() => {});
      check((await orderOf())[0] === o1[0], 'Alt+↑ on the grip moves it back');
      if (OUT) { await p.$eval('#plorder', (e) => e.scrollIntoView({ block: 'center' })); await sleep(200); await p.screenshot({ path: `${OUT}/playlist-play-from-1280-dark.png` }); }
      await p.click('#plownorder'); await p.waitForFunction(() => document.querySelector('#plownorder') && document.querySelector('#plownorder').checked, null, { timeout: 30000 }).catch(() => {});
      check(await checkedOf(p, '#plownorder'), '"Use my node’s order" goes back to the global order');
      // global order in Settings
      await p.goto(`${BASE}/settings/playback`); await p.waitForSelector('#gorder .porder-row', { timeout: 20000 });
      const g = await p.$$eval('#gorder .porder-row', (e) => e.length);
      check(g === globalBefore.length, `Settings lists the global order (${g})`);
      if (OUT) { await p.$eval('#gordercard', (e) => e.scrollIntoView({ block: 'center' })); await sleep(200); await p.screenshot({ path: `${OUT}/settings-play-from-1280-dark.png` }); }
      // the import dialog asks once (opened and cancelled: nothing is imported)
      for (const svc of ['tidal', 'spotify']) {
        await p.goto(`${BASE}/source/${svc}`); await p.waitForSelector(`[data-${svc[0]}pl]`, { timeout: 20000 }).catch(() => {});
        const btn = await p.$(`[data-${svc[0]}pl]`);
        if (!btn) { console.log(`skip: no ${svc} playlists to import here`); continue; }
        const before = (await api('GET', '/playlists')).length;
        await btn.click(); await p.waitForSelector('dialog[open] .choice', { timeout: 10000 }).catch(() => {});
        const choices = await p.$$eval('dialog[open] .choice-label', (e) => e.map((x) => x.textContent));
        check(choices.length === 2 && choices[0].startsWith('Keep ') && choices[1] === 'Auto', `${svc} import asks: ${JSON.stringify(choices)}`);
        if (OUT) await p.screenshot({ path: `${OUT}/import-dialog-${svc}-1280-dark.png` });
        await p.keyboard.press('Escape'); await sleep(300);
        check(!(await p.$('dialog[open]')) && (await api('GET', '/playlists')).length === before, `${svc}: Esc cancels without importing`);
        break;
      }
      await ctx.close();
    }
  } finally {
    // put everything back
    await api('PUT', `/playlists/${pl.id}/order`, { order: before.playlist.source_order || null });
    for (const [k, it] of before.playlist.items.entries()) if (it.source) await api('PUT', `/playlists/${pl.id}/items/${k}/source`, it.source);
    await api('PUT', '/play-order', { order: globalBefore });
    await browser.close();
  }
  for (const e of errors) console.log(e);
  if (fails.length || errors.length) { console.log(`${fails.length} failed, ${errors.length} page errors`); process.exit(1); }
  console.log('all passed');
})();
