// No overlap, anywhere (STEWARD-UX-SETTINGS.md): every screen at 1440, 1024 and 390 px, light and
// dark, fails when
//   * the page scrolls sideways;
//   * an element's box spills out of its card, row or container (unless a box between them clips
//     it on purpose: overflow hidden/auto, ellipsis);
//   * two neighbours overlap (siblings in the same row or column; badges and overlays placed on
//     purpose with position absolute/fixed are left out).
// Screens: Home, the library pages, an album, a song, an artist, search, playlists, queue, zones,
// sources, radio, favourites, every Settings tab, every Sources tab, the now-playing overlay and a dialog.
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-overlap.js <api token> [http://127.0.0.1:7788]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || ''; const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const SETTINGS = ['library', 'playback', 'accounts', 'discover', 'backup', 'import', 'appearance', 'about'];

const inspect = () => {
  const out = { scroll: document.documentElement.scrollWidth - innerWidth, spill: [], overlap: [] };
  const name = (el) => `${el.tagName.toLowerCase()}${el.id ? '#' + el.id : ''}${typeof el.className === 'string' && el.className.trim() ? '.' + el.className.trim().split(/\s+/).slice(0, 2).join('.') : ''}`;
  const text = (el) => (el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 30);
  const shown = (el) => { const r = el.getBoundingClientRect(); if (r.width < 1 || r.height < 1) return false; const st = getComputedStyle(el); return st.visibility !== 'hidden' && st.display !== 'none' && Number(st.opacity) > 0.05 && !el.closest('[hidden],[aria-hidden="true"]'); };
  const placed = (el) => ['absolute', 'fixed', 'sticky'].includes(getComputedStyle(el).position);
  const clips = (el) => { const st = getComputedStyle(el); return /(hidden|auto|scroll|clip)/.test(st.overflowX + st.overflow) || st.textOverflow === 'ellipsis'; };
  const CONTAINERS = '.card, .set-rows, .set-row, .list-row, .track, .tile, .notice, .player, .chip, .btn, .dialog, .popover, .np-page, .set-nav, .toolbar, .section-head';
  const roots = [document.querySelector('#main'), document.querySelector('.player'), document.querySelector('#overlay:not([hidden])'), ...document.querySelectorAll('dialog[open], .popover')].filter(Boolean);
  const seen = new Set();
  for (const root of roots) {
    for (const el of root.querySelectorAll('*')) {
      if (seen.has(el) || !shown(el) || el.closest('svg') || placed(el)) continue;
      seen.add(el);
      const r = el.getBoundingClientRect();
      // 1. spill out of the nearest container (unless something between them clips)
      let c = el.parentElement; let clipped = false;
      while (c && c !== document.body) { if (clips(c)) { clipped = true; break; } if (c.matches(CONTAINERS)) break; c = c.parentElement; }
      if (!clipped && c && c !== document.body && c.matches(CONTAINERS)) {
        const cr = c.getBoundingClientRect();
        if (r.right > cr.right + 1.5 || r.left < cr.left - 1.5) out.spill.push(`${name(el)} “${text(el)}” spills ${Math.round(Math.max(r.right - cr.right, cr.left - r.left))} px out of ${name(c)}`);
      }
      // 2. neighbours that overlap
      const kids = [...el.children].filter((k) => shown(k) && !placed(k) && !k.closest('svg') && k.tagName !== 'BR');
      for (let i = 0; i < kids.length; i++) {
        const a = kids[i].getBoundingClientRect();
        for (let j = i + 1; j < kids.length; j++) {
          const b = kids[j].getBoundingClientRect();
          const w = Math.min(a.right, b.right) - Math.max(a.left, b.left); const h = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
          if (w > 2 && h > 2) out.overlap.push(`${name(kids[i])} “${text(kids[i])}” and ${name(kids[j])} “${text(kids[j])}” overlap (${Math.round(w)}×${Math.round(h)})`);
        }
      }
    }
  }
  out.spill = [...new Set(out.spill)].slice(0, 12); out.overlap = [...new Set(out.overlap)].slice(0, 12);
  return out;
};

(async () => {
  const api = async (path) => { const r = await fetch(BASE + '/v1' + path, { headers: TOKEN ? { authorization: 'Bearer ' + TOKEN } : {} }); return r.ok ? r.json() : null; };
  const albums = (await api('/library/albums?limit=1')) || []; const artists = (await api('/library/artists')) || []; const tracks = ((await api('/library/tracks?limit=1')) || { tracks: [] }).tracks;
  const screens = [['Home', '/'], ['Albums', '/albums'], ['Artists', '/artists'], ['Tracks', '/tracks'], ['Genres', '/genres'], ['Playlists', '/playlists'], ['Favourites', '/favourites'], ['Queue', '/queue'], ['Zones', '/zones'], ['Sources', '/sources'], ['Radio', '/radio'], ['Search', '/search?q=love&in=library'],
    ...(albums[0] ? [['Album', `/album/lib-${albums[0].id}`]] : []), ...(artists[0] ? [['Artist', `/artist/lib-${artists[0].id}`]] : []), ...(tracks[0] ? [['Song', `/song/lib-${tracks[0].id}`]] : []),
    ['Settings', '/settings'], ...SETTINGS.map((t) => [`Settings › ${t}`, `/settings/${t}`]),
    // Every Sources tab (STEWARD-UX-SOURCES.md)
    ...['library', ...((await api('/sources')) || []).map((x) => x.id)].map((id) => [`Sources › ${id}`, `/sources/${id}`])];
  const browser = await chromium.launch();
  const problems = [];
  for (const scheme of ['dark', 'light']) {
    for (const [w, h] of [[1440, 900], [1024, 768], [390, 844]]) {
      const ctx = await browser.newContext({ viewport: { width: w, height: h }, colorScheme: scheme, reducedMotion: 'reduce' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
      const p = await ctx.newPage();
      const check = async (label) => {
        const r = await p.evaluate(inspect); const at = `${label} ${w} ${scheme}`;
        if (r.scroll > 1) problems.push(`${at}: the page scrolls sideways by ${r.scroll} px`);
        for (const x of r.spill) problems.push(`${at}: ${x}`);
        for (const x of r.overlap) problems.push(`${at}: ${x}`);
      };
      for (const [label, path] of screens) {
        await p.goto(BASE + path); await p.waitForLoadState('networkidle', { timeout: 15000 }).catch(() => {}); await sleep(900);
        await check(label);
      }
      // The now-playing overlay and a dialog
      await p.goto(BASE + '/tracks'); await sleep(1200);
      const play = await p.$('.track [data-act="play"]'); if (play) { await p.hover('.track'); await play.click().catch(() => {}); await sleep(700); await p.keyboard.press('n'); await sleep(900); await check('Now playing'); await p.keyboard.press('Escape'); await sleep(300); }
      const more = await p.$('.track [data-act="more"]'); if (more) { await p.hover('.track'); await more.click().catch(() => {}); await sleep(500); await check('Track menu'); await p.keyboard.press('Escape'); }
      await ctx.close();
    }
  }
  await browser.close();
  for (const x of problems) console.log('OVERLAP ' + x);
  if (problems.length) { console.log(`${problems.length} problems`); process.exit(1); }
  console.log('no overlap, no spill, no sideways scroll');
})();
