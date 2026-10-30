// Accessibility and small-screen checks for the main screens (steward next-2, item 2).
// Home, search, an item page, the library, the queue and Settings, at 1280 and 375 px, light and dark:
//   * names: every button has an accessible name (text, aria-label or title); every image has alt;
//   * keyboard: Tab reaches the main controls and each shows a visible focus ring;
//     Space and the arrow keys do not reach the player while typing in a field;
//   * live regions: toasts are announced (aria-live);
//   * contrast: visible text meets WCAG AA (4.5:1, 3:1 for large text) against what is behind it;
//   * 375 px: no horizontal scroll; the player's buttons are at least 40 px for a thumb.
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-a11y.js <api token> [http://127.0.0.1:7788]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2]; const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const problems = [];
const add = (where, what) => { problems.push(`${where}: ${what}`); };

// In the page: names, alt, contrast, overflow.
const inspect = () => {
  const out = { unnamed: [], noAlt: [], contrast: [], overflow: 0, smallPlayer: [] };
  const visible = (el) => { const r = el.getBoundingClientRect(); const st = getComputedStyle(el); return r.width > 0 && r.height > 0 && st.visibility !== 'hidden' && st.display !== 'none' && Number(st.opacity) > 0.05 && !el.closest('[hidden],[aria-hidden="true"]'); };
  const label = (el) => (el.getAttribute('aria-label') || el.getAttribute('title') || el.textContent || '').trim() || (el.getAttribute('aria-labelledby') ? 'labelled' : '');
  for (const b of document.querySelectorAll('button, [role=button], a[href], input:not([type=hidden]), select')) {
    if (!visible(b)) continue;
    const tag = b.tagName.toLowerCase();
    if (tag === 'input' || tag === 'select') {
      const id = b.id; const lab = (id && document.querySelector(`label[for="${id}"]`)) || b.closest('label');
      if (!b.getAttribute('aria-label') && !b.getAttribute('placeholder') && !b.getAttribute('title') && !lab && b.type !== 'file') out.unnamed.push(`${tag}#${id || b.className}`);
      continue;
    }
    if (!label(b)) out.unnamed.push(`${tag}.${String(b.className).split(' ')[0]}${b.id ? '#' + b.id : ''}`);
  }
  for (const img of document.querySelectorAll('img')) if (visible(img) && !img.hasAttribute('alt')) out.noAlt.push(img.getAttribute('src')?.slice(0, 60));
  const rgb = (s) => { const m = s.match(/rgba?\(([^)]+)\)/); if (!m) return null; const p = m[1].split(',').map((x) => parseFloat(x)); return { r: p[0], g: p[1], b: p[2], a: p.length > 3 ? p[3] : 1 }; };
  const lum = (c) => { const f = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }; return 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b); };
  const behind = (el) => { let bg = { r: 255, g: 255, b: 255, a: 0 }; const stack = []; for (let e = el; e; e = e.parentElement) { const st = getComputedStyle(e); if (st.backgroundImage && st.backgroundImage !== 'none') return null; const c = rgb(st.backgroundColor); if (c && c.a > 0) { stack.push(c); if (c.a >= 1) break; } } bg = stack.pop() || { r: 255, g: 255, b: 255, a: 1 }; while (stack.length) { const c = stack.pop(); bg = { r: c.r * c.a + bg.r * (1 - c.a), g: c.g * c.a + bg.g * (1 - c.a), b: c.b * c.a + bg.b * (1 - c.a), a: 1 }; } return bg; };
  const seen = new Set();
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    if (!n.textContent.trim()) continue;
    const el = n.parentElement; if (!el || seen.has(el) || !visible(el)) continue; seen.add(el);
    if (el.closest('button:disabled, [aria-disabled="true"], .tile .art, .skeleton, svg')) continue;
    const st = getComputedStyle(el); const fg = rgb(st.color); const bg = behind(el); if (!fg || !bg) continue;
    const fgc = { r: fg.r * fg.a + bg.r * (1 - fg.a), g: fg.g * fg.a + bg.g * (1 - fg.a), b: fg.b * fg.a + bg.b * (1 - fg.a) };
    const l1 = lum(fgc); const l2 = lum(bg); const ratio = (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
    const size = parseFloat(st.fontSize); const bold = Number(st.fontWeight) >= 700;
    const need = size >= 24 || (bold && size >= 18.66) ? 3 : 4.5;
    if (ratio < need) out.contrast.push(`${ratio.toFixed(2)} < ${need}: “${n.textContent.trim().slice(0, 30)}” (${el.className || el.tagName})`);
  }
  // Clickable rows the keyboard cannot reach
  out.unreachable = [...document.querySelectorAll('.clickable')].filter((el) => visible(el) && !['BUTTON', 'A'].includes(el.tagName) && el.tabIndex < 0).map((el) => el.className.split(' ').slice(0, 2).join('.'));
  out.overflow = document.documentElement.scrollWidth - innerWidth;
  for (const b of document.querySelectorAll('.player button, #toggle, #prev, #next')) { if (!visible(b)) continue; const r = b.getBoundingClientRect(); if (r.width < 40 || r.height < 40) out.smallPlayer.push(`${b.id || b.className}: ${Math.round(r.width)}×${Math.round(r.height)}`); if (r.bottom > innerHeight + 1 || r.right > innerWidth + 1) out.smallPlayer.push(`${b.id || b.className} is off the screen (bottom ${Math.round(r.bottom)} of ${innerHeight})`); }
  return out;
};

(async () => {
  const browser = await chromium.launch();
  const screens = [['Home', '/'], ['Search', '/search?q=queen'], ['Album', '/album/lib-1'], ['Song', '/song/mb-b1a9c0e9-d987-4042-ae91-78d6a3267d69'], ['Albums', '/albums'], ['Tracks', '/tracks'], ['Playlists', '/playlists'], ['Queue', '/queue'], ['Settings', '/settings'], ...['playback', 'accounts', 'apps', 'nodehome', 'discover', 'backup', 'import', 'appearance', 'about'].map((t) => [`Settings › ${t}`, `/settings/${t}`]),
    ...['library', 'tidal', 'qobuz', 'soundcloud', 'bandcamp', 'spotify', 'jamendo', 'subsonic', 'jellyfin', 'deezer', 'archive', 'audius', 'radio', 'youtube'].map((id) => [`Sources › ${id}`, `/sources/${id}`])];
  for (const scheme of ['dark', 'light']) {
    for (const width of [1280, 375]) {
      const ctx = await browser.newContext({ viewport: { width, height: width < 600 ? 812 : 900 }, colorScheme: scheme, reducedMotion: 'reduce' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
      const p = await ctx.newPage();
      for (const [name, path] of screens) {
        await p.goto(BASE + path); await p.waitForLoadState('networkidle', { timeout: 15000 }).catch(() => {}); await sleep(900);
        const r = await p.evaluate(inspect);
        const at = `${name} ${width} ${scheme}`;
        for (const u of new Set(r.unnamed)) add(at, `control without a name: ${u}`);
        for (const a of new Set(r.noAlt)) add(at, `image without alt: ${a}`);
        for (const u of new Set(r.unreachable)) add(at, `clickable but not reachable by keyboard: ${u}`);
        for (const c of [...new Set(r.contrast)].slice(0, 8)) add(at, `low contrast ${c}`);
        if (width === 375 && r.overflow > 1) add(at, `horizontal scroll by ${r.overflow} px`);
        if (width === 375) for (const s of new Set(r.smallPlayer)) add(at, `player button too small for a thumb: ${s}`);
      }
      if (width === 1280 && scheme === 'dark') {
        // Tab path: the first 25 stops show a focus ring
        await p.goto(BASE + '/'); await sleep(1200);
        let ringless = [];
        for (let i = 0; i < 25; i++) {
          await p.keyboard.press('Tab');
          const f = await p.evaluate(() => { const el = document.activeElement; if (!el || el === document.body) return null; const st = getComputedStyle(el); const wrap = el.closest('.search'); const ws = wrap ? getComputedStyle(wrap) : null; const ring = (st.outlineStyle !== 'none' && parseFloat(st.outlineWidth) > 0) || (st.boxShadow && st.boxShadow !== 'none') || (ws && ws.boxShadow && ws.boxShadow !== 'none'); return { ring, what: `${el.tagName.toLowerCase()}.${String(el.className).split(' ')[0]}${el.id ? '#' + el.id : ''}` }; });
          if (f && !f.ring) ringless.push(f.what);
        }
        for (const w of new Set(ringless)) add('Tab path', `no visible focus ring on ${w}`);
        // A clickable row opens with Enter (keyboard only)
        await p.goto(BASE + '/playlists'); await p.waitForSelector('[data-pl]', { timeout: 10000 }).catch(() => {});
        if (await p.$('[data-pl]')) {
          await p.focus('[data-pl]'); await p.keyboard.press('Enter'); await sleep(800);
          if (!new URL(p.url()).pathname.startsWith('/playlist/')) add('Keys', 'Enter on a playlist row did not open it');
        }
        // Typing in a field never reaches the player
        await p.goto(BASE + '/settings'); await sleep(800);
        const before = await p.evaluate(() => window.__pmKeys || 0);
        await p.evaluate(() => { window.__pmTransport = 0; const orig = window.fetch; window.fetch = (u, o) => { if (String(u).includes('/transport') || String(u).includes('/toggle')) window.__pmTransport++; return orig(u, o); }; });
        await p.focus('#search'); await p.keyboard.type(' a b ', { delay: 20 }); await p.keyboard.press('ArrowLeft'); await p.keyboard.press('ArrowRight');
        const audioPlays = await p.evaluate(() => window.__pmTransport);
        if (audioPlays > 0) add('Keys', `typing in the search field sent ${audioPlays} player command(s)`);
        void before;
        // Toasts are announced
        const live = await p.evaluate(() => { const t = document.querySelector('#toast, #toasts, .toasts, [data-toasts]'); return t ? t.getAttribute('aria-live') : 'missing'; });
        if (!live || live === 'missing' || live === 'off') add('Toasts', `the toast area is not a live region (${live})`);
      }
      await ctx.close();
    }
  }
  await browser.close();
  for (const pr of problems) console.log('A11Y ' + pr);
  if (problems.length) { console.log(`${problems.length} problems`); process.exit(1); }
  console.log('accessibility and 375 px checks pass');
})();
