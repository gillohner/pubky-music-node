// Real-browser check for a Pubky session approved for fewer folders than the node uses
// (STEWARD-FIX-resign.md):
//   * the node's real state (an old session): Settings shows the notice, "Sign in again" is the
//     primary button and Sync is disabled with the notice as its tooltip; Home and the item page
//     show the one-line notice with the button, which opens Settings and starts the sign-in;
//   * after a re-approval (the status answer replaced in the browser): no notice, Sync enabled,
//     "Sign in again" still offered as a plain button.
// If the node's session already covers every folder, the first part is skipped.
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-resign.js <api token> [http://127.0.0.1:7788] [screenshot dir]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788'; const OUT = process.argv[4] || null;
const errors = [];
const fails = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) fails.push(what); };
(async () => {
  const browser = await chromium.launch();
  const shot = async (p, name) => { if (OUT) await p.screenshot({ path: `${OUT}/${name}.png` }); };
  const open = async (width, cleared) => {
    const ctx = await browser.newContext({ viewport: { width, height: width < 600 ? 844 : 800 }, colorScheme: 'dark' });
    await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);
    if (cleared) {
      await ctx.route('**/v1/pubky', async (route) => {
        const r = await route.fetch(); const j = await r.json();
        delete j.missing; delete j.notice; j.needs_resign = false;
        await route.fulfill({ response: r, json: j });
      });
    }
    const p = await ctx.newPage();
    p.on('pageerror', (e) => errors.push('page error: ' + e.message));
    p.on('console', (m) => { if (m.type() === 'error' && !/Failed to load resource/.test(m.text())) errors.push('console: ' + m.text()); });
    return { ctx, p };
  };
  const status = await (await fetch(BASE + '/v1/pubky', { headers: { Authorization: 'Bearer ' + TOKEN } })).json();
  if (!status.user) { console.log('skip: the node has no Pubky session'); await browser.close(); return; }

  if (status.needs_resign) {
    for (const width of [1280, 390]) {
      const { ctx, p } = await open(width, false);
      await p.goto(BASE + '/settings'); await p.waitForSelector('#pklogin', { timeout: 20000 });
      check(await p.isVisible('#pkresign'), `${width}: Settings shows the notice`);
      check((await p.textContent('#pklogin')).trim() === 'Sign in again', `${width}: the button says "Sign in again"`);
      check(await p.$eval('#pklogin', (b) => b.classList.contains('primary')), `${width}: it is the primary button`);
      await p.$eval('#pkresign', (e) => e.scrollIntoView({ block: 'center' })); await sleep(200);
      await shot(p, `resign-settings-${width}-dark`);
      await p.goto(BASE + '/library'); await p.waitForSelector('#sync', { timeout: 20000 });
      check(await p.$eval('#sync', (b) => b.disabled && b.title.startsWith('This node needs permission')), `${width}: Save now (Your library) is disabled with the notice as tooltip`);
      await p.goto(BASE + '/'); await p.waitForSelector('#main .notice.resign', { timeout: 20000 }).catch(() => {});
      check(await p.isVisible('#main .notice.resign [data-resign]'), `${width}: Home shows the notice with the button`);
      await shot(p, `resign-home-${width}-dark`);
      if (width === 1280) {
        await p.goto(BASE + '/song/mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69'); await p.waitForSelector('#iyours', { timeout: 30000 }).catch(() => {});
        check(await p.isVisible('#iyours .notice.resign [data-resign]'), 'the item page shows it next to tags and rating');
        await p.$eval('#iyours', (e) => e.scrollIntoView({ block: 'center' })).catch(() => {}); await sleep(200);
        await shot(p, 'resign-item-1280-dark');
        await p.click('#iyours [data-resign]');
        await p.waitForSelector('#pkurl .qrbox', { timeout: 20000 }).catch(() => {});
        check(new URL(p.url()).pathname === '/settings' && await p.isVisible('#pkurl .qrbox'), 'its button opens Settings and starts the Pubky Ring sign-in');
      }
      await ctx.close();
    }
  } else {
    console.log('skip: this node\'s session already covers every folder (no notice to check)');
  }

  for (const width of [1280, 390]) {
    const { ctx, p } = await open(width, true);
    await p.goto(BASE + '/settings'); await p.waitForSelector('#pklogin', { timeout: 20000 });
    check(!(await p.$('#pkresign')), `${width}: after re-approval there is no notice`);
    check(await p.$eval('#pklogin', (b) => b.textContent.trim() === 'Sign in again' && !b.classList.contains('primary')), `${width}: "Sign in again" stays, as a plain button`);
    await p.$eval('#pklogin', (e) => e.scrollIntoView({ block: 'center' })); await sleep(200);
    await shot(p, `resign-cleared-settings-${width}-dark`);
    await p.goto(BASE + '/library'); await p.waitForSelector('#sync', { timeout: 20000 });
    check(await p.$eval('#sync', (b) => !b.disabled), `${width}: Save now (Your library) is enabled`);
    await p.goto(BASE + '/'); await sleep(1500);
    check(!(await p.$('#main .notice.resign')), `${width}: Home has no notice`);
    await ctx.close();
  }
  await browser.close();
  for (const e of errors) console.log(e);
  if (fails.length || errors.length) { console.log(`${fails.length} failed, ${errors.length} page errors`); process.exit(1); }
  console.log('all passed');
})();
