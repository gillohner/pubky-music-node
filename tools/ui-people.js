// Settings › People on phones and tablets (UX sweep 2026-11-01, S1–S5):
// - Invite someone is three radio cards, one line each, and only the chosen one's details show;
// - the key field takes a paste or a scanned QR code in any spelling Pubky apps show, and says so at the field when it isn't a key;
//   other clipboard text never lands in the field, and a browser that can't read QR codes turns the camera off and says so;
// - on phones each section folds (its title still a heading), what was open stays open when the page reloads after an action,
//   and the folds follow a phone turned into a tablet and back, keeping what was typed;
// - Play to says "Only you" instead of "Nobody";
// - tag chips are 40 px to touch;
// - the song page's Tags say they are public.
// /v1/people, the party, the clipboard and the camera are mocked or stubbed here.
//   node tools/ui-people.js <api token> <node URL>
const assert = require('node:assert/strict');
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || 'ci-token';
const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const KEY = 'pxnu33x7jtpx9ar1ytsi4yxbp6a5o36gwhffs8zoxmbuptici1jy';
const KEY2 = 'o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy';
const PEOPLE = { homeserver: { configured: true, reachable: true, version: '0.6.0', users: 1, disk_used_mb: 2, public_key: KEY }, people: [], invites: [] };
(async () => {
  const browser = await chromium.launch();
  try {
    for (const width of [390, 768]) {
      const phone = width < 600;
      const ctx = await browser.newContext({ viewport: { width, height: 900 }, reducedMotion: 'reduce', hasTouch: phone, isMobile: phone, permissions: ['clipboard-read', 'clipboard-write'] });
      await ctx.addInitScript((t) => {
        localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser');
        // A camera that sees a QR code with whatever window.__scanValue holds
        // (window.__detectorThrows: a browser with the API that can't read QR codes)
        window.BarcodeDetector = class { constructor() { if (window.__detectorThrows) throw new TypeError('qr_code is not supported'); } async detect() { return window.__scanValue ? [{ rawValue: window.__scanValue }] : []; } };
        const cam = async () => { const c = document.createElement('canvas'); c.width = 64; c.height = 64; c.getContext('2d').fillRect(0, 0, 64, 64); window.__cam = c.captureStream(5); return window.__cam; };
        if (navigator.mediaDevices) navigator.mediaDevices.getUserMedia = cam; else Object.defineProperty(navigator, 'mediaDevices', { value: { getUserMedia: cam } });
      }, TOKEN);
      const page = await ctx.newPage();
      const errors = []; page.on('pageerror', (e) => errors.push(e.message));
      const sent = [];
      await page.route(/\/v1\/people(\?.*)?$/, (r) => r.fulfill({ json: PEOPLE }));
      await page.route(/\/v1\/people\/zones$/, (r) => r.fulfill({ json: { zones: [] } }));
      await page.route(/\/v1\/party$/, (r) => r.fulfill({ json: [] }));
      await page.route(/\/v1\/people\/invites$/, (r) => { sent.push(r.request().postDataJSON()); return r.fulfill({ json: { id: 'i1', access: 'music', created_at: 1 } }); });
      await page.goto(`${BASE}/settings/people`);
      const box = page.locator('#peoplebox');
      await box.locator('.ways .way').first().waitFor();

      // S1: three cards, no select; the chosen one's details only
      assert.equal(await box.locator('.ways .way').count(), 3, 'three kinds as cards');
      assert.equal(await box.locator('select#pinvaccess').count(), 0, 'no select');
      const shownMore = () => box.locator('.way-more:visible').count();
      assert.equal(await shownMore(), 1, 'one kind’s details at a time');
      assert.equal(await box.locator('#pinvkeyrow').isVisible(), false, 'no key field for a homeserver invite');
      await box.locator('.way', { hasText: 'Music only' }).click();
      assert.equal(await shownMore(), 1);
      assert.match(await box.locator('.way.on').textContent(), /Music only.*elsewhere.*Added by their key/s, 'the chosen card says the rest');
      assert.equal(await box.locator('#pinvquota').isVisible(), false);
      assert.equal(await box.locator('#pinvkeyrow').isVisible(), true);

      // S2: on phones the sections fold, Invite open while nobody is invited, Party shut
      if (phone) {
        assert.equal(await box.locator('details[data-fold="invite"]').getAttribute('open'), '', 'Invite open');
        assert.equal(await box.locator('details[data-fold="party"][open]').count(), 0, 'Party shut');
        assert.equal(await box.locator('#ptgo').isVisible(), false);
        assert.equal(await page.getByRole('heading', { name: 'Party' }).count(), 1, 'a fold’s title is still a heading');
        await box.locator('details[data-fold="party"] > summary').click();
        assert.equal(await box.locator('details[data-fold="party"][open]').count(), 1, 'Party opens');
      } else {
        assert.equal(await box.locator('[data-fold]').count(), 0, 'no folds off phones');
        assert.equal(await box.locator('.set-h', { hasText: 'Party' }).isVisible(), true);
      }

      // The key: a wrong one is said at the field, and nothing is sent
      const key = box.locator('#pinvkey');
      await key.fill('not a key'); await key.dispatchEvent('change');
      assert.equal(await box.locator('#pinvkeyerr').isVisible(), true, 'a wrong key is said at the field');
      await box.locator('#pinvgo').click(); await page.waitForTimeout(150);
      assert.equal(sent.length, 0, 'nothing sent for a wrong key');
      // Paste: what isn't a key never lands in the field; a profile link pastes as its key
      await key.fill('');
      await page.evaluate(() => navigator.clipboard.writeText('my bank password'));
      await box.locator('#pinvpaste').click();
      await page.locator('#toast', { hasText: 'no Pubky key' }).waitFor();
      assert.equal(await key.inputValue(), '', 'the clipboard’s other text stays out of the field');
      await page.evaluate((k) => navigator.clipboard.writeText(`pubky://${k}/pub/social/v1/profile.json`), KEY);
      await box.locator('#pinvpaste').click();
      await page.waitForFunction((k) => document.querySelector('#pinvkey').value === k, KEY);
      assert.equal(await box.locator('#pinvkeyerr').isVisible(), false);
      // Scan, in a browser that can't read QR codes: the camera goes off and it says why
      await key.fill('');
      await page.evaluate(() => { window.__detectorThrows = true; });
      await box.locator('#pinvscan').click();
      const scanner = page.getByRole('dialog', { name: 'Scan their key' });
      await scanner.locator('[data-scanmsg]', { hasText: 'can’t read QR codes' }).waitFor();
      assert.equal(await page.evaluate(() => window.__cam.getTracks().every((t) => t.readyState === 'ended')), true, 'the camera is off');
      await scanner.locator('[data-scanclose]').click();
      assert.equal(await page.locator('dialog.scan-dialog').count(), 0);
      await page.evaluate(() => { window.__detectorThrows = false; });
      // Scan: the camera sees a QR code with the key
      await page.evaluate((k) => { window.__scanValue = `pubky${k}`; }, KEY2);
      await box.locator('#pinvscan').click();
      await page.waitForFunction((k) => document.querySelector('#pinvkey').value === k, KEY2);
      assert.equal(await page.locator('dialog.scan-dialog').count(), 0, 'the scanner closes on a key');
      await box.locator('#pinvname').fill('Bo');
      await box.locator('#pinvgo').click();
      await page.locator('#toast', { hasText: 'Added' }).waitFor();
      assert.deepEqual(sent.shift(), { access: 'music', name: 'Bo', key: KEY2 });
      if (phone) {
        await page.waitForTimeout(300);
        assert.equal(await box.locator('details[data-fold="party"][open]').count(), 1, 'what was open stays open after the reload');
        // Turned into a tablet and back: the folds follow the shell, and what was typed stays
        await box.locator('.way', { hasText: 'Music only' }).click();
        await box.locator('#pinvname').fill('Cy');
        await page.setViewportSize({ width: 768, height: 900 });
        await box.locator('[data-fold]').first().waitFor({ state: 'detached' });
        await box.locator('#pinvname').waitFor();
        assert.equal(await box.locator('#pinvname').inputValue(), 'Cy', 'what was typed stays');
        assert.equal(await box.locator('.way.on').textContent().then((t) => /Music only/.test(t)), true, 'the chosen kind stays');
        await page.setViewportSize({ width, height: 900 });
        await box.locator('[data-fold]').first().waitFor({ state: 'attached' });
        assert.equal(await box.locator('#pinvname').inputValue(), 'Cy');
      }
      let overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
      assert.ok(overflow <= 0, `no sideways scroll on People at ${width} px (${overflow})`);

      // S3: who sees what you play reads "Only you"
      const zb = page.locator('#zonebtn');
      assert.equal(await zb.isVisible(), true, 'the zone button is there');
      await zb.click();
      const vis = page.locator('#zpvisible');
      await vis.waitFor();
      assert.match(await vis.textContent(), /Who sees what you play\s*Only you/);
      await page.keyboard.press('Escape');

      // S4 and S5: the song page's tag chips are 40 px to touch, and its Tags say they are public
      await page.route(/\/v1\/tags(\?.*)?$/, (r) => (r.request().method() === 'GET' ? r.fulfill({ json: { tags: [{ label: 'chill' }] } }) : r.fallback()));
      await page.goto(`${BASE}/song/lib-1`);
      await page.locator('#itags .chip.tag').first().waitFor();
      const h = (await page.locator('#itags .chip.tag').first().boundingBox()).height;
      assert.ok(h >= 40, `a tag chip is 40 px to touch at ${width} (${h})`);
      for (const part of ['.chiplink', '.chipx']) {
        const ph = (await page.locator(`#itags .chip.tag ${part}`).first().boundingBox()).height;
        assert.ok(ph >= 36, `a tag chip's ${part} fills it at ${width} (${ph})`);
      }
      assert.match(await page.locator('#itags .tag-hint').textContent(), /Public on your homeserver/);
      await page.goto(`${BASE}/genres`);
      await page.locator('[data-mytag]').first().waitFor();
      const g = (await page.locator('[data-mytag]').first().boundingBox()).height;
      assert.ok(g >= 40, `Your Pubky tags are 40 px to touch at ${width} (${g})`);
      overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
      assert.ok(overflow <= 0, `no sideways scroll on Genres at ${width} px (${overflow})`);
      assert.deepEqual(errors, []);
      await ctx.close();
    }
  } finally {
    await browser.close();
  }
  console.log('people: invite cards, paste and scan, folds on phones, Only you, tag chips to touch, public tags');
})().catch((e) => { console.error(e); process.exit(1); });
