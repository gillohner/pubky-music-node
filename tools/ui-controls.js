// Control audit: every visible button does something (STEWARD-PLAN step 4, "nav and control
// audits"). For each main view, each distinct visible button (rows repeat the same buttons; one
// of each kind is enough) is clicked on a fresh page load, and the click must change something:
// the DOM, the URL, a request to the node, a dialog or a new window. A button that changes nothing
// is a dead control and fails the run.
// Buttons with lasting effects (delete, sign in or out, scan, sync, import, …) are skipped; run it
// against a node whose data you can lose (a copy, or CI's fixture node).
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-controls.js <api token> [http://127.0.0.1:7788]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const SKIP = /share on pubky|make private|delete|remove|trash|sign (in|out)|sign in again|log ?out|forget|clear|reset|rescan|scan|sync|import|mirror|upload|download|backup|restore|tag my whole|identify|accept|write ids|restart|stop|change token|add folder|create|save|apply|disconnect|connect|pubky ring|copy/i;
const api = async (path) => (await fetch(BASE + '/v1' + path, { headers: { Authorization: 'Bearer ' + TOKEN } })).json();

(async () => {
  const views = ['/', '/favourites', '/albums', '/artists', '/genres', '/tracks', '/playlists', '/radio', '/sources', '/queue', '/zones', '/settings', '/search?q=queen'];
  try { const pls = await api('/playlists'); const pl = pls.find((p) => p.items > 0); if (pl) views.push('/playlist/' + pl.id); } catch { /* none */ }
  try { const al = await api('/library/albums'); if (al[0]) views.push('/album/lib-' + al[0].id); } catch { /* none */ }
  views.push('/song/mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69');
  const browser = await chromium.launch();
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, colorScheme: 'dark' });
  await ctx.addInitScript((t) => {
    localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser');
    HTMLMediaElement.prototype.play = async function () {};
    window.open = () => { window.__opened = (window.__opened || 0) + 1; return null; };
  }, TOKEN);
  const p = await ctx.newPage();
  const pageErrors = [];
  p.on('pageerror', (e) => pageErrors.push(`${p.url()}: ${e.message}`));
  let requests = 0; p.on('request', (r) => { if (r.url().includes('/v1/') || r.url().includes('/mb/')) requests++; });
  const settle = async () => { await p.waitForLoadState('networkidle', { timeout: 15000 }).catch(() => {}); await sleep(400); };
  // Visible, enabled buttons, one per kind: class, data attributes and wording (digits dropped).
  const kinds = () => p.$$eval('button', (els) => {
    const seen = new Set(); const out = [];
    els.forEach((b, i) => {
      const r = b.getBoundingClientRect(); const st = getComputedStyle(b);
      if (b.disabled || r.width < 2 || r.height < 2 || st.visibility === 'hidden' || st.display === 'none' || Number(st.opacity) === 0 || b.closest('[hidden]')) return;
      if (r.bottom < 0 || r.top > innerHeight * 4) return;
      const data = Object.keys(b.dataset).sort().join(',');
      const words = (b.getAttribute('aria-label') || b.title || b.textContent || '').replace(/\d+/g, '#').replace(/\s+/g, ' ').trim().slice(0, 40);
      const sig = `${b.className}|${data}|${words}`;
      if (seen.has(sig)) return; seen.add(sig);
      b.dataset.auditIdx = String(i);
      out.push({ i, sig, words, id: b.id });
    });
    return out;
  });
  const dead = []; let clicked = 0; let skipped = 0;
  for (const view of views) {
    await p.goto(BASE + view); await settle();
    const list = await kinds();
    for (const k of list) {
      if (SKIP.test(k.words) || SKIP.test(k.id)) { skipped++; continue; }
      await p.goto(BASE + view); await settle();
      const now = await kinds(); const target = now.find((x) => x.sig === k.sig);
      if (!target) continue;
      const before = { url: p.url(), req: requests };
      await p.evaluate(() => { window.__scroll = (document.querySelector('#main') || document.scrollingElement).scrollTop + scrollY; window.__focus = document.activeElement; window.__mut = 0; window.__mo && window.__mo.disconnect(); window.__mo = new MutationObserver((ms) => { window.__mut += ms.filter((m) => !(m.type === 'attributes' && m.attributeName === 'data-audit-idx')).length; }); window.__mo.observe(document.body, { subtree: true, childList: true, attributes: true, characterData: true }); window.__opened = 0; });
      const el = await p.$(`button[data-audit-idx="${target.i}"]`);
      if (!el) continue;
      try { await el.click({ timeout: 3000 }); } catch { try { await el.click({ force: true, timeout: 2000 }); } catch { continue; } }
      clicked++;
      await sleep(900);
      const after = await p.evaluate(() => ({ mut: window.__mut || 0, opened: window.__opened || 0, dialog: !!document.querySelector('dialog[open]'), scrolled: Math.abs((document.querySelector('#main') || document.scrollingElement).scrollTop + scrollY - window.__scroll) > 4 })).catch(() => ({ mut: 1 }));
      // Moving the view (scrolling to a section) counts; moving focus alone does not: people do not see it.
      const did = after.mut > 0 || after.opened > 0 || after.dialog || after.scrolled || p.url() !== before.url || requests > before.req;
      if (!did) dead.push(`${view}  "${k.words}"  (${k.sig})`);
      await p.keyboard.press('Escape').catch(() => {});
    }
  }
  await browser.close();
  console.log(`${views.length} views, ${clicked} controls clicked, ${skipped} skipped (lasting effects)`);
  for (const d of dead) console.log('DEAD ' + d);
  for (const e of pageErrors) console.log('page error: ' + e);
  if (dead.length || pageErrors.length) { console.log(`${dead.length} dead controls, ${pageErrors.length} page errors`); process.exit(1); }
  console.log('every control does something');
})();
