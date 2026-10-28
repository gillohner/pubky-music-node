// Page and search speed on a big library (steward next-3, item 1).
// For each page: first paint, when its content shows ("ready"), when the browser is next idle
// after that ("interactive"), the JSON it downloaded and the size of the page (DOM nodes).
// Search: from the last key typed to the library's first hit on screen (the box waits 250 ms
// after the last key before it searches; that wait is included), and the node's own time.
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-perf.js <api token> [http://127.0.0.1:7788] [--budget tools/perf-budget.json] [--runs 3]
// With --budget, exits 1 when a number is over its budget (CI: the 5,000-track fixture).
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
// Waits until fn(arg) is truthy in the page and returns its value. Playwright's waitForFunction rebuilds the
// predicate with new Function inside the page, which the node's Content-Security-Policy forbids, so this polls
// through evaluate instead.
const waitFor = async (page, fn, arg = null, timeout = 30000) => { const end = Date.now() + timeout; for (;;) { const v = await page.evaluate(fn, arg).catch(() => null); if (v) return v; if (Date.now() > end) throw new Error('timed out waiting in the page'); await new Promise((r) => setTimeout(r, 100)); } };
const args = process.argv.slice(2);
const opt = (name, dflt) => { const i = args.indexOf(name); return i >= 0 ? args.splice(i, 2)[1] : dflt; };
const budgetFile = opt('--budget', null); const RUNS = Number(opt('--runs', 3));
const TOKEN = args[0]; const BASE = args[1] || 'http://127.0.0.1:7788';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const median = (xs) => { const s = [...xs].sort((a, b) => a - b); return s[Math.floor(s.length / 2)]; };

const api = async (path) => { const r = await fetch(BASE + '/v1' + path, { headers: { authorization: 'Bearer ' + TOKEN } }); if (!r.ok) throw new Error(`${path}: ${r.status}`); return r.json(); };

// Content that means "this page has shown what it is for".
const PAGES = async () => {
  const albums = await api('/library/albums?limit=1'); const artists = await api('/library/artists?limit=1');
  const artist = Array.isArray(artists) ? artists[0] : (artists.artists || [])[0];
  return [
    ['Home', '/', '#h-played .hbody .tile, #h-played .hbody p, #h-added .hbody .tile'],
    ['Albums', '/albums', '.grid .tile'],
    ['Artists', '/artists', '.list .list-row'],
    ['Tracks', '/tracks', '#tracklist .track, #tracklist [data-key], #tracklist > *'],
    ['Album page', `/album/lib-${albums[0].id}`, 'main h1'],
    ['Artist page', `/artist/lib-${artist.id}`, 'main h1'],
  ];
};

const measure = async (p, path, selector) => {
  await p.goto('about:blank');
  const t0 = Date.now();
  await p.goto(BASE + path, { waitUntil: 'commit' });
  await p.waitForSelector(`main :is(${selector})`, 60000).catch(() => null);
  // Ready: the selector is on screen; then the first idle moment of the main thread.
  return p.evaluate(async () => {
    const nav = performance.getEntriesByType('navigation')[0];
    const ready = performance.now();
    await new Promise((r) => requestIdleCallback(r, 30000));
    const idle = performance.now();
    const fcp = (performance.getEntriesByName('first-contentful-paint')[0] || {}).startTime || null;
    const json = performance.getEntriesByType('resource').filter((e) => e.name.includes('/v1/')).reduce((n, e) => n + (e.encodedBodySize || 0), 0);
    return { fcp: Math.round(fcp), ready: Math.round(ready - (nav ? nav.startTime : 0)), interactive: Math.round(idle), json_kb: Math.round(json / 1024), dom: document.getElementsByTagName('*').length };
  }).then((r) => ({ ...r, wall: Date.now() - t0 }));
};

(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
  await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);
  // Pubky and services are not what is measured: keep their slow answers out of the way.
  await ctx.route(/\/v1\/(home\/|discover\/|listenbrainz)/, (r) => r.fulfill({ status: 200, contentType: 'application/json', body: '{"items":[],"tags":[]}' }));
  const p = await ctx.newPage();
  const out = { pages: {}, search: {} };
  for (const [name, path, sel] of await PAGES()) {
    const runs = [];
    for (let i = 0; i < RUNS; i++) runs.push(await measure(p, path, sel));
    const pick = (k) => median(runs.map((r) => r[k]));
    out.pages[name] = { fcp_ms: pick('fcp'), ready_ms: pick('ready'), interactive_ms: pick('interactive'), json_kb: pick('json_kb'), dom_nodes: pick('dom') };
  }
  // Search as you type, library only (other sources are other people's servers)
  const words = ['amber', 'night riv', 'golden', 'café', 'storm 12'];
  const typed = []; const node = [];
  for (const w of words) {
    await p.goto(BASE + '/'); await p.waitForSelector('#search'); await p.focus('#search');
    await p.evaluate(() => { window.pmLastSearch = null; });
    for (const ch of w) await p.keyboard.type(ch, { delay: 40 });
    const t = Date.now();
    await p.waitForSelector('#sresults #stop', 15000).catch(() => null);
    typed.push(Date.now() - t);
    const ms = await waitFor(p, (q) => window.pmLastSearch && window.pmLastSearch.q === q && window.pmLastSearch.sources.library, w, 15000).catch(() => null);
    node.push(ms);
  }
  out.search = { after_last_key_ms: median(typed), node_ms: median(node), node_max_ms: Math.max(...node) };
  await browser.close();
  console.log(JSON.stringify(out, null, 1));
  if (budgetFile) {
    const budget = JSON.parse(require('fs').readFileSync(budgetFile, 'utf8'));
    const over = [];
    for (const [page, lim] of Object.entries(budget.pages || {})) for (const [k, v] of Object.entries(lim)) { const got = out.pages[page] && out.pages[page][k]; if (got == null || got > v) over.push(`${page} ${k}: ${got} > ${v}`); }
    for (const [k, v] of Object.entries(budget.search || {})) if (out.search[k] > v) over.push(`search ${k}: ${out.search[k]} > ${v}`);
    for (const o of over) console.log('SLOWER ' + o);
    if (over.length) process.exit(1);
    console.log('within the speed budget');
  }
})();
