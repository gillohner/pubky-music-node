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
  const views = ['home', 'favourites', 'albums', 'artists', 'tracks', 'playlists', 'discover', 'queue', 'settings'];
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
  w.document.querySelector('#npclose').click();
  // search
  const s = w.document.querySelector('#search'); s.value = 'mono'; s.dispatchEvent(new w.Event('input', { bubbles: true })); await sleep(900);
  console.log('[search]', w.document.querySelector('#main h2')?.textContent, '| rows', w.document.querySelectorAll('.track').length);
  // zone popover
  w.document.querySelector('#zonebtn').click(); await sleep(100);
  console.log('zones in popover:', [...w.document.querySelectorAll('#zonepop .pop-item .name')].map((e) => e.textContent).join(', '));
  w.document.querySelector('#scrim').click();
  // heart on first track row (fixtures have no ids → disabled)
  const h = w.document.querySelector('.track .heart'); console.log('heart disabled (no ids):', h && h.disabled);
  console.log('errors:', errors.length ? errors : 'none');
  process.exit(errors.length ? 1 : 0);
})();
