// Real-browser check that the queue reorders by drag (mouse at 1280, touch-enabled phone at 390).
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node /path/to/tools/ui-drag.js <api token> [http://127.0.0.1:7788] [screenshot dir]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788'; const OUT = process.argv[4] || require('os').tmpdir();
(async () => {
  const b = await chromium.launch();
  for (const [w, h, name] of [[1280, 800, 'desktop'], [390, 844, 'phone']]) {
    const ctx = await b.newContext({ viewport: { width: w, height: h }, colorScheme: 'dark', hasTouch: name === 'phone' });
    await ctx.addInitScript((token) => { localStorage.setItem('pm.token', token); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
    const p = await ctx.newPage();
    await p.goto(BASE + '/tracks'); await p.waitForTimeout(2000);
    await p.click('#shuffle'); await p.waitForTimeout(800); // Play all
    await p.evaluate(() => document.querySelector('.nav-item[data-view="queue"]').click()); await p.waitForTimeout(1500); // phones reach it from Now playing
    const names = async () => p.$$eval('.list.queue .list-row .name', (els) => els.map((e) => e.textContent));
    const before = await names();
    const g0 = await p.$('.list.queue [data-qgrip="0"]'); const r2 = await p.$('.list.queue [data-qrow="2"]');
    const a = await g0.boundingBox(); const t = await r2.boundingBox();
    await p.mouse.move(a.x + a.width / 2, a.y + a.height / 2); await p.mouse.down();
    for (let k = 1; k <= 10; k++) { await p.mouse.move(a.x + a.width / 2, a.y + (t.y + t.height * 0.8 - a.y) * k / 10); await p.waitForTimeout(30); }
    await p.screenshot({ path: `${OUT}/qdrag-${name}-mid.png` });
    await p.mouse.up(); await p.waitForTimeout(800);
    const after = await names();
    const want = [...before.slice(1, 3), before[0], ...before.slice(3)];
    const ok = JSON.stringify(after) === JSON.stringify(want);
    console.log(name, ok ? 'ok' : 'FAILED', '| before', before.join(' | '), '| after', after.join(' | '));
    if (!ok) process.exitCode = 1;
    await ctx.close();
  }
  await b.close();
})();
