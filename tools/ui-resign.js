// Real-browser check for a Pubky session approved for fewer folders than the node uses
// (STEWARD-FIX-resign.md, then TASK-login-ux): there is no separate "Sign in again" any more; the
// owner's Log out on its notice makes the next log in ask for the folders. So, with the node's
// status answer set in the browser:
//   * folders missing: Settings › My accounts and Your library say so with a Log out button (Save
//     now disabled, the notice as its tooltip); Home and item pages show no banner;
//   * after a log in renewed the session: no notice anywhere, Save now enabled.
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-resign.js <api token> [http://127.0.0.1:7788] [screenshot dir]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788'; const OUT = process.argv[4] || null;
const errors = [];
const fails = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) fails.push(what); };
const NOTICE = 'This node needs your approval for new folders on your homeserver. Use Log out here and log in again with Pubky Ring to give it.';
(async () => {
  const browser = await chromium.launch();
  const shot = async (p, name) => { if (OUT) await p.screenshot({ path: `${OUT}/${name}.png` }); };
  const open = async (width, missing) => {
    const ctx = await browser.newContext({ viewport: { width, height: width < 600 ? 844 : 800 }, colorScheme: 'dark' });
    await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);
    await ctx.route('**/v1/pubky', (route) => route.fulfill({ json: { user: 'pubky' + 'o'.repeat(52), unsynced: 0, needs_resign: missing, missing: missing ? ['/pub/music/v1/'] : [], notice: missing ? NOTICE : null, sync: null } }));
    const p = await ctx.newPage();
    p.on('pageerror', (e) => errors.push('page error: ' + e.message));
    p.on('console', (m) => { if (m.type() === 'error' && !/Failed to load resource/.test(m.text())) errors.push('console: ' + m.text()); });
    return { ctx, p };
  };
  for (const width of [1280, 390]) {
    for (const missing of [true, false]) {
      const { ctx, p } = await open(width, missing);
      const what = missing ? 'folders missing' : 'renewed';
      await p.goto(BASE + '/settings/accounts'); await p.waitForSelector('#main .set-group', { timeout: 20000 });
      check(missing ? await p.isVisible('#pkresign [data-resign]') : !(await p.$('#pkresign')), `${width}, ${what}: Settings ${missing ? 'says so, with Log out' : 'has no notice'}`);
      check(!(await p.$('#pklogin')), `${width}, ${what}: no "Sign in again"`);
      await shot(p, `resign-settings-${width}-${missing ? 'missing' : 'renewed'}`);
      await p.goto(BASE + '/library'); await p.waitForSelector('#sync', { timeout: 20000 });
      check(await p.$eval('#sync', (b, m) => (m ? b.disabled && b.title.startsWith('This node needs your approval') : !b.disabled), missing), `${width}, ${what}: Save now is ${missing ? 'disabled with the notice as tooltip' : 'enabled'}`);
      await p.goto(BASE + '/'); await sleep(1500);
      check(!(await p.$('#main .notice.resign')), `${width}, ${what}: Home has no banner`);
      await ctx.close();
    }
  }
  await browser.close();
  for (const e of errors) console.log(e);
  if (fails.length || errors.length) { console.log(`${fails.length} failed, ${errors.length} page errors`); process.exit(1); }
  console.log('all passed');
})();
