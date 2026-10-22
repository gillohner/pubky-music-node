// Real-browser checks for the one search (STEWARD-UX-HOME.md "Search"):
// scope chips and the URL, per-source streaming (time to first and to all results),
// merged rows, the progress bar, and a pasted link opening its item page.
//   node tools/ui-search.js <api token> [http://127.0.0.1:7788] [screenshot dir]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788'; const OUT = process.argv[4] || null;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const errors = [];
(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, colorScheme: 'dark' });
  await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
  const p = await ctx.newPage(); p.on('pageerror', (e) => errors.push('page error: ' + e.message));
  await p.goto(BASE + '/'); await sleep(1500);
  for (const q of ['queen', 'daft punk']) {
    await p.fill('#search', q); await p.press('#search', 'Enter');
    await sleep(150);
    const progressShown = await p.$eval('#sprog', (e) => !e.hidden).catch(() => false);
    await p.waitForFunction(() => document.querySelector('#sprog').hidden, null, { timeout: 40000 }).catch(() => {});
    const t = await p.evaluate(() => window.pmLastSearch);
    const chips = await p.$$eval('#scopechips .chip', (els) => els.map((e) => e.textContent.replace(/\s+/g, ' ').trim()));
    const sections = await p.$$eval('#sresults .section h3', (els) => els.map((e) => e.textContent));
    const merged = await p.$$eval('[data-skind="song"] .srow', (els) => els.filter((e) => e.querySelectorAll('.src-badges .src-badge').length > 1).length);
    console.log(`"${q}": first result ${t && t.first_ms} ms, all ${t && t.all_ms} ms | per source ${JSON.stringify(t && t.sources)} | progress bar ${progressShown ? 'shown' : 'not seen'} | sections ${sections.join(', ')} | merged song rows ${merged}`);
    console.log('   chips:', chips.join(' · '));
    if (!sections.length) errors.push(`no sections for "${q}"`);
    if (OUT && q === 'queen') await p.screenshot({ path: `${OUT}/search-everywhere.png`, fullPage: true });
  }
  // Scope chip → URL with in=, results only from that source.
  const chip = await p.$('#scopechips [data-scope-in="library"]');
  await chip.click(); await sleep(1200);
  const u = new URL(p.url());
  console.log('scope chip Library →', u.pathname + u.search);
  if (u.searchParams.get('in') !== 'library') errors.push('the scope is not in the URL');
  await p.goBack(); await sleep(1200);
  console.log('Back →', new URL(p.url()).search);
  // A pasted link opens its item page.
  await p.fill('#search', 'https://tidal.com/album/199574241'); await p.press('#search', 'Enter');
  await p.waitForSelector('#main #iabout', { timeout: 30000 }).catch(() => {});
  const h1 = await p.textContent('#main h1').catch(() => null);
  console.log('pasted TIDAL link →', new URL(p.url()).pathname, '|', h1);
  if (!/^\/album\/(u|tidal)-/.test(new URL(p.url()).pathname)) errors.push('a pasted link did not open its item page');
  if (OUT) await p.screenshot({ path: `${OUT}/search-pasted-link.png` });
  await browser.close();
  console.log('errors:', errors.length ? errors : 'none');
  process.exitCode = errors.length ? 1 : 0;
})();
