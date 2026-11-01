// Copy the library to the homeserver, the page's side (TASK-copy-library-folders): the dialog's
// folder tree (lazy levels, paging, boxes that can be partly ticked, keys), the selection's
// totals and a full homeserver, the job chip from polling and from the node's `jobs` events,
// Pause / Cancel / Retry failed / Dismiss, a reload finding the job, slow answers, and a
// 20 000-file library that stays quick. Every answer about copying is mocked (the job is
// mn-infra's, against API-copy-library-2026-11-01.md); the node only serves the page.
//   node tools/ui-copy-library.js <api token> <node URL>
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || 'ci-token';
const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const KEY = 'o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy';
const problems = [];
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) problems.push(what); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (fn, ms = 8000) => { const end = Date.now() + ms; for (;;) { if (await fn()) return true; if (Date.now() > end) return false; await sleep(100); } };

// A fake library of 20 000 files: /music with 2 000 artist folders (each two albums of 5 songs),
// and a share whose homeserver list isn't known yet
const ARTISTS = 2000;
const artist = (i) => `Artist ${String(i).padStart(4, '0')}`;
const tree = (path) => {
  if (!path) return [{ path: '/music', name: '/music', files: ARTISTS * 10, bytes: ARTISTS * 10 * 30e6, present: 1200, leaf: false }, { path: '/mnt/share', name: '/mnt/share', files: 40, bytes: 40 * 300e6, present_pending: true, leaf: false }];
  if (path === '/music') return Array.from({ length: ARTISTS }, (_, i) => ({ path: `/music/${artist(i + 1)}`, name: artist(i + 1), files: 10, bytes: 300e6, present: i % 3 ? 0 : 5, leaf: false }));
  if (path === '/mnt/share') return [{ path: '/mnt/share/Huge', name: 'Huge', files: 40, bytes: 40 * 300e6, present_pending: true, leaf: true }];
  return ['Album A', 'Album B'].map((a) => ({ path: `${path}/${a}`, name: a, files: 5, bytes: 150e6, present: 0, leaf: true }));
};
const filesOf = (paths) => paths.reduce((n, p) => n + (p === '/music' ? ARTISTS * 10 : p.startsWith('/mnt/share') ? 40 : /Album/.test(p) ? 5 : 10), 0);
const homeFiles = Array.from({ length: 20000 }, (_, i) => ({ path: `/priv/pubky.music/files/${i}.flac`, name: `${i}.flac`, size: 30e6, song: { id: i + 1, title: `Song ${i}`, artist: artist((i % ARTISTS) + 1), album: 'Album A' } }));

(async () => {
  const browser = await chromium.launch();
  try {
    for (const [w, h] of [[1280, 900], [390, 844]]) {
      const ctx = await browser.newContext({ viewport: { width: w, height: h }, reducedMotion: 'reduce' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);
      const log = []; const slow = { tree: 0, music: 0, dry: 0 }; let paged = true; const musicAsks = []; let jobs500 = 0;
      const job = { current: null, queued: [], last: null };
      let sock = null; let wsDown = false;
      await ctx.routeWebSocket(/\/v1\/events/, (ws) => { if (wsDown) { ws.close(); return; } ws.connectToServer(); sock = ws; });
      await ctx.route(/\/v1\/pubky$/, (r) => r.fulfill({ json: { user: 'pubky' + KEY, unsynced: 0, needs_resign: false, sync: null } }));
      await ctx.route(/\/v1\/storage$/, (r) => r.fulfill({ json: { homeserver: { mode: 'external', user: KEY, quota_mb: 'unlimited' }, saving: { state: 'saved', pending: 0 } } }));
      await ctx.route(/\/v1\/library\/tree(\?.*)?$/, async (r) => {
        const u = new URL(r.request().url()); const path = u.searchParams.get('path'); const after = u.searchParams.get('after'); const limit = Number(u.searchParams.get('limit') || 200);
        if (slow.tree) await sleep(slow.tree);
        const all = tree(path); let from = 0; if (after) from = all.findIndex((x) => x.name === after) + 1;
        const page = all.slice(from, from + limit);
        return r.fulfill({ json: { path, children: page, next: from + limit < all.length ? page[page.length - 1].name : null } });
      });
      await ctx.route(/\/v1\/pubky\/files\/upload$/, async (r) => {
        const b = r.request().postDataJSON(); log.push(b);
        if (b.dry_run && slow.dry) await sleep(slow.dry);
        const paths = b.paths || []; const files = b.all ? ARTISTS * 10 + 40 : filesOf(paths);
        const huge = paths.some((p) => p.startsWith('/mnt/share'));
        if (b.dry_run) return r.fulfill({ status: huge ? 507 : 200, json: huge ? { error: 'Not enough space on your homeserver: 12.0 GB needed, 3.0 GB free.', total: files, bytes_total: files * 300e6, needed_bytes: 12e9, free_bytes: 3e9 } : { total: files, bytes_total: files * 30e6, skipped: Math.floor(files / 4), needed_bytes: files * 22e6, free_bytes: 120e9 } });
        job.current = { id: 'j1', kind: 'copy', state: 'running', total: files, done: 0, failed: 0, skipped: 0, bytes_total: files * 30e6, bytes_done: 0, errors: [], errors_count: 0, current_folder: paths[0] || '/music', speed_bps: 2.4e6, eta_s: 600 };
        return r.fulfill({ status: 202, json: { id: 'j1', state: 'running' } });
      });
      await ctx.route(/\/v1\/pubky\/files\/jobs$/, (r) => (jobs500-- > 0 ? r.fulfill({ status: 500, json: { error: 'busy' } }) : r.fulfill({ json: job })));
      await ctx.route(/\/v1\/pubky\/files\/jobs\/last$/, (r) => { log.push({ dismiss: true }); job.last = null; return r.fulfill({ json: {} }); });
      await ctx.route(/\/v1\/pubky\/files\/(upload|jobs)\/j1\/(pause|resume|retry|cancel)$/, (r) => {
        const act = /\/(pause|resume|retry|cancel)$/.exec(r.request().url())[1]; log.push({ act });
        if (act === 'pause') job.current.state = 'paused'; if (act === 'resume') job.current.state = 'running';
        if (act === 'cancel') { job.last = { ...job.current, state: 'cancelled', finished: true }; job.current = null; }
        return r.fulfill({ json: {} });
      });
      await ctx.route(/\/v1\/me\/music(\?.*)?$/, async (r) => {
        const u = new URL(r.request().url()); const q = (u.searchParams.get('q') || '').toLowerCase(); musicAsks.push(u.search);
        if (slow.music) await sleep(slow.music);
        const all = q ? homeFiles.filter((f) => f.song.title.toLowerCase().includes(q)) : homeFiles;
        if (!paged) return r.fulfill({ json: { files: all, count: all.length, bytes: all.length * 30e6 } });
        const off = Number(u.searchParams.get('offset') || 0); const lim = Number(u.searchParams.get('limit') || 200);
        return r.fulfill({ json: { files: all.slice(off, off + lim), count: all.length, bytes: all.length * 30e6 } });
      });
      const p = await ctx.newPage();
      const errors = []; p.on('pageerror', (e) => errors.push(e.message));
      const W = `${w}:`;

      // ---- My music: the 20 000 files, paged, no empty flash ----
      slow.music = 2500;
      const t0 = Date.now();
      await p.goto(BASE + '/library');
      check(await p.waitForSelector('#mmfiles .skeleton', { timeout: 15000 }).then(() => true).catch(() => false), `${W} My music shows a skeleton while the files load`);
      check(!(await p.evaluate(() => /No music on your homeserver yet/.test(document.querySelector('#main').textContent))), `${W} never "No music" before the answer`);
      check(await until(async () => p.isVisible('#mmfiles .slow-note'), 3500), `${W} "Still loading…" after 2 s`);
      check(await p.waitForSelector('#mmlist .mrow', { timeout: 10000 }).then(() => true).catch(() => false), `${W} the files arrive (${Date.now() - t0} ms)`);
      slow.music = 0;
      check(await p.$$eval('#mmlist .mrow', (r) => r.length) === 200 && /19800 more/.test(await p.textContent('#mmlist [data-mmmore]')), `${W} 200 of 20 000, with Show more`);
      await p.click('#mmlist [data-mmmore]');
      check(await until(async () => (await p.$$eval('#mmlist .mrow', (r) => r.length)) === 400), `${W} Show more adds the next 200`);
      await p.fill('#mmq', 'Song 1999'); check(await until(async () => (await p.$$eval('#mmlist .mrow', (r) => r.length)) >= 1 && (await p.$$eval('#mmlist .mrow', (r) => r.length)) <= 11) && musicAsks.some((q) => /[?&]q=Song\+1999/.test(q)), `${W} the search goes to the node (q=…)`);
      // A node that sends every file at once: still 200 rows drawn
      paged = false; await p.reload(); await p.waitForSelector('#mmlist .mrow', { timeout: 15000 });
      check(await p.$$eval('#mmlist .mrow', (r) => r.length) === 200, `${W} a node that sends all 20 000 still draws 200`);
      paged = true;

      // ---- The dialog: slow tree, the levels, paging ----
      slow.tree = 2500;
      await p.click('#pkcopyall');
      await p.waitForSelector('.copydlg', { timeout: 5000 });
      check(await p.isVisible('.copydlg #ctree .skeleton') && await p.evaluate(() => document.activeElement && document.activeElement.dataset.cpick === 'folders'), `${W} the dialog opens at once on Folders, the tree loading`);
      check(await until(async () => p.isVisible('.copydlg .slow-note'), 3500), `${W} "Still loading…" for a slow tree`);
      check(await p.waitForSelector('.ctrow[data-cpath="/music"]', { timeout: 8000 }).then(() => true).catch(() => false), `${W} the roots arrive`);
      slow.tree = 0;
      check(/1200 already there/.test(await p.textContent('.ctrow[data-cpath="/music"]')) && !/already there/.test(await p.textContent('.ctrow[data-cpath="/mnt/share"]')), `${W} "already there" when known, nothing while the homeserver list is pending`);
      // Open /music: 200 of 2 000 artist folders, then Show more until all are there, each quick
      let t = Date.now(); await p.click('.ctrow[data-cpath="/music"] [data-ccaret]');
      await p.waitForSelector(`.ctrow[data-cpath="/music/${artist(200)}"]`, { timeout: 5000 });
      const firstLevel = Date.now() - t;
      let slowest = 0;
      for (let k = 1; k < 10; k++) { t = Date.now(); await p.click('[data-cmore="/music"]'); await p.waitForSelector(`.ctrow[data-cpath="/music/${artist(200 * (k + 1))}"]`, { timeout: 5000 }); slowest = Math.max(slowest, Date.now() - t); }
      check(await p.$$eval('.ctrow', (r) => r.length) === 2002 && !(await p.$('[data-cmore="/music"]')), `${W} 2 000 folders in one level, paged in`);
      check(firstLevel < 1000 && slowest < 1000, `${W} each page of the level within 1 s (first ${firstLevel} ms, slowest ${slowest} ms)`);
      // A tick with 2 000 rows: under 200 ms of work
      const tickMs = await p.evaluate(async (a) => { const r = document.querySelector(`.ctrow[data-cpath="/music/${a}"]`); const t1 = performance.now(); r.click(); await new Promise((ok) => requestAnimationFrame(() => ok())); return performance.now() - t1; }, artist(1));
      check(tickMs < 200, `${W} a tick among 2 000 rows takes ${Math.round(tickMs)} ms`);
      await p.click(`.ctrow[data-cpath="/music/${artist(2)}"]`);
      check(await p.getAttribute(`.ctrow[data-cpath="/music/${artist(1)}"]`, 'aria-checked') === 'true' && await p.getAttribute('.ctrow[data-cpath="/music"]', 'aria-checked') === 'mixed', `${W} two folders ticked; /music partly`);
      check(await until(async () => /20 files/.test(await p.textContent('#copytotals')) && /needs/.test(await p.textContent('#copytotals'))), `${W} the totals follow the selection (${(await p.textContent('#copytotals')).trim()})`);
      // Inside a ticked folder: unticking one album keeps the other
      await p.click(`.ctrow[data-cpath="/music/${artist(3)}"]`);
      await p.click(`.ctrow[data-cpath="/music/${artist(3)}"] [data-ccaret]`);
      await p.waitForSelector(`.ctrow[data-cpath="/music/${artist(3)}/Album A"]`, { timeout: 5000 });
      check(await p.getAttribute(`.ctrow[data-cpath="/music/${artist(3)}/Album A"]`, 'aria-checked') === 'true', `${W} a ticked folder's albums are ticked with it`);
      await p.click(`.ctrow[data-cpath="/music/${artist(3)}/Album A"]`);
      check(await until(async () => (await p.getAttribute(`.ctrow[data-cpath="/music/${artist(3)}"]`, 'aria-checked')) === 'mixed'), `${W} unticking an album leaves its folder partly ticked`);
      // Keys: Down, Space, Left
      await p.focus(`.ctrow[data-cpath="/music/${artist(3)}/Album B"]`);
      await p.keyboard.press('ArrowLeft');
      check(await p.evaluate((a) => document.activeElement.dataset.cpath === `/music/${a}`, artist(3)), `${W} Left goes to the folder`);
      // (its two albums are open under it, then the next folder)
      await p.keyboard.press('ArrowDown'); await p.keyboard.press('ArrowDown'); await p.keyboard.press('ArrowDown'); await p.keyboard.press('Space');
      check(await p.getAttribute(`.ctrow[data-cpath="/music/${artist(4)}"]`, 'aria-checked') === 'true', `${W} Down and Space tick a folder`);
      await p.keyboard.press('Space');
      check(await p.getAttribute(`.ctrow[data-cpath="/music/${artist(4)}"]`, 'aria-checked') === 'false', `${W} and Space unticks it`);
      // A full homeserver: Copy off, with the numbers
      await p.click('.ctrow[data-cpath="/mnt/share"]');
      check(await until(async () => /3.0 GB free/.test(await p.textContent('#copytotals'))) && await p.isDisabled('#copygo'), `${W} not enough room: Copy is off and says the numbers`);
      await p.click('.ctrow[data-cpath="/mnt/share"]');
      check(await until(async () => !(await p.isDisabled('#copygo'))), `${W} without it, Copy is on again`);
      // A slow count for one selection, then another selection: the old answer can't turn Copy on
      slow.dry = 1200;
      await p.click(`.ctrow[data-cpath="/music/${artist(5)}"]`); await sleep(600);
      // (the old answer lands about 1 s after the second click, the new one about 1.6 s after)
      await p.click(`.ctrow[data-cpath="/music/${artist(5)}"]`); await sleep(1300);
      check(await p.isDisabled('#copygo'), `${W} an answer for an older selection doesn't turn Copy on`);
      slow.dry = 0;
      check(await until(async () => !(await p.isDisabled('#copygo')), 5000), `${W} the current one does`);
      // The connection drops: the totals say so with Try again, Copy stays off, nothing hangs on "Counting…"
      wsDown = true; sock.close(); await sleep(1500);
      await p.click('[data-cpick="all"]');
      check(await until(async () => /Not connected/.test(await p.textContent('#copytotals')), 14000) && await p.isDisabled('#copygo') && await p.isVisible('#copyretot'), `${W} with the connection down the totals say so, Copy off`);
      wsDown = false;
      // ("Everything" doesn't fit this mocked homeserver, so Copy stays off: the count is what's back)
      check(await until(async () => { const b = await p.$('#copyretot'); if (b) await b.click().catch(() => {}); return /20\D?040 files/.test(await p.textContent('#copytotals')); }, 40000), `${W} back again, Try again counts`);
      await p.click('[data-cpick="folders"]'); await p.waitForSelector(`.ctrow[data-cpath="/music/${artist(1)}"]`, { timeout: 5000 });
      // Copy: exactly the ticked folders; the dialog closes at once
      log.length = 0; await p.click('#copygo');
      const sent = log.find((b) => !b.dry_run);
      const want = [`/music/${artist(1)}`, `/music/${artist(2)}`, `/music/${artist(3)}/Album B`].sort().join('|');
      check(!!sent && [...sent.paths].sort().join('|') === want, `${W} the job gets exactly the ticked folders (${sent && sent.paths.join(', ')})`);
      check(await until(async () => !(await p.$('.copydlg'))), `${W} the dialog closes at once`);

      // ---- The chip: polling, then events ----
      check(await until(async () => p.isVisible('#jobchip')), `${W} the chip shows the job`);
      job.current.done = 7; job.current.bytes_done = 210e6;
      check(await until(async () => /7\/25/.test(await p.textContent('#jobchip'))), `${W} it follows by polling while no event comes`);
      jobs500 = 1; job.current.done = 9; job.current.bytes_done = 270e6;
      check(await until(async () => /9\/25/.test(await p.textContent('#jobchip')), 12000), `${W} a failed look doesn't stop the polling`);
      sock.send(JSON.stringify({ type: 'jobs', current: { ...job.current, done: 12, bytes_done: 360e6 }, queued: [], last: null }));
      check(await until(async () => /12\/25/.test(await p.textContent('#jobchip')), 2000), `${W} a jobs event moves it at once`);
      job.current.done = 12; job.current.bytes_done = 360e6;
      await p.click('#jobchip');
      check(/2.4 MB\/s/.test(await p.textContent('#jobpop')) && /10 min left/.test(await p.textContent('#jobpop')) && /Artist 0001/.test(await p.textContent('#jobpop .jobcur')), `${W} speed, time left and the folder`);
      job.current.errors = [{ path: '/music/Artist 0002/x.flac', reason: 'cannot read' }]; job.current.errors_count = 1; job.current.failed = 1;
      sock.send(JSON.stringify({ type: 'jobs', ...job }));
      await p.waitForSelector('#jobpop [data-jerrs]', { timeout: 3000 });
      await p.click('#jobpop [data-jerrs]');
      check(/cannot read/.test(await p.textContent('#jobpop .joberr')), `${W} the errors list`);
      await p.click('#jobpop [data-jact="pause"]');
      check(await until(async () => log.some((l) => l.act === 'pause') && /Resume/.test(await p.textContent('#jobpop'))), `${W} Pause, then Resume`);
      await p.click('#jobpop [data-jact="resume"]');
      check(await until(async () => log.some((l) => l.act === 'resume')), `${W} Resume`);
      // A reload finds the job
      await p.reload(); check(await until(async () => p.isVisible('#jobchip'), 15000), `${W} after a reload the chip finds the job`);
      // Finished with failures: the summary, Retry failed, Dismiss
      job.last = { ...job.current, state: 'finished', finished: true, done: 23, failed: 2, failed_retryable: 2, skipped: 6, not_audio: 1 }; job.current = null;
      sock && sock.send(JSON.stringify({ type: 'jobs', ...job }));
      check(await until(async () => /failed/.test(await p.textContent('#jobchip')), 5000), `${W} the chip says how many failed`);
      await p.click('#jobchip');
      const sum = await p.textContent('#jobpop');
      check(/23 files copied/.test(sum) && /6 already there/.test(sum) && /1 not audio/.test(sum) && /2 failed/.test(sum), `${W} the summary (${sum.replace(/\s+/g, ' ').slice(0, 120)})`);
      await p.click('#jobpop [data-jact="retry"]');
      check(await until(async () => log.some((l) => l.act === 'retry')), `${W} Retry failed`);
      await p.click('#jobpop [data-jact="dismiss"]').catch(() => {});
      check(await until(async () => log.some((l) => l.dismiss)), `${W} Dismiss`);
      // Stopped before its last file (#224's stopped_reason): never "Done", with the reason in the chip, card and summary
      const stops = [
        [{ stopped_reason: 'Not enough space on this machine: 2.0 GB stay free for the homeserver.' }, 'Stopped: full', /space on this machine/],
        [{ failed: 3, stopped_reason: '3 failures in a row: the homeserver answered 503.' }, 'Stopped: failures', /3 failures in a row/],
        [{ cancelled: true, state: 'cancelled' }, 'Cancelled', /Cancelled\./],
      ];
      for (const [extra, chipText, why] of stops) {
        job.last = { id: 'j' + chipText.length, kind: 'copy', state: 'finished', finished: true, total: 25, done: 9, failed: 0, skipped: 0, bytes_total: 750e6, bytes_done: 270e6, errors: [], errors_count: 0, ...extra };
        sock && sock.send(JSON.stringify({ type: 'jobs', ...job }));
        check(await until(async () => (await p.textContent('#jobchip')).includes(chipText), 5000) && await p.$eval('#jobchip', (c) => c.classList.contains('stopped') && !c.classList.contains('done')) && why.test(await p.getAttribute('#jobchip', 'aria-label')), `${W} the chip says "${chipText}", not Done, and why in its name`);
        await p.click('#jobchip');
        const card = await p.textContent('#jobpop');
        check(/Copy stopped/.test(card) && /Stopped after 9 of 25 files/.test(card) && why.test(card) && !/Copied to your homeserver/.test(card), `${W} the summary says it stopped and why (${card.replace(/\s+/g, ' ').slice(0, 140)})`);
        await p.keyboard.press('Escape'); await sleep(200);
      }
      job.last = null;
      // Downloads shows the job too
      job.current = { id: 'j1', kind: 'copy', state: 'running', total: 25, done: 3, failed: 0, skipped: 0, bytes_total: 750e6, bytes_done: 90e6, errors: [], errors_count: 0 };
      await p.goto(BASE + '/downloads');
      check(await until(async () => /Copying to your homeserver/.test(await p.textContent('#dljob'))), `${W} Downloads shows the job`);
      check(await p.locator('#dljob [data-jobclose]').count() === 0 && await p.locator('#dljob [data-jact="cancel"]').count() === 1, `${W} with Cancel, and no Hide that does nothing`);
      check(await p.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), `${W} no horizontal overflow`);
      check(errors.length === 0, `${W} no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
      await ctx.close();
    }
    // ---- A level whose second page fails: an untick inside it asks once, says so, keeps the tick ----
    {
      const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);
      await ctx.route(/\/v1\/pubky$/, (r) => r.fulfill({ json: { user: 'pubky' + KEY, unsynced: 0, needs_resign: false, sync: null } }));
      await ctx.route(/\/v1\/storage$/, (r) => r.fulfill({ json: { homeserver: { mode: 'external', user: KEY, quota_mb: 'unlimited' }, saving: { state: 'saved', pending: 0 } } }));
      await ctx.route(/\/v1\/me\/music(\?.*)?$/, (r) => r.fulfill({ json: { files: [], count: 0, bytes: 0 } }));
      await ctx.route(/\/v1\/pubky\/files\/upload$/, (r) => r.fulfill({ json: { total: 1, bytes_total: 1e6, skipped: 0 } }));
      let asks = 0;
      await ctx.route(/\/v1\/library\/tree(\?.*)?$/, async (r) => {
        const u = new URL(r.request().url()); const path = u.searchParams.get('path');
        if (!path) return r.fulfill({ json: { children: [{ path: '/big', name: '/big', files: 300, bytes: 3e9, leaf: false }, { path: '/slow', name: '/slow', files: 300, bytes: 3e9, leaf: false }], next: null } });
        if (path === '/slow') {
          if (u.searchParams.get('after')) { await sleep(1500); return r.fulfill({ json: { children: Array.from({ length: 100 }, (_, i) => ({ path: `/slow/f${200 + i}`, name: `f${200 + i}`, files: 1, bytes: 1e7, leaf: true })), next: null } }); }
          return r.fulfill({ json: { children: Array.from({ length: 200 }, (_, i) => ({ path: `/slow/f${i}`, name: `f${i}`, files: 1, bytes: 1e7, leaf: true })), next: 'f199' } });
        }
        asks++;
        if (u.searchParams.get('after')) return r.fulfill({ status: 500, json: { error: 'the share went away' } });
        return r.fulfill({ json: { children: Array.from({ length: 200 }, (_, i) => ({ path: `/big/f${i}`, name: `f${i}`, files: 1, bytes: 1e7, leaf: true })), next: 'f199' } });
      });
      const p = await ctx.newPage(); const errors = []; p.on('pageerror', (e) => errors.push(e.message));
      await p.goto(BASE + '/library'); await p.waitForSelector('#pkcopyall', { timeout: 15000 }); await p.click('#pkcopyall');
      await p.waitForSelector('.ctrow[data-cpath="/big"]'); await p.click('.ctrow[data-cpath="/big"]');
      await p.click('.ctrow[data-cpath="/big"] [data-ccaret]'); await p.waitForSelector('.ctrow[data-cpath="/big/f1"]');
      const before = asks; await p.click('.ctrow[data-cpath="/big/f1"]'); await sleep(1500);
      check(asks - before <= 1, `a failed page is asked for once, not again and again (${asks - before})`);
      check(await p.getAttribute('.ctrow[data-cpath="/big"]', 'aria-checked') === 'true' && await p.getAttribute('.ctrow[data-cpath="/big/f1"]', 'aria-checked') === 'true', 'the untick is not done: the folder stays ticked');
      check(/Couldn’t load every folder/.test(await p.textContent('#toast')), 'and it says why');
      // A level whose next page is still on its way (Show more pressed), then an untick inside it:
      // the untick waits for that page instead of spinning, and the tab stays responsive
      await p.click('.ctrow[data-cpath="/slow"]'); await p.click('.ctrow[data-cpath="/slow"] [data-ccaret]'); await p.waitForSelector('.ctrow[data-cpath="/slow/f1"]');
      await p.click('[data-cmore="/slow"]'); await p.click('.ctrow[data-cpath="/slow/f1"]');
      const alive = await Promise.race([p.evaluate(() => 1), sleep(4000).then(() => 0)]);
      check(alive === 1, 'an untick while that level loads leaves the tab responsive');
      check(await until(async () => (await p.getAttribute('.ctrow[data-cpath="/slow"]', 'aria-checked')) === 'mixed' && (await p.getAttribute('.ctrow[data-cpath="/slow/f1"]', 'aria-checked')) === 'false' && (await p.getAttribute('.ctrow[data-cpath="/slow/f250"]', 'aria-checked')) === 'true', 6000), 'and the untick is done once the page is in');
      // The album picker pages through a library with 2 500 albums (the node answers 1 000 at most)
      const albumAsks = [];
      await ctx.route(/\/v1\/library\/albums(\?.*)?$/, (r) => { const u = new URL(r.request().url()); albumAsks.push(u.search); const after = Number(u.searchParams.get('after') || 0); const lim = Math.min(1000, Number(u.searchParams.get('limit') || 100)); return r.fulfill({ json: Array.from({ length: Math.max(0, Math.min(lim, 2500 - after)) }, (_, i) => ({ id: after + i + 1, title: `Album ${after + i + 1}`, album_artist: 'Someone' })) }); });
      await p.click('[data-cpick="album"]'); await p.fill('#cpq', 'Album 2400');
      check(await until(async () => /Album 2400/.test(await p.textContent('#cplist'))) && albumAsks.length === 3 && albumAsks.every((q) => /limit=1000/.test(q)), `the album picker pages to the 2 500th album (${albumAsks.length} requests)`);
      check(errors.length === 0, `no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
      await ctx.close();
    }
    // ---- Today's node: no folder list, /jobs only lists running jobs, an unpaged file list ----
    {
      const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);
      await ctx.route(/\/v1\/pubky$/, (r) => r.fulfill({ json: { user: 'pubky' + KEY, unsynced: 0, needs_resign: false, sync: null } }));
      await ctx.route(/\/v1\/storage$/, (r) => r.fulfill({ json: { homeserver: { mode: 'external', user: KEY, quota_mb: 'unlimited' }, saving: { state: 'saved', pending: 0 } } }));
      let files = Array.from({ length: 150 }, (_, i) => ({ path: `/priv/pubky.music/files/${i}.flac`, name: `${i}.flac`, size: 30e6, song: { id: i + 1, title: `Song ${i}`, artist: 'Old Node', album: 'Album' } }));
      await ctx.route(/\/v1\/me\/music(\?.*)?$/, (r) => r.fulfill({ json: { files, count: files.length, bytes: files.length * 30e6 } }));
      await ctx.route(/\/v1\/pubky\/files$/, (r) => { if (r.request().method() !== 'DELETE') return r.continue(); const gone = r.request().postDataJSON().paths; files = files.filter((f) => !gone.includes(f.path)); return r.fulfill({ json: [] }); });
      let old = null; const sent = [];
      await ctx.route(/\/v1\/pubky\/files\/upload$/, (r) => { const b = r.request().postDataJSON(); sent.push(b); if (b.dry_run) return r.fulfill({ json: { total: 4, bytes_total: 1.2e8, skipped: 0 } }); old = { id: 'o1', kind: 'copy', total: 4, done: 0, failed: 0, skipped: 0, bytes_total: 1.2e8, bytes_done: 0, finished: false, errors: [] }; return r.fulfill({ json: old }); });
      await ctx.route(/\/v1\/pubky\/files\/jobs$/, (r) => r.fulfill({ json: old && !old.finished ? old : null }));
      await ctx.route(/\/v1\/pubky\/files\/jobs\/o1$/, (r) => r.fulfill({ json: old }));
      const p = await ctx.newPage(); const errors = []; p.on('pageerror', (e) => errors.push(e.message));
      await p.goto(BASE + '/library'); await p.waitForSelector('#mmlist .mrow', { timeout: 15000 });
      // The search works in the page when the node sent every file
      await p.fill('#mmq', 'Song 14');
      check(await until(async () => (await p.$$eval('#mmlist .mrow', (r) => r.length)) === 11), "today's node: the search filters in the page (150 files)");
      await p.fill('#mmq', '');
      await until(async () => (await p.$$eval('#mmlist .mrow', (r) => r.length)) === 150);
      // Delete while paging in the page: the file goes from the list
      await p.click('#mmlist [data-mmore="0"]'); await p.locator('.popover.menu:not([hidden]) .pop-item', { hasText: 'Delete from my homeserver' }).click();
      await p.locator('dialog .choice', { hasText: 'Delete' }).click();
      check(await until(async () => (await p.$$eval('#mmlist .mrow', (r) => r.length)) === 149 && !/^Song 0$/m.test(await p.textContent('#mmlist'))), "today's node: a deleted file leaves the list");
      // No folder list: Folders goes, Everything is chosen and has the focus
      await p.click('#pkcopyall'); await p.waitForSelector('.copydlg');
      check(await until(async () => p.evaluate(() => { const f = document.querySelector('[data-cpick="folders"]'); const on = document.querySelector('[data-cpick].on'); return f.hidden && on && on.dataset.cpick === 'all' && on.tabIndex === 0 && document.activeElement === on; })), "today's node: no Folders; Everything is chosen, reachable and focused");
      await until(async () => !(await p.isDisabled('#copygo')));
      await p.click('#copygo');
      check(sent.some((b) => b.all && !b.dry_run), "today's node: Copy everything");
      check(await until(async () => p.isVisible('#jobchip')), "today's node: the chip shows the job");
      // It finishes with failures; /jobs then forgets it: the result stays, from the job's own answer
      old = { ...old, done: 2, failed: 2, finished: true, errors: ['x.flac: cannot read', 'y.flac: refused'] };
      check(await until(async () => /2 failed/.test(await p.textContent('#jobchip')), 6000), "today's node: the finished job's result stays on the chip");
      await p.click('#jobchip');
      check(/cannot read/.test(await p.textContent('#jobpop')) || /2 errors/.test(await p.textContent('#jobpop')), "today's node: with its errors");
      check(!(await p.$('#jobpop [data-jact="pause"]')) && !(await p.$('#jobpop [data-jact="dismiss"]')), "today's node: no Pause or Dismiss it can't do");
      check(errors.length === 0, `today's node: no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
      await ctx.close();
    }
  } finally { await browser.close(); }
  if (problems.length) { console.log(`${problems.length} failed`); process.exit(1); }
  console.log('copy library ok');
})().catch((e) => { console.error(e); process.exit(1); });
