// Sign-in chooser regression checks (Qobuz has its own guided page: tools/ui-stream-setup.js). Provider replies are mocked here; pm-api's Rust tests
// cover validation against mock provider servers and the private files on disk.
// Run against a disposable fixture node:
//   node tools/ui-signin.js <api token> <node URL> [screenshot directory]
const assert = require('node:assert/strict');
const path = require('node:path');
const fs = require('node:fs');
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || 'ci-token';
const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const SHOTS = process.argv[4];
const cases = [
  { source: 'spotify', url: '/sources/spotify', mount: '#signin-spotify', way: 'librespot-file', label: 'librespot, from a credentials file', text: '{"username":"listener","auth_type":1,"auth_data":"c3ludGhldGlj"}' },
  { source: 'youtube', url: '/sources/youtube', mount: '#signin-youtube', way: 'cookies', label: 'yt-dlp cookies', text: '# Netscape HTTP Cookie File\n#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t4000000000\tSID\tsynthetic-cookie\n' },
  { source: 'lastfm', url: '/settings/accounts', mount: '#lfbody', way: 'paste', label: 'Pasted sign-in', text: '0123456789abcdef0123456789abcdef\nfedcba9876543210fedcba9876543210\nsynthetic-session\n' },
];
(async () => {
  if (SHOTS) fs.mkdirSync(SHOTS, { recursive: true });
  const initial = {};
  for (const c of cases) {
    const r = await fetch(`${BASE}/v1/sources/${c.source}/signin`, { headers: { authorization: `Bearer ${TOKEN}` } });
    assert.equal(r.status, 200);
    initial[c.source] = await r.json();
  }
  const browser = await chromium.launch();
  try {
    for (const width of [1440, 390]) for (const theme of ['dark', 'light']) {
      const ctx = await browser.newContext({ viewport: { width, height: width === 390 ? 844 : 1000 }, colorScheme: theme, reducedMotion: 'reduce' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
      const page = await ctx.newPage();
      const errors = []; page.on('pageerror', (e) => errors.push(e.message));
      for (const c of cases) {
        let si = structuredClone(initial[c.source]); si.in_use = null; si.chosen = null; si.line = 'Not signed in';
        si.ways.forEach((w) => { w.available = true; delete w.reason; });
        let submitted = 0; let signedOut = 0;
        const endpoint = new RegExp(`/v1/sources/${c.source}/signin(?:/import)?$`);
        await page.route(endpoint, async (route) => {
          const req = route.request();
          const body = req.method() === 'POST' || req.method() === 'PUT' ? req.postDataJSON() : null;
          if (req.method() === 'POST') {
            if (body.text === 'not a sign-in') return route.fulfill({ status: 400, json: { error: 'Not usable: the service refused this sign-in' } });
            assert.equal(body.text, c.text, `${c.source}: the submitted file/paste reaches the API`);
            submitted++;
            si.chosen = c.way; si.in_use = { way: c.way, label: c.label, user: 'listener', gives: 'playback' }; si.line = `${c.label} · listener`;
          } else if (req.method() === 'PUT') si.chosen = body.way;
          else if (req.method() === 'DELETE') { signedOut++; si.in_use = null; si.line = 'Not signed in'; }
          return route.fulfill({ json: si });
        });
        await page.goto(BASE + c.url);
        const panel = page.locator(c.mount);
        await panel.locator('input[type=radio]:checked').waitFor();
        assert.equal(await panel.locator('input[type=radio]:checked').inputValue(), si.ways.find((w) => w.recommended).id);
        assert.equal(await panel.locator('input[type=radio]').count(), si.ways.length);
        await panel.locator(`input[value="${c.way}"]`).check();
        await page.evaluate(() => { document.activeElement?.blur(); for (const el of document.querySelectorAll('*')) if (el.scrollTop) el.scrollTo({ top: 0, left: 0, behavior: 'instant' }); window.scrollTo({ top: 0, left: 0, behavior: 'instant' }); });
        if (SHOTS) {
          // The shell scrolls #main internally. Extend the capture height so the screenshot
          // includes the whole chooser at the requested width, then restore the test viewport.
          const viewport = page.viewportSize();
          const height = await page.evaluate(() => { const m = document.querySelector('#main'); return innerHeight + Math.max(0, m.scrollHeight - m.clientHeight); });
          await page.setViewportSize({ width, height: Math.ceil(height) });
          await page.screenshot({ path: path.join(SHOTS, `13-signin-${c.source}-${width}-${theme}.png`), fullPage: true });
          await page.setViewportSize(viewport);
        }
        // A refused import remains visible and keyboard users can return to the ways.
        if (!c.file) {
          await panel.locator('textarea').fill('not a sign-in');
          await panel.locator('[data-sg=paste] button').click();
          await panel.locator('[data-sg=error]:not([hidden])').waitFor();
          await panel.locator('[data-sg=another]').click();
          assert.equal(await page.evaluate(() => document.activeElement.type), 'radio');
          await panel.locator('textarea').fill(c.text);
          await panel.locator('[data-sg=paste] button').click();
        } else {
          await panel.locator('input[type=file]').setInputFiles({ name: 'config.ini', mimeType: 'text/plain', buffer: Buffer.from(c.text) });
          await panel.locator('[data-sg=file] button').click();
        }
        await panel.locator('[data-sg=out]').waitFor();
        assert.equal(submitted, 1);
        assert.ok((await panel.innerText()).includes(c.label));
        assert.equal(await panel.locator('textarea').count(), 0, 'saved sign-in contents are cleared');
        await panel.locator('[data-sg=change]').click();
        assert.equal(await panel.locator('input[type=radio]:checked').inputValue(), c.way, 'successful way is remembered');
        await panel.locator('[data-sg=change]').click();
        await panel.locator('[data-sg=out]').click();
        await panel.locator('input[type=radio]:checked').waitFor();
        assert.equal(signedOut, 1);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1), false, 'no horizontal overflow');
        await page.unroute(endpoint);
        console.log(`ok ${c.source} ${width} ${theme}: choose, import, change way, sign out`);
      }
      assert.deepEqual(errors, []);
      await ctx.close();
    }
  } finally { await browser.close(); }
})().catch((e) => { console.error(e); process.exitCode = 1; });
