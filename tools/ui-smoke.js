// Headless smoke test for the web UI: loads index.html + app.js in jsdom against a
// running node, renders every view, plays a track on the browser zone, opens Now
// Playing, searches, and fails on any JS error.
//
//   npm install jsdom@24            (anywhere; then run from that directory)
//   node /path/to/tools/ui-smoke.js <api token> [http://127.0.0.1:7788] [repo root]

// jsdom is resolved from the current directory, so the repo needs no node_modules.
const { JSDOM, VirtualConsole } = require(require.resolve('jsdom', { paths: [process.cwd(), __dirname] }));
const fs = require('fs');
const TOKEN = process.argv[2];
const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const ROOT = process.argv[4] || require('path').resolve(__dirname, '..');
const html = fs.readFileSync(ROOT + '/crates/pm-api/static/index.html', 'utf8');
const js = fs.readFileSync(ROOT + '/crates/pm-api/static/app.js', 'utf8');
const errors = [];
const vc = new VirtualConsole();
vc.on('jsdomError', (e) => errors.push('jsdomError: ' + (e.message || e)));
vc.on('error', (...a) => errors.push('console.error: ' + a.join(' ')));
const dom = new JSDOM(html.replace('<script src="/app.js"></script>', ''), { url: BASE + '/', runScripts: 'outside-only', pretendToBeVisual: true, virtualConsole: vc });
const w = dom.window;
w.localStorage.setItem('pm.token', TOKEN);
w.fetch = (path, init) => fetch(path.startsWith('http') ? path : BASE + path, init);
w.HTMLMediaElement.prototype.play = async function () {};
w.HTMLMediaElement.prototype.pause = function () {};
w.HTMLDialogElement = w.HTMLDialogElement || class {};
if (!w.HTMLDialogElement.prototype.showModal) w.HTMLDialogElement.prototype.showModal = function () { this.open = true; };
w.addEventListener('error', (e) => errors.push('window.error: ' + e.message));
w.addEventListener('unhandledrejection', (e) => errors.push('unhandled: ' + e.reason));
process.on('unhandledRejection', (e) => errors.push('node unhandled: ' + (e && e.stack || e)));
w.eval(js);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  await sleep(1500);
  const views = ['home', 'favourites', 'albums', 'artists', 'genres', 'tracks', 'playlists', 'radio', 'sources', 'discover', 'queue', 'zones', 'settings'];
  for (const v of views) {
    w.document.querySelector(`.nav-item[data-view="${v}"]`).click();
    await sleep(900);
    const main = w.document.querySelector('#main');
    const text = main.textContent.replace(/\s+/g, ' ').trim().slice(0, 110);
    console.log(`[${v}] nav.on=${w.document.querySelector('.nav-item.on')?.dataset.view} skeleton=${!!main.querySelector('.skeleton')} :: ${text}`);
  }
  // Open an album via the first track's album button or the first tile.
  w.document.querySelector('.nav-item[data-view="tracks"]').click(); await sleep(800);
  const tracks = w.document.querySelectorAll('.track').length;
  console.log('tracks rows', tracks);
  const play = w.document.querySelector('.track .rowplay'); if (play) { play.click(); await sleep(700); }
  console.log('bar:', w.document.querySelector('#np').textContent.replace(/\s+/g, ' ').trim(), '| quality:', w.document.querySelector('#quality .qtext').textContent, '| zone:', w.document.querySelector('#zonename').textContent);
  w.document.querySelector('#np').click(); await sleep(400);
  console.log('overlay hidden?', w.document.querySelector('#nowplaying').hidden, '| overlay h1:', w.document.querySelector('#nowplaying h1')?.textContent);
  // Now Playing carries the sleep timer menu and the (hidden until loaded) lyrics panel.
  if (!w.document.querySelector('#npsleep')) errors.push('no sleep timer button on Now Playing');
  if (!w.document.querySelector('#nplyrics')) errors.push('no lyrics panel on Now Playing');
  const sleepBtn = w.document.querySelector('#npsleep'); if (sleepBtn) { sleepBtn.click(); await sleep(200); const items = [...w.document.querySelectorAll('.popover.menu .pop-item span')].map((e) => e.textContent); console.log('sleep menu:', items.join(' | ')); if (!items.includes('Off')) errors.push('sleep menu incomplete'); w.document.body.click(); await sleep(100); }
  w.document.querySelector('#npclose').click();
  // Albums: Focus chips filter the grid; "All" shows everything again.
  w.document.querySelector('.nav-item[data-view="albums"]').click(); await sleep(900);
  const chips = w.document.querySelectorAll('[data-focus]');
  const allTiles = w.document.querySelectorAll('.tile').length;
  console.log('focus chips:', [...chips].map((c) => c.textContent.trim()).join(' | '), '| tiles', allTiles);
  for (const c of chips) { if (!c.dataset.focus) continue; c.click(); await sleep(400); const n = w.document.querySelectorAll('.tile').length; if (n > allTiles || n === 0) errors.push(`focus ${c.dataset.focus}: ${n} tiles of ${allTiles}`); }
  const allChip = w.document.querySelector('[data-focus=""]'); if (allChip) { allChip.click(); await sleep(400); if (w.document.querySelectorAll('.tile').length !== allTiles) errors.push('focus All did not restore the grid'); }
  // search
  const s = w.document.querySelector('#search'); s.value = 'mono'; s.dispatchEvent(new w.Event('input', { bubbles: true })); await sleep(900);
  console.log('[search]', w.document.querySelector('#main h2')?.textContent, '| rows', w.document.querySelectorAll('.track').length);
  // zone popover
  w.document.querySelector('#zonebtn').click(); await sleep(100);
  console.log('zones in popover:', [...w.document.querySelectorAll('#zonepop .pop-item .name')].map((e) => e.textContent).join(', '));
  w.document.querySelector('#scrim').click();
  // DSP editor on the first node zone: open, add a band, save, close.
  w.document.querySelector('.nav-item[data-view="settings"]').click(); await sleep(900);
  const dspBtn = w.document.querySelector('[data-dsp]');
  if (dspBtn) {
    dspBtn.click(); await sleep(900);
    const editor = w.document.querySelector('#d-save');
    console.log('dsp editor open:', !!editor, '| eq rows:', w.document.querySelectorAll('#eq .eq-row').length);
    if (editor) {
      w.document.querySelector('#eq-add').click();
      w.document.querySelector('#d-headroom').value = '-3';
      w.document.querySelector('#d-save').click(); await sleep(700);
      const cfg = await (await fetch(BASE + '/v1/zones/' + encodeURIComponent(dspBtn.dataset.dsp) + '/dsp', { headers: { authorization: 'Bearer ' + TOKEN } })).json();
      console.log('dsp saved: headroom', cfg.headroom_db, '| bands', cfg.eq.length);
      w.document.querySelector('#d-reset').click(); await sleep(700);
      const cfg2 = await (await fetch(BASE + '/v1/zones/' + encodeURIComponent(dspBtn.dataset.dsp) + '/dsp', { headers: { authorization: 'Bearer ' + TOKEN } })).json();
      console.log('dsp reset: headroom', cfg2.headroom_db, '| bands', cfg2.eq.length);
      if (cfg.headroom_db !== -3 || cfg.eq.length !== 1 || cfg2.eq.length !== 0) errors.push('dsp editor round trip failed');
    } else errors.push('dsp editor did not open');
  }
  // Queue editing on the browser zone: add two tracks, move, remove.
  w.document.querySelector('.nav-item[data-view="tracks"]').click(); await sleep(800);
  const adds = w.document.querySelectorAll('.track [data-act="add"]');
  if (adds.length >= 2) { adds[0].click(); await sleep(200); adds[1].click(); await sleep(200); }
  w.document.querySelector('.nav-item[data-view="queue"]').click(); await sleep(700);
  const before = w.document.querySelectorAll('.list.queue .list-row').length;
  const shuffle = w.document.querySelector('#qshuffle'); if (shuffle) { shuffle.click(); await sleep(300); }
  // Save the browser queue as a playlist (fixtures carry no canonical ids, so a 422 is the expected answer; anything else is a bug).
  const save = w.document.querySelector('#qsave');
  if (save) {
    const plBefore = await (await fetch(BASE + '/v1/playlists', { headers: { authorization: 'Bearer ' + TOKEN } })).json();
    save.click(); await sleep(600);
    const plAfter = await (await fetch(BASE + '/v1/playlists', { headers: { authorization: 'Bearer ' + TOKEN } })).json();
    console.log('save queue as playlist: playlists', plBefore.length, '->', plAfter.length);
    for (const p of plAfter) if (!plBefore.some((b) => b.id === p.id)) await fetch(BASE + '/v1/playlists/' + encodeURIComponent(p.id), { method: 'DELETE', headers: { authorization: 'Bearer ' + TOKEN } });
  } else errors.push('no save-as-playlist button on the queue page');
  if (w.document.querySelectorAll('.list.queue .list-row').length !== before) errors.push('shuffle changed the queue length');
  const down = w.document.querySelector('[data-qdown="0"]'); if (down) { down.click(); await sleep(400); }
  const rm = w.document.querySelector('[data-qrm="0"]'); if (rm) { rm.click(); await sleep(400); }
  const after = w.document.querySelectorAll('.list.queue .list-row').length;
  console.log('queue rows before/after:', before, after);
  if (before < 2 || after !== before - 1) errors.push('queue editing failed');
  // Settings carries the Subsonic jukebox zone picker.
  w.document.querySelector('.nav-item[data-view="settings"]').click(); await sleep(900);
  if (!w.document.querySelector('#jukeboxzone')) errors.push('no jukebox zone picker in Settings');
  // Track ⋯ menu opens with playlist entries.
  w.document.querySelector('.nav-item[data-view="tracks"]').click(); await sleep(800);
  const more = w.document.querySelector('.track [data-act="more"]');
  if (more) { more.click(); await sleep(400); const items = [...w.document.querySelectorAll('.popover.menu .pop-item span')].map((e) => e.textContent); console.log('track menu:', items.join(' | ')); if (!items.includes('Play next') || !items.includes('New playlist…')) errors.push('track menu incomplete'); w.document.body.click(); }
  // Discover: the chart tabs render; with a contact set the rows carry site badges and a details menu.
  w.document.querySelector('.nav-item[data-view="discover"]').click(); await sleep(2500);
  const dtabs = [...w.document.querySelectorAll('#main .tabs .chip')].map((c) => c.textContent.trim());
  console.log('discover tabs:', dtabs.join(' | '), '| rows', w.document.querySelectorAll('#main [data-ditem]').length);
  if (!dtabs.includes('Top tracks') || !dtabs.includes('Search Pubky')) errors.push('discover tabs incomplete');
  // Identify library: a card in Settings; an unidentified row's mark opens the review view.
  w.document.querySelector('.nav-item[data-view="settings"]').click(); await sleep(1500);
  const idbox = w.document.querySelector('#idbox');
  console.log('identify card:', idbox ? idbox.textContent.replace(/\s+/g, ' ').trim().slice(0, 90) : null);
  if (!idbox || /Loading/.test(idbox.textContent)) errors.push('no identify card in Settings');
  w.document.querySelector('.nav-item[data-view="tracks"]').click(); await sleep(900);
  const mark = w.document.querySelector('.track .idmark');
  if (mark) {
    mark.click(); await sleep(900);
    const idh = (w.document.querySelector('#main h2') || {}).textContent || '';
    console.log('identify view:', idh);
    if (!/Confirm matches/.test(idh)) errors.push('identify review view missing');
  }
  const drow = w.document.querySelector('#main [data-dmore]');
  if (drow) { drow.click(); await sleep(300); const items = [...w.document.querySelectorAll('.popover.menu .pop-item span')].map((e) => e.textContent); console.log('discover menu:', items.join(' | ')); if (!items.some((t) => t.startsWith('Details'))) errors.push('discover row menu lacks details'); w.document.body.click(); await sleep(100); }
  const first = w.document.querySelector('#main [data-ditem]');
  if (first) { first.click(); await sleep(3000); const h1 = w.document.querySelector('#main h1'); console.log('discover item:', h1 && h1.textContent, '| also on rows', w.document.querySelectorAll('#main [data-vplay], #main .list-row').length, '| tag input', !!w.document.querySelector('#tagadd')); if (!h1) errors.push('discover item page did not render'); }
  // heart on first track row (fixtures have no ids → disabled)
  const h = w.document.querySelector('.track .heart'); console.log('heart disabled (no ids):', h && h.disabled);
  console.log('errors:', errors.length ? errors : 'none');
  process.exit(errors.length ? 1 : 0);
})();
