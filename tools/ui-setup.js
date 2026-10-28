// Setting a node up in the web page, against a node with a config folder and a config token (CI's):
//   * Home's Get started shows the steps the node names (no music step: the config has a folder),
//     goes when hidden, stays hidden in a new browser (kept on the node), comes back from
//     Settings › Home for everyone, and goes by itself when only optional steps are left;
//   * Library folders marks the config's folder and offers no Remove for it;
//   * Group outputs offers every output, by kind, says what a pick becomes, and doesn't offer
//     outputs that share no sample rate with the ones picked (the API refuses those);
//   * About: "Use a different token in this browser", the node's token set in the config, or
//     replaced and shown once; the login dialog says where the token is.
// The rules behind the steps, the folders and the groups are pm-api's tests/node_setup.rs.
//   node tools/ui-setup.js <api token> [http://127.0.0.1:7788]
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || ''; const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const problems = [];
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) problems.push(what); };
const api = async (method, path, body) => { const r = await fetch(BASE + '/v1' + path, { method, headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined }); if (!r.ok) throw new Error(`${method} ${path}: ${r.status}`); return r.status === 204 ? null : r.json(); };
const visible = (p, sel, ms = 8000) => p.locator(sel).first().waitFor({ state: 'visible', timeout: ms }).then(() => true).catch(() => false);
const gone = (p, sel, ms = 8000) => p.locator(sel).first().waitFor({ state: 'detached', timeout: ms }).then(() => true).catch(() => false);
const rate = (r) => ({ pcm_rates: r, max_pcm_bits: 24, max_channels: 2, dsd: null, hw_volume: null, standby: false, source_switch: false, gapless: true, groupable: false, fixed_latency_ms: null, quality_ceiling: 'bit_perfect' });
const OUTS = [
  { id: 'upnp:RINCON_A', name: 'Sonos Kitchen', protocol: 'upnp', caps: rate([44100, 48000]), group_by: 'sonos' },
  { id: 'upnp:RINCON_B', name: 'Sonos Living', protocol: 'upnp', caps: rate([44100, 48000]), group_by: 'sonos' },
  { id: 'bluos:aa01', name: 'Bluesound Study', protocol: 'bluos', caps: rate([44100, 48000, 96000]), group_by: 'bluos' },
  { id: 'slim:00:04:20:00:00:01', name: 'Pi Bedroom', protocol: 'slim', caps: { ...rate([44100, 96000]), groupable: true } },
  { id: 'upnp:streamer', name: 'Streamer', protocol: 'upnp', caps: rate([44100, 48000, 192000]) },
  { id: 'airplay:phone', name: 'Old phone', protocol: 'airplay', caps: rate([8000]) },
];

(async () => {
  const browser = await chromium.launch();
  const ctx = async () => { const c = await browser.newContext({ viewport: { width: 1280, height: 900 } }); await c.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN); return c; };
  const before = await api('GET', '/node/settings');
  try {
    // ---- Get started ----
    let c = await ctx(); let p = await c.newPage();
    const real = await api('GET', '/me/first-steps');
    await p.goto(BASE + '/');
    if (real.show) {
      check(await visible(p, '#h-setup .setup-card'), 'Home shows Get started');
      const shown = await p.$$eval('#h-setup [data-step]', (els) => els.map((e) => e.dataset.step));
      check(JSON.stringify(shown) === JSON.stringify(real.steps.map((x) => x.id)), `the steps the node names (${shown.join(', ')})`);
      check(!shown.includes('music'), 'no "Add your music": the config has a folder');
      await p.click('#setuphide');
      check(await gone(p, '#h-setup .setup-card'), 'Hide these steps hides them');
      check((await api('GET', '/node/settings')).first_steps_hidden === true, 'kept on the node');
      await c.close(); c = await ctx(); p = await c.newPage();
      await p.goto(BASE + '/'); await p.waitForSelector('#h-setup', { state: 'attached' }); await p.waitForTimeout(1500);
      check(!(await p.$('#h-setup .setup-card')), 'still hidden in another browser');
      await p.goto(BASE + '/settings/nodehome');
      await p.check('#firststeps');
      await p.waitForTimeout(500);
      check((await api('GET', '/node/settings')).first_steps_hidden === false, 'Settings › Home for everyone shows them again');
      await p.goto(BASE + '/');
      check(await visible(p, '#h-setup .setup-card'), 'and Home has them again');
    } else check(false, `the CI node should have steps to do: ${JSON.stringify(real)}`);
    // Only optional steps left: no card (the node says show: false)
    await p.route(/\/v1\/me\/first-steps$/, (r) => r.fulfill({ json: { show: false, hidden: false, steps: [{ id: 'play', done: true, optional: false }, { id: 'contact', done: true, optional: false }, { id: 'pubky', done: false, optional: true }] } }));
    await p.goto(BASE + '/'); await p.waitForSelector('#h-setup', { state: 'attached' }); await p.waitForTimeout(1500);
    check(!(await p.$('#h-setup .setup-card')), 'no card once only optional steps are left');
    await p.unroute(/\/v1\/me\/first-steps$/);
    await c.close();

    // ---- Library folders ----
    c = await ctx(); p = await c.newPage();
    const ns = await api('GET', '/node/settings');
    await p.goto(BASE + '/settings/library');
    await p.waitForSelector('#folderadd');
    const rows = await p.$$eval('#main code.path', (els) => els.map((e) => { const r = e.closest('.set-row'); return { path: e.textContent, text: r.textContent, remove: !!r.querySelector('[data-folder-del]') }; }));
    for (const f of ns.config_folders) {
      const row = rows.find((r) => r.path === f);
      check(!!row && /From config/.test(row.text), `${f} is marked as from the config`);
      check(!!row && !row.remove, `${f} has no Remove`);
    }

    // ---- Group outputs ----
    await p.route(/\/v1\/outputs$/, (r) => r.fulfill({ json: OUTS }));
    let made = null;
    await p.route(/\/v1\/zones$/, (r) => { if (r.request().method() !== 'POST') return r.continue(); made = r.request().postDataJSON(); return r.fulfill({ json: { id: 'g', name: made.name, output: 'group:x', transport: 'stopped', sync: 'loose' } }); });
    await p.goto(BASE + '/settings/playback');
    await p.waitForSelector('#groupouts');
    const offered = await p.$$eval('#groupouts input', (els) => els.map((e) => e.value));
    check(OUTS.every((o) => offered.includes(o.id)), `every output is offered (${offered.length} of ${OUTS.length})`);
    const kinds = await p.$$eval('#groupouts .groupkind .small', (els) => els.map((e) => e.textContent));
    check(['Sonos', 'BluOS', 'Squeezelite'].every((k) => kinds.some((x) => x.startsWith(k))), `by kind (${kinds.join(' · ')})`);
    const box = (id) => p.locator(`#groupouts input[value="${id}"]`);
    await box('upnp:RINCON_A').check(); await box('upnp:RINCON_B').check();
    check(/Sonos groups them itself/.test(await p.textContent('#groupsync')), 'two Sonos: Sonos groups them');
    check(await box('airplay:phone').isDisabled(), 'an output with no sample rate in common is not offered');
    await box('upnp:RINCON_B').uncheck(); await box('bluos:aa01').check();
    check(/Loosely synced/.test(await p.textContent('#groupsync')), 'a mix: loosely synced, said so');
    await p.fill('#groupname', 'Downstairs'); await p.click('#groupmake');
    await p.waitForTimeout(500);
    check(made && JSON.stringify(made.outputs) === JSON.stringify(['upnp:RINCON_A', 'bluos:aa01']), `the group the page asks for (${JSON.stringify(made)})`);
    await c.close();

    // ---- About: tokens ----
    c = await ctx(); p = await c.newPage();
    await p.goto(BASE + '/settings/about');
    await p.waitForSelector('#logout');
    check((await p.textContent('#logout')).trim() === 'Use a different token in this browser', 'the browser token button says what it does');
    check(!(await p.$('#tokennew')) && /Set in the node’s config/.test(await p.textContent('#main')), 'a config token: changed in the config, with the guide');
    await p.route(/\/v1\/node\/settings$/, async (r) => { if (r.request().method() !== 'GET') return r.continue(); const res = await r.fetch(); const j = await res.json(); return r.fulfill({ json: { ...j, token_replaceable: true } }); });
    let rotated = 0; await p.route(/\/v1\/node\/token$/, (r) => { rotated++; return r.fulfill({ json: { token: 'f'.repeat(64) } }); });
    await p.goto(BASE + '/settings/about');
    await p.click('#tokennew');
    await p.click('dialog .choice');
    check(await visible(p, '#tokenout code'), 'the new token is shown');
    check(rotated === 1 && (await p.textContent('#tokenout code')) === 'f'.repeat(64), 'once, as the node made it');
    check(await p.evaluate(() => localStorage.getItem('pm.token')) === 'f'.repeat(64), 'this browser uses it');
    await c.close();

    // ---- The login dialog ----
    c = await browser.newContext(); p = await c.newPage();
    await p.goto(BASE + '/');
    await p.waitForSelector('#login[open]');
    const hint = await p.textContent('#login details');
    check(/api-token/.test(hint) && /PM_TOKEN/.test(hint) && /Umbrel/.test(hint), 'the login dialog says where the token is');
    check(await p.locator('#login a[href*="troubleshooting.md"]').count() === 1, 'and links the guide');
    await c.close();
  } finally {
    await api('PUT', '/node/settings', { first_steps_hidden: !!before.first_steps_hidden }).catch(() => {});
    await browser.close();
  }
  if (problems.length) { console.log(`${problems.length} failed`); process.exit(1); }
  console.log('setup ok');
})().catch((e) => { console.error(e); process.exit(1); });
