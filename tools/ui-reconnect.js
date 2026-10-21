// Real-browser check of the connection (docs/CONNECTION.md): two controllers on a playing zone, the node killed
// and restored. Measures how fast both notice, how long the resync takes after the node is back, that a change
// made during a short outage is held and goes through, and that one made during a long outage is refused.
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   PM_NULL_OUTPUT=realtime node /path/to/tools/ui-reconnect.js <token> <http://127.0.0.1:7799> <shots dir> -- <pm-node> --config <node.toml> serve
// The script starts and kills the node itself (the command after `--`). The node needs at least one track
// longer than a minute and a `null-output` zone; PM_NULL_OUTPUT=realtime makes the null output play in real time.
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const { spawn } = require('child_process');
const [TOKEN, BASE, OUT] = process.argv.slice(2, 5);
const NODE = process.argv.slice(process.argv.indexOf('--') + 1);
const ZONE = process.env.ZONE || 'null-output';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const api = async (method, path, body) => {
  const r = await fetch(BASE + '/v1' + path, { method, headers: { authorization: 'Bearer ' + TOKEN, 'content-type': 'application/json' }, body: body && JSON.stringify(body) });
  const t = await r.text(); return t ? JSON.parse(t) : null;
};
let node = null;
const startNode = async () => {
  node = spawn(NODE[0], NODE.slice(1), { stdio: 'ignore', env: process.env });
  for (let i = 0; i < 100; i++) { try { await api('GET', '/info'); return; } catch { await sleep(100); } }
  throw new Error('node did not start');
};
const killNode = () => new Promise((r) => { node.once('exit', r); node.kill('SIGKILL'); });
const until = async (what, fn, ms = 60000) => { const t = Date.now(); while (Date.now() - t < ms) { if (await fn()) return Date.now() - t; await sleep(25); } throw new Error('timed out: ' + what); };
const connState = (p) => p.$eval('#conn', (e) => e.dataset.state || '');
const barTitle = (p) => p.$eval('#np .np-title', (e) => e.textContent);
const toggleLabel = (p) => p.$eval('#toggle', (e) => e.getAttribute('aria-label'));
let failed = false;
const check = (ok, msg) => { console.log((ok ? 'ok    ' : 'FAILED ') + msg); if (!ok) failed = true; };

(async () => {
  await startNode();
  let tracks = [];
  for (let i = 0; i < 40 && !tracks.length; i++) {
    tracks = (await api('GET', '/library/tracks')).tracks.filter((t) => t.duration_ms > 60000);
    if (!tracks.length) { if (i === 0) await api('POST', '/library/scan', {}); await sleep(500); }
  }
  if (!tracks.length) throw new Error('needs a track longer than a minute');
  await api('POST', `/zones/${ZONE}/queue`, { track_ids: [tracks[0].id], replace: true, play: true });
  const b = await chromium.launch();
  const pages = [];
  for (const [w, h, name] of [[1280, 800, 'desktop'], [390, 844, 'phone']]) {
    const ctx = await b.newContext({ viewport: { width: w, height: h }, colorScheme: name === 'phone' ? 'light' : 'dark' });
    await ctx.addInitScript(([t, z]) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', z); }, [TOKEN, ZONE]);
    const p = await ctx.newPage(); p.name = name;
    p.on('pageerror', (e) => check(false, `${name}: page error ${e.message}`));
    await p.goto(BASE + '/');
    pages.push(p);
  }
  const [A, B] = pages;
  for (const p of pages) await until(`${p.name} connected`, async () => (await connState(p)) === 'connected');
  for (const p of pages) await until(`${p.name} shows the track`, async () => (await barTitle(p)) === tracks[0].title, 5000);
  await until('latency shown', async () => /\d+ ms/.test(await A.$eval('#conn', (e) => e.textContent)), 15000);
  console.log('      connected, latency:', await A.$eval('#conn', (e) => e.textContent));

  // 1. Short outage: kill during playback, press pause on A while down, restore within the hold time.
  let t0 = Date.now();
  await killNode();
  const noticed = await Promise.all(pages.map((p) => until(`${p.name} notices`, async () => (await connState(p)) !== 'connected', 30000)));
  check(Math.max(...noticed) < 2000, `both controllers notice the node is gone (${noticed.join(' / ')} ms)`);
  await sleep(2600 - (Date.now() - t0));
  for (const p of pages) await p.screenshot({ path: `${OUT}/conn-reconnecting-${p.name}.png` });
  const pressed = A.click('#toggle'); // held until the node is back
  await sleep(500);
  await startNode();
  const tUp = Date.now();
  const resync = await Promise.all(pages.map((p) => until(`${p.name} back`, async () => (await connState(p)) === 'connected', 30000)));
  check(Math.max(...resync) < 5000, `both controllers resync after the restart (${resync.join(' / ')} ms after the node was back; ${tUp - t0} ms outage)`);
  await pressed;
  // The node restores the queue stopped; the held toggle starts it again, and both controllers show it.
  const played = await Promise.all(pages.map((p) => until(`${p.name} sees the held toggle`, async () => (await toggleLabel(p)) === 'Pause', 10000).catch(() => -1)));
  const z = (await api('GET', `/zones/${ZONE}`));
  check(z.transport === 'playing' && played.every((t) => t >= 0), `the change held during the outage went through and both show it (node: ${z.transport})`);
  for (const p of pages) check((await barTitle(p)) === tracks[0].title, `${p.name} shows the same track as the node`);

  // 2. Two controllers stay in sync: pause on B, A follows.
  t0 = Date.now(); await B.click('#toggle');
  const follow = await until('A follows B', async () => (await toggleLabel(A)) === 'Play', 5000);
  check(follow < 1000, `a change on one controller shows on the other (${follow} ms)`);

  // 3. Long outage: a change is refused with a message after the hold time; Retry now reconnects at once.
  await killNode();
  await until('A notices', async () => (await connState(A)) !== 'connected', 30000);
  await sleep(21000);
  for (const p of pages) await p.screenshot({ path: `${OUT}/conn-offline-${p.name}.png` });
  check((await connState(A)) === 'offline', `after 20 s the state is offline (${await A.$eval('#conn', (e) => e.textContent)})`);
  t0 = Date.now(); await A.click('#toggle');
  const refused = await until('refusal toast', async () => /Not connected/.test(await A.$eval('#toast', (e) => e.className.includes('show') ? e.textContent : '')), 12000);
  check(refused > 7000, `a change during a long outage is refused after the hold time (${refused} ms)`);
  check((await api('GET', `/zones/${ZONE}`).catch(() => null)) === null, 'the node is still down (nothing was sent)');
  await startNode();
  t0 = Date.now(); await A.click('#connretry');
  const retry = await until('Retry now', async () => (await connState(A)) === 'connected', 5000);
  check(retry < 2000, `Retry now reconnects at once (${retry} ms)`);
  const zb = await api('GET', `/zones/${ZONE}`);
  check(zb.transport !== 'playing', `the refused change was not applied later (node: ${zb.transport})`);
  await B.waitForTimeout(100);
  await b.close();
  await killNode();
  process.exitCode = failed ? 1 : 0;
})().catch(async (e) => { console.error(e); if (node) node.kill('SIGKILL'); process.exit(1); });
