// YouTube in "This browser" (STEWARD-DUAL-MODE.md): a YouTube item plays in YouTube's own player
// (IFrame Player API), our transport drives it, the next item goes back to the <audio> element,
// and a video that can't be embedded says why and moves on.
// YouTube's player script is replaced by a stand-in with the same API (YouTube asks servers and CI
// runners to sign in first, so the real one can't be relied on there); --live uses the real one.
//   node tools/ui-youtube.js <api token> [http://127.0.0.1:7788] [--live]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
// Waits until fn(arg) is truthy in the page and returns its value. Playwright's waitForFunction rebuilds the
// predicate with new Function inside the page, which the node's Content-Security-Policy forbids, so this polls
// through evaluate instead.
const waitFor = async (page, fn, arg = null, timeout = 30000) => { const end = Date.now() + timeout; for (;;) { const v = await page.evaluate(fn, arg).catch(() => null); if (v) return v; if (Date.now() > end) throw new Error('timed out waiting in the page'); await new Promise((r) => setTimeout(r, 100)); } };
const args = process.argv.slice(2).filter((a) => a !== '--live'); const LIVE = process.argv.includes('--live');
const TOKEN = args[0] || ''; const BASE = args[1] || 'http://127.0.0.1:7788';
// The stand-in: YT.Player with the calls and events the page uses; ids starting with "x" can't be embedded (150)
const FAKE_API = `window.YT = { Player: function (el, o) {
  const f = document.createElement('iframe'); f.src = 'https://www.youtube.com/embed/' + o.videoId; f.title = 'YouTube'; document.getElementById(el).replaceWith(f);
  let st = -1, t0 = 0, at = 0; const now = () => (st === 1 ? at + (Date.now() - t0) / 1000 : at); const set = (s) => { at = now(); t0 = Date.now(); st = s; o.events.onStateChange({ data: s }); };
  const load = (id) => { at = 0; if (/^x/.test(id)) { setTimeout(() => o.events.onError({ data: 150 }), 50); return; } setTimeout(() => set(1), 50); };
  Object.assign(this, { playVideo: () => set(1), pauseVideo: () => set(2), stopVideo: () => { at = 0; st = 5; }, loadVideoById: load, getCurrentTime: now, getDuration: () => 600, seekTo: (v) => { at = v; t0 = Date.now(); }, setVolume: () => {} });
  setTimeout(() => { o.events.onReady(); }, 30);
} }; setTimeout(() => window.onYouTubeIframeAPIReady && window.onYouTubeIframeAPIReady(), 0);`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const VIDEO = 'aqz-KE-bpKQ'; // Big Buck Bunny (Blender Foundation), embeddable

(async () => {
  const fail = (m) => { console.error('FAIL ' + m); process.exitCode = 1; };
  const b = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
  const ctx = await b.newContext({ viewport: { width: 1280, height: 900 } });
  await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);
  if (!LIVE) {
    await ctx.route('https://www.youtube.com/iframe_api', (r) => r.fulfill({ contentType: 'text/javascript', body: FAKE_API }));
    await ctx.route('https://www.youtube.com/embed/**', (r) => r.fulfill({ contentType: 'text/html', body: '<body style="background:#000"></body>' }));
  }
  const p = await ctx.newPage(); const errs = []; p.on('pageerror', (e) => errs.push(e.message));
  await p.goto(BASE + '/'); await waitFor(p, () => !!window.pmPlayer, null, 15000);
  const lib = await p.evaluate(async (t) => (await (await fetch('/v1/library/tracks?limit=1', { headers: { authorization: 'Bearer ' + t } })).json()).tracks[0], TOKEN);
  const file = `${BASE}/v1/library/tracks/${lib.id}/file?token=${encodeURIComponent(TOKEN)}`;
  await p.evaluate(([v, f]) => window.pmPlayer.play([{ uri: `youtube:video:${v}`, title: 'Big Buck Bunny' }, { uri: f, title: 'A library song' }]), [VIDEO, file]);
  const now = () => p.evaluate(() => window.pmPlayer.now());
  let n; for (let i = 0; i < 40; i++) { await sleep(500); n = await now(); if (n.youtube && !n.paused && n.time > 1) break; }
  if (!(n.youtube && !n.paused && n.time > 1)) fail(`the YouTube video plays: ${JSON.stringify(n)}`); else console.log(`ok   YouTube plays (${n.time.toFixed(1)} s in)`);
  if (!(await p.isVisible('#ytpanel')) || !(await p.$('#ytpanel iframe[src*="youtube"]'))) fail('the YouTube panel shows its player'); else console.log('ok   the panel shows YouTube’s player');
  if (!/Big Buck Bunny/.test(await p.textContent('.np-title'))) fail('the player bar names the video'); else console.log('ok   the player bar names it');
  await p.click('#toggle'); await sleep(1200); n = await now();
  if (!n.paused) fail('our pause button pauses YouTube'); else console.log('ok   our pause button pauses it');
  await p.click('#toggle'); await sleep(1500); n = await now();
  if (n.paused) fail('our play button resumes YouTube'); else console.log('ok   our play button resumes it');
  await p.click('#next'); await sleep(2000); n = await now();
  if (n.youtube || n.index !== 1 || !(await p.isHidden('#ytpanel'))) fail(`Next goes to the library song in <audio>: ${JSON.stringify(n)}`); else console.log('ok   Next plays the library song; the panel hides');
  // A video that can't play: said, then the next item
  await p.evaluate((f) => window.pmPlayer.play([{ uri: 'youtube:video:xxxxxxxxxx0', title: 'Gone' }, { uri: f, title: 'After' }]), file);
  for (let i = 0; i < 30; i++) { await sleep(500); n = await now(); if (n.index === 1) break; }
  const toastText = await p.evaluate(() => [...document.querySelectorAll('.toast')].map((x) => x.textContent).join(' | '));
  if (n.index !== 1 || !/can't play here/.test(toastText)) fail(`a broken video is skipped with a reason: ${JSON.stringify(n)} ${toastText}`); else console.log('ok   a broken video says why and moves on');
  if (errs.length) fail('page errors: ' + errs.join(' | '));
  await b.close();
  if (!process.exitCode) console.log('YouTube in this browser works');
})();
