// First run from zero (steward next-3, item 2): a brand-new node (no folders, no contact, no
// Pubky identity) walked as a new user would, until music plays.
//   * Home shows "Get started" and no Pubky sections that only say a contact is missing;
//   * the music folder is suggested; one press adds it and the card counts the files read;
//   * "Shuffle my music" plays in this browser and the step is ticked;
//   * the contact step saves and Home's Pubky sections come back;
//   * "Sign in with Pubky Ring" opens the sign-in on Settings;
//   * at 375 px the card fits; "Hide" hides it for good.
// Needs a node started on an empty data dir, with HOME holding Music/ with some audio files
// (PM_MUSIC_DIR names that folder when the suggestion is missing):
//   npm install playwright@1.49.1 && npx playwright install chromium   (anywhere; run from there)
//   node tools/ui-first-run.js [http://127.0.0.1:7790]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
// Waits until fn(arg) is truthy in the page and returns its value. Playwright's waitForFunction rebuilds the
// predicate with new Function inside the page, which the node's Content-Security-Policy forbids, so this polls
// through evaluate instead.
const waitFor = async (page, fn, arg = null, timeout = 30000) => { const end = Date.now() + timeout; for (;;) { const v = await page.evaluate(fn, arg).catch(() => null); if (v) return v; if (Date.now() > end) throw new Error('timed out waiting in the page'); await new Promise((r) => setTimeout(r, 100)); } };
const BASE = process.argv[2] || 'http://127.0.0.1:7790';
const problems = [];
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) problems.push(what); };
const stepDone = (p, n) => p.evaluate((i) => { const s = document.querySelectorAll('#h-setup .setup-step')[i]; return !!(s && s.classList.contains('done')); }, n - 1);

(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
  const p = await ctx.newPage();
  const errors = [];
  p.on('pageerror', (e) => errors.push(e.message));
  p.on('console', (m) => { if (m.type() === 'error' && !/Failed to load resource/.test(m.text())) errors.push(m.text()); });

  await p.goto(BASE + '/');
  const card = await p.waitForSelector('#h-setup .setup-card', { timeout: 20000 }).catch(() => null);
  check(!!card, 'Home shows "Get started" on a new install');
  if (!card) { await browser.close(); process.exit(1); }
  check(/0 of 4 done/.test(await p.textContent('#h-setup')), 'nothing is ticked yet');
  await p.waitForTimeout(2500);
  const main = await p.textContent('main');
  check(!/upstream contact/i.test(main), 'Home does not ask for an "upstream contact"');
  check((main.match(/Set the .*contact/gi) || []).length === 0, 'no section repeats that a contact is missing');

  // 1. Add the music: the suggested folder, one press
  const sug = await p.$('[data-addfolder]');
  check(!!sug, 'the Music folder is suggested');
  if (sug) await sug.click();
  else { await p.fill('#setupfolder', process.env.PM_MUSIC_DIR || process.env.HOME + '/Music'); await p.click('#setupadd'); }
  await waitFor(p, () => { const s = document.querySelector('#h-setup .setup-step'); return s && s.classList.contains('done'); }, null, 60000).catch(() => null);
  check(await stepDone(p, 1), 'step 1 is ticked once the files are read');
  check(/\d+ tracks?/.test(await p.textContent('#h-setup')), 'the card says how many tracks came in');

  // 2. Play: in this browser
  await p.click('#setupplay');
  const playing = await waitFor(p, () => { const a = document.querySelector('audio'); return a && a.src && !a.paused && a.currentTime > 0; }, null, 20000).then(() => true).catch(() => false);
  check(playing, 'music plays in this browser');
  await waitFor(p, () => { const s = document.querySelectorAll('#h-setup .setup-step')[1]; return s && s.classList.contains('done'); }, null, 15000).catch(() => null);
  check(await stepDone(p, 2), 'step 2 is ticked after playing');

  // 3. Contact: saved from the card; Pubky's sections come back
  await p.fill('#setupcontact', 'https://example.org/pm-first-run-check');
  await p.click('#setupcontactsave');
  await p.waitForSelector('#h-trending-songs', { timeout: 15000 }).catch(() => null);
  check(!!(await p.$('#h-trending-songs')), 'Pubky sections show once the contact is saved');
  await waitFor(p, () => { const s = document.querySelectorAll('#h-setup .setup-step')[2]; return s && s.classList.contains('done'); }, null, 15000).catch(() => null);
  check(await stepDone(p, 3), 'step 3 is ticked');

  // 4. Sign in: opens the Pubky Ring sign-in on Settings
  await p.click('#setupsignin');
  await waitFor(p, () => location.pathname.startsWith('/settings') && document.querySelector('#pkurl') && document.querySelector('#pkurl').textContent.trim().length > 0, null, 20000).catch(() => null);
  check(new URL(p.url()).pathname.startsWith('/settings'), 'sign-in goes to Settings');
  check(!!(await p.evaluate(() => { const u = document.querySelector('#pkurl'); return u && /Pubky Ring/.test(u.textContent); })), 'the Pubky Ring sign-in (QR and link) is open');
  check(new URL(p.url()).pathname === '/settings/accounts' && /Music folders/.test(await p.textContent('#set-library')), 'sign-in opens the Accounts tab; the Library tab has the music folders');

  // Phone width: the card fits
  const phone = await browser.newContext({ viewport: { width: 375, height: 812 }, reducedMotion: 'reduce' });
  const q = await phone.newPage();
  await q.goto(BASE + '/'); await q.waitForSelector('#h-setup .setup-card', { timeout: 20000 }).catch(() => null);
  const overflow = await q.evaluate(() => document.documentElement.scrollWidth - innerWidth);
  check(overflow <= 1, `no sideways scroll at 375 px (${overflow} px)`);
  // Hide: gone now and after a reload
  await q.click('#setuphide'); await q.waitForTimeout(300);
  check(!(await q.$('#h-setup .setup-card')), '"Hide" hides the card');
  await q.reload(); await q.waitForTimeout(2500);
  check(!(await q.$('#h-setup .setup-card')), 'it stays hidden after a reload');
  await phone.close();

  check(errors.length === 0, `no script errors${errors.length ? ': ' + errors.slice(0, 3).join(' | ') : ''}`);
  await browser.close();
  if (problems.length) { console.log(`${problems.length} problems`); process.exit(1); }
  console.log('first run works from zero to music playing');
})();
