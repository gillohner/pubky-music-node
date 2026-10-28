// A library FLAC always plays in "This browser" (STEWARD-UX-INSTANCES item 0: one did not while a
// big copy filled the node's uplink and the proxy in front answered with an error page).
//   * a library FLAC (the first on the Tracks page) plays in the browser and the time moves;
//   * when the node (or a proxy in front of it) answers the file with an error, the page says
//     what came back and tries again, and the song plays.
// The copy job itself is covered by tests/testnet_sync.rs (other calls and a library file stay
// fast while 400 MB are copied, and the copy keeps its pace).
//   node tools/ui-playback.js <api token> [http://127.0.0.1:7788]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
// Waits until fn(arg) is truthy in the page and returns its value. Playwright's waitForFunction rebuilds the
// predicate with new Function inside the page, which the node's Content-Security-Policy forbids, so this polls
// through evaluate instead.
const waitFor = async (page, fn, arg = null, timeout = 30000) => { const end = Date.now() + timeout; for (;;) { const v = await page.evaluate(fn, arg).catch(() => null); if (v) return v; if (Date.now() > end) throw new Error('timed out waiting in the page'); await new Promise((r) => setTimeout(r, 100)); } };
const TOKEN = process.argv[2] || ''; const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const problems = [];
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) problems.push(what); };
const playing = (p) => waitFor(p, () => { const a = document.querySelector('audio'); return a && a.src && !a.paused && a.currentTime > 0.3; }, null, 15000).then(() => true).catch(() => false);

(async () => {
  // The first FLAC of the Tracks page (fixtures may have no album tags)
  const page = `${BASE}/tracks`;
  const start = async (p) => { await p.goto(page); await p.waitForSelector('.track'); await p.hover('.track'); await p.click('.track [data-act="play"]'); };
  const browser = await chromium.launch();
  const ctx = await browser.newContext();
  await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);

  let p = await ctx.newPage();
  await start(p);
  check(await playing(p), 'a library song plays in this browser');
  const type = await p.evaluate(async () => { const r = await fetch(document.querySelector('audio').src, { headers: { range: 'bytes=0-1' } }); return r.headers.get('content-type'); });
  check(/^audio\//.test(type || ''), `the node sends audio (${type})`);
  await p.close();

  p = await ctx.newPage();
  let failed = 0;
  await p.route(/\/v1\/library\/tracks\/\d+\/file/, (r) => { if (failed < 2) { failed++; return r.fulfill({ status: 503, contentType: 'text/html', body: '<h1>Bad gateway</h1>' }); } return r.continue(); });
  const toasts = []; await p.exposeFunction('pmNote', (t) => toasts.push(t));
  await p.addInitScript(() => document.addEventListener('DOMContentLoaded', () => new MutationObserver(() => window.pmNote(document.querySelector('#toast').textContent)).observe(document.querySelector('#toast'), { childList: true, characterData: true, subtree: true })));
  await start(p);
  check(await playing(p), 'after a busy answer the song plays on the retry');
  check(toasts.some((t) => /503/.test(t) && /Trying again/.test(t)), 'the page says what came back and that it tries again');
  await browser.close();
  if (problems.length) { console.log(`${problems.length} problems`); process.exit(1); }
  console.log('library FLACs play in the browser, also after a busy answer');
})();
